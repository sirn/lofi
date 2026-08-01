use std::path::{Path, PathBuf};

use lofi_error::{Error, Result};

/// Stable per-workspace directory key shared by `sessions/` and `tmp/` so a
/// project's transcripts and scratch dirs land under the same label. Derived
/// from the workspace root path (not the basename alone, which would collide
/// across `~/work/a/b` and `~/other/a/b`) via a v5 (name-based, deterministic)
/// UUID — a v4 random id would change run-to-run and orphan prior state.
pub(crate) fn workspace_key(cwd: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let label = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                        ch
                    } else {
                        '-'
                    }
                })
                .take(40)
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, cwd.as_os_str().as_bytes());
    format!("{label}-{id}")
}

/// Tests that mutate the process-global `XDG_STATE_HOME` / `LOFI_STATE_HOME`
/// env vars acquire this lock for their whole duration. The vars are
/// process-global, so parallel test threads otherwise interleave their
/// `set_var`/`remove_var` and race on the assertions. Shared with the
/// `models` tests that also point `XDG_STATE_HOME` at a temp dir.
#[cfg(test)]
pub(crate) static STATE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Precedence: `$LOFI_STATE_HOME` (used as the base, with `lofi` appended),
/// then `$XDG_STATE_HOME` on Linux via [`dirs::state_dir`], then a
/// `~/.local/state` fallback. The directory is *not* created here — call
/// [`ensure_state_dir`] before writing.
/// # Errors
/// Returns [`Error::State`] only when no base state directory can be
/// determined (e.g. `HOME` is unset and `dirs::state_dir` returns `None`).
pub fn state_dir() -> Result<PathBuf> {
    let mut base = std::env::var_os("LOFI_STATE_HOME").map(PathBuf::from);
    if base.is_none() {
        base = dirs::state_dir();
    }
    if base.is_none() {
        if let Ok(home) = std::env::var("HOME") {
            base = Some(PathBuf::from(home).join(".local").join("state"));
        }
    }
    let mut path = base.ok_or_else(|| Error::State("could not resolve state dir".into()))?;
    path.push("lofi");
    Ok(path)
}

/// # Errors
/// Propagates [`state_dir`]'s error if the base directory cannot be resolved.
pub fn discovery_cache_path() -> Result<PathBuf> {
    let mut path = state_dir()?;
    path.push("discovery.json");
    Ok(path)
}

/// # Errors
/// Returns [`Error::Io`] on filesystem failure, or [`Error::State`] if the
/// base directory cannot be resolved.
pub fn ensure_state_dir() -> Result<PathBuf> {
    let path = state_dir()?;
    ensure_private_dir(&path)?;
    Ok(path)
}

pub(crate) fn ensure_private_dir(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub(crate) fn ensure_private_file(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

pub(crate) struct SessionTempDir {
    path: PathBuf,
    _lease: std::fs::File,
}

impl SessionTempDir {
    pub(crate) fn path(&self) -> &std::path::Path {
        &self.path
    }

    #[cfg(test)]
    fn create_under(root: &std::path::Path) -> Result<Self> {
        create_leased_tmp_dir(root).map_err(Error::Io)
    }
}

impl Drop for SessionTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const LEGACY_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_hours(24);

fn entry_is_stale(entry: &std::fs::DirEntry) -> bool {
    entry
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= LEGACY_TMP_MAX_AGE)
}

/// `tmp/` holds one workspace-key subdir per project, each containing leased
/// session dirs; recurse so stale dirs are swept at every depth (a workspace
/// dir whose leases all lapsed is emptied, then dropped if left empty).
fn collect_abandoned_tmp_dirs(root: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(".creating-")
        {
            if entry_is_stale(&entry) {
                let _ = std::fs::remove_dir_all(path);
            }
            continue;
        }
        // A workspace-key dir holds leased session dirs, not a `.lease` of its
        // own; sweep into it and drop it once empty.
        if !path.join(".lease").exists() {
            let _ = collect_abandoned_tmp_dirs(&path);
            let _ = std::fs::remove_dir(&path);
            continue;
        }
        let lease_path = path.join(".lease");
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(lease_path)
        {
            Ok(lease) if lease.try_lock().is_ok() => {
                let _ = std::fs::remove_dir_all(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if entry_is_stale(&entry) {
                    let _ = std::fs::remove_dir_all(path);
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    Ok(())
}

fn create_leased_tmp_dir(root: &std::path::Path) -> std::io::Result<SessionTempDir> {
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};

    loop {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let staging = root.join(format!(".creating-{id}"));
        let path = root.join(id);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&staging) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
        let lease = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(staging.join(".lease"))
        {
            Ok(lease) => lease,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error);
            }
        };
        if let Err(error) = lease.lock() {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&staging, &path) {
            let _ = std::fs::remove_dir_all(&staging);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                continue;
            }
            return Err(error);
        }
        return Ok(SessionTempDir {
            path,
            _lease: lease,
        });
    }
}

