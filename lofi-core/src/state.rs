use std::path::PathBuf;

use lofi_error::{Error, Result};

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

fn collect_abandoned_tmp_dirs(root: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(root)? {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
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
                let stale = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age >= LEGACY_TMP_MAX_AGE);
                if stale {
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

/// Create a leased per-session directory for pageable tool output.
/// # Errors
/// Returns [`Error::Io`] on filesystem failure, or [`Error::State`] if the
/// base directory cannot be resolved.
pub(crate) fn create_session_tmp_dir() -> Result<SessionTempDir> {
    let root = ensure_state_dir()?.join("tmp");
    ensure_private_dir(&root)?;
    collect_abandoned_tmp_dirs(&root)?;
    create_leased_tmp_dir(&root).map_err(Error::Io)
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
}