/// Create a leased per-session directory for pageable tool output, scoped to
/// the workspace so a project's scratch dirs group alongside its transcripts.
/// Orphaned dirs are collected from the whole `tmp/` tree (not just this
/// workspace), so a stale dir from any project is reclaimed at its owner's
/// next startup.
/// # Errors
/// Returns [`Error::Io`] on filesystem failure, or [`Error::State`] if the
/// base directory cannot be resolved.
pub(crate) fn create_session_tmp_dir(cwd: &Path) -> Result<SessionTempDir> {
    let root = ensure_state_dir()?.join("tmp");
    ensure_private_dir(&root)?;
    collect_abandoned_tmp_dirs(&root)?;
    let scoped = root.join(workspace_key(cwd));
    ensure_private_dir(&scoped)?;
    create_leased_tmp_dir(&scoped).map_err(Error::Io)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn paths_under_xdg_state_home() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join("lofi_state_test_paths");
        std::env::set_var("XDG_STATE_HOME", &tmp);

        let sd = state_dir().unwrap();
        assert!(sd.ends_with("lofi"));
        assert!(sd.starts_with(&tmp));

        let dc = discovery_cache_path().unwrap();
        assert!(dc.ends_with("lofi/discovery.json"));
        assert!(dc.starts_with(&tmp));

        std::env::remove_var("XDG_STATE_HOME");
    }

    #[test]
    fn lofi_state_home_overrides_xdg() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join("lofi_state_test_lofi_override");
        std::env::set_var("XDG_STATE_HOME", "/this/should/not/be/used");
        std::env::set_var("LOFI_STATE_HOME", &tmp);

        let sd = state_dir().unwrap();
        assert_eq!(sd, tmp.join("lofi"));

        let dc = discovery_cache_path().unwrap();
        assert_eq!(dc, tmp.join("lofi").join("discovery.json"));

        std::env::remove_var("LOFI_STATE_HOME");
        std::env::remove_var("XDG_STATE_HOME");
    }

    #[test]
    fn workspace_key_is_readable_and_collision_resistant() {
        let first = workspace_key(Path::new("/work/a-b/c"));
        let second = workspace_key(Path::new("/work/a/b-c"));
        assert!(first.starts_with("c-"));
        assert!(second.starts_with("b-c-"));
        assert_ne!(first, second);
        assert_eq!(workspace_key(Path::new("/work/a-b/c")), first);
    }

    #[test]
    fn session_tmp_dir_is_removed_after_last_lease() {
        let root = tempfile::tempdir().unwrap();
        let lease = SessionTempDir::create_under(root.path()).unwrap();
        let path = lease.path().to_path_buf();
        std::fs::write(path.join("output.log"), "secret").unwrap();
        let lease = std::sync::Arc::new(lease);
        let clone = lease.clone();

        drop(lease);
        assert!(path.exists());
        drop(clone);
        assert!(!path.exists());
    }

    #[test]
    fn startup_collects_unlocked_temp_dirs() {
        use std::os::unix::fs::OpenOptionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let abandoned = root.path().join("abandoned");
        ensure_private_dir(&abandoned).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(abandoned.join(".lease"))
            .unwrap();

        collect_abandoned_tmp_dirs(root.path()).unwrap();
        assert!(!abandoned.exists());
    }

    #[test]
    fn startup_preserves_locked_temp_dirs() {
        let root = tempfile::tempdir().unwrap();
        let lease = SessionTempDir::create_under(root.path()).unwrap();
        let path = lease.path().to_path_buf();

        collect_abandoned_tmp_dirs(root.path()).unwrap();
        assert!(path.exists());

        drop(lease);
        assert!(!path.exists());
    }

    #[test]
    fn collect_sweeps_workspace_scoped_abandoned_dirs() {
        let root = tempfile::tempdir().unwrap();
        let scoped = root.path().join(workspace_key(Path::new("/work/proj")));
        let abandoned = scoped.join("abandoned");
        ensure_private_dir(&abandoned).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(abandoned.join(".lease"))
            .unwrap();

        collect_abandoned_tmp_dirs(root.path()).unwrap();
        assert!(!scoped.exists());
    }

    #[test]
    fn startup_preserves_fresh_staging_dirs() {
        let root = tempfile::tempdir().unwrap();
        let staging = root.path().join(".creating-in-progress");
        ensure_private_dir(&staging).unwrap();

        collect_abandoned_tmp_dirs(root.path()).unwrap();
        assert!(staging.exists());
    }
}
