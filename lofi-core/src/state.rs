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

/// The workspace memory file `memory.md` in the workspace's directory
/// under `states/`, keyed like `sessions/` and `tmp/` so a project's
/// memory never crosses into another workspace. The agent maintains it
/// through the `lofi.memory` sandbox tool; the compaction hook reads it.
/// The directory is created here (mode 0700, like the rest of the state
/// tree) so a first write lands inside a private tree; the file itself
/// appears only when the agent writes it.
/// # Errors
/// Returns [`Error::State`] if the base state directory cannot be resolved,
/// or [`Error::Io`] on filesystem failure.
pub fn workspace_memory_path(cwd: &Path) -> Result<PathBuf> {
    let mut path = ensure_state_dir()?;
    path.push("states");
    path.push(workspace_key(cwd));
    ensure_private_dir(&path)?;
    path.push("memory.md");
    Ok(path)
}

/// The per-workspace state document: `state.json` in a directory under
/// `states/`, keyed like `sessions/` and `tmp/`, so a project's remembered
/// state never crosses into another workspace. The manager owns the document's format
/// and its read-modify-write cycle, so the rest of the code reaches state
/// through methods instead of reading the file itself.
pub struct WorkspaceState {
    path: PathBuf,
}

impl WorkspaceState {
    /// Open the state of the workspace rooted at `cwd`. Nothing is read or
    /// written yet: the file appears only when something is remembered.
    /// # Errors
    /// Returns [`Error::State`] if the base state directory cannot be
    /// resolved.
    pub fn open(cwd: &Path) -> Result<Self> {
        let mut path = state_dir()?;
        path.push("states");
        path.push(workspace_key(cwd));
        path.push("state.json");
        Ok(Self { path })
    }

    /// The remembered grants, longest-covering order preserved as stored.
    /// Reads fail toward "not approved": a missing, unreadable, or corrupt
    /// document only costs one more prompt.
    #[must_use]
    pub fn grants(&self) -> Vec<WorkspaceGrant> {
        std::fs::read(&self.path)
            .map(|bytes| parse_grants(&bytes))
            .unwrap_or_default()
    }

    /// The remembered grant that covers `dir` and allows `requested`.
    /// The longest match wins, so a nested grant names the narrowest root.
    #[must_use]
    pub fn grant_allowing(
        &self,
        dir: &std::path::Path,
        requested: lofi_types::Access,
    ) -> Option<WorkspaceGrant> {
        self.grants()
            .into_iter()
            .filter(|g| dir.starts_with(&g.dir) && g.access.allows(requested))
            .max_by_key(|g| g.dir.components().count())
    }

    /// The strongest remembered grant covering `dir`, whatever its level:
    /// what an upgrade prompt starts from.
    #[must_use]
    pub fn strongest_covering(&self, dir: &std::path::Path) -> Option<WorkspaceGrant> {
        self.grants()
            .into_iter()
            .filter(|g| dir.starts_with(&g.dir))
            .max_by_key(|g| (g.dir.components().count(), g.access))
    }

    /// Remember or update the grant for `dir`, so later sessions do not
    /// ask again.
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure, or [`Error::State`]
    /// if the document cannot be serialized.
    pub fn remember_grant(&self, dir: &std::path::Path, access: lofi_types::Access) -> Result<()> {
        self.update_grants(|grants| {
            if let Some(existing) = grants.iter_mut().find(|g| g.dir == dir) {
                existing.access = access;
            } else {
                grants.push(WorkspaceGrant {
                    dir: dir.to_path_buf(),
                    access,
                });
                grants.sort_by(|a, b| a.dir.cmp(&b.dir));
            }
        })
    }

    /// Forget the grant for exactly `dir`.
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure, or [`Error::State`]
    /// if the document cannot be serialized.
    pub fn forget_grant(&self, dir: &std::path::Path) -> Result<()> {
        self.update_grants(|grants| grants.retain(|g| g.dir != dir))
    }

    /// Read-modify-write under a lock: concurrent approvals touch the same
    /// file, so the existing list is re-read and merged inside the lock
    /// instead of trusting an earlier read.
    fn update_grants(&self, apply: impl FnOnce(&mut Vec<WorkspaceGrant>)) -> Result<()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A first grant has no file to merge into, but any other read failure
        // must abort the save: treating it as an empty list would overwrite
        // a good file and drop every remembered grant.
        let mut grants = match std::fs::read(&self.path) {
            Ok(bytes) => parse_grants(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        apply(&mut grants);
        self.write(&grants)
    }

    /// Replace the document atomically beside its destination, so a torn
    /// write cannot drop every remembered directory. The tmp name carries
    /// a timestamp so writers in other processes cannot clobber one tmp
    /// file before the rename.
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure, or [`Error::State`]
    /// if the state path has no parent.
    fn write(&self, grants: &[WorkspaceGrant]) -> Result<()> {
        let root = self
            .path
            .parent()
            .ok_or_else(|| Error::State("workspace state path has no parent".to_string()))?;
        ensure_private_dir(root)?;
        let value = serde_json::json!({ "version": 2, "grants": grants });
        let bytes = serde_json::to_vec(&value)
            .map_err(|e| Error::State(format!("serialize workspace state: {e}")))?;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_nanos());
        let tmp = root.join(format!(".{}-{nanos}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes)?;
        ensure_private_file(&tmp)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// One remembered grant as stored in the state document.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkspaceGrant {
    #[serde(rename = "path")]
    pub dir: PathBuf,
    pub access: lofi_types::Access,
}

/// The parsed list, or empty on an unreadable or unknown-version document:
/// a bad file is treated as no grants rather than aborting the caller.
/// Version 1 carried bare directories with read-and-write meaning, so its
/// entries load as write grants; the file itself is only rewritten on the
/// next change.
fn parse_grants(bytes: &[u8]) -> Vec<WorkspaceGrant> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return Vec::new();
    };
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(1) => value
            .get("dirs")
            .and_then(serde_json::Value::as_array)
            .map(|dirs| {
                dirs.iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(|dir| WorkspaceGrant {
                        dir: PathBuf::from(dir),
                        access: lofi_types::Access::Write,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        Some(2) => value
            .get("grants")
            .and_then(serde_json::Value::as_array)
            .map(|grants| {
                grants
                    .iter()
                    .filter_map(|g| {
                        let dir = g.get("path")?.as_str()?.to_string();
                        let access = g
                            .get("access")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|a| match a {
                                "read" => Some(lofi_types::Access::Read),
                                "write" => Some(lofi_types::Access::Write),
                                _ => None,
                            })?;
                        Some(WorkspaceGrant {
                            dir: PathBuf::from(dir),
                            access,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
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

/// `tmp/` holds one workspace-key dir per project, each containing leased
/// session dirs; leased dirs never sit directly at the root, so this recurses
/// exactly one level. A workspace-key dir has no `.lease` of its own — it is
/// swept into and dropped once empty.
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
        // Workspace-key dir: holds leased session dirs, not a `.lease` of its
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
    fn workspace_memory_path_is_workspace_scoped() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("lofi_state_test_memory_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LOFI_STATE_HOME", &tmp);

        let path = workspace_memory_path(Path::new("/ws/proj")).unwrap();
        assert!(path.ends_with("memory.md"));
        assert!(path.starts_with(&tmp));
        let key = path
            .parent()
            .and_then(std::path::Path::file_name)
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(key, workspace_key(Path::new("/ws/proj")));
        // Another workspace gets its own memory file.
        assert_ne!(workspace_memory_path(Path::new("/ws/other")).unwrap(), path);

        std::env::remove_var("LOFI_STATE_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
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

    #[test]
    fn grants_remember_across_restarts_and_stay_workspace_scoped() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp =
            std::env::temp_dir().join(format!("lofi_state_test_approved_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LOFI_STATE_HOME", &tmp);

        let ws = Path::new("/ws/proj");
        let state = WorkspaceState::open(ws).unwrap();
        let read = lofi_types::Access::Read;
        let write = lofi_types::Access::Write;
        assert_eq!(state.grant_allowing(Path::new("/data/dir"), read), None);

        state.remember_grant(Path::new("/data/dir"), write).unwrap();

        // Every lookup re-reads the file, so these cover a restart.
        assert_eq!(
            state.grant_allowing(Path::new("/data/dir"), write),
            Some(WorkspaceGrant {
                dir: PathBuf::from("/data/dir"),
                access: write
            })
        );
        assert_eq!(
            state.grant_allowing(Path::new("/data/dir/sub/x.txt"), read),
            Some(WorkspaceGrant {
                dir: PathBuf::from("/data/dir"),
                access: write
            })
        );
        // A write grant allows reads, but a read grant never allows writes.
        assert_eq!(
            state
                .grant_allowing(Path::new("/data/dir"), write)
                .unwrap()
                .access,
            write
        );
        state.remember_grant(Path::new("/ro"), read).unwrap();
        assert_eq!(state.grant_allowing(Path::new("/ro/sub"), write), None);
        assert_eq!(
            state
                .grant_allowing(Path::new("/ro/sub"), read)
                .unwrap()
                .access,
            read
        );

        // A nested grant wins over its parent, so the narrowest root names
        // the tree the tool result is shown against.
        state.remember_grant(Path::new("/data"), read).unwrap();
        assert_eq!(
            state.grant_allowing(Path::new("/data/dir/sub"), read),
            Some(WorkspaceGrant {
                dir: PathBuf::from("/data/dir"),
                access: write
            })
        );
        assert_eq!(
            state.grant_allowing(Path::new("/data/other"), read),
            Some(WorkspaceGrant {
                dir: PathBuf::from("/data"),
                access: read
            })
        );

        // A sibling of every grant stays unapproved, and another workspace
        // remembers nothing.
        assert_eq!(state.grant_allowing(Path::new("/elsewhere"), read), None);
        assert_eq!(
            WorkspaceState::open(Path::new("/ws/other"))
                .unwrap()
                .grant_allowing(Path::new("/data/dir"), read),
            None
        );

        // Forgetting removes exactly one grant; the nested one stays.
        state.forget_grant(Path::new("/data")).unwrap();
        assert_eq!(
            state
                .strongest_covering(Path::new("/data/dir/sub"))
                .unwrap()
                .dir,
            PathBuf::from("/data/dir")
        );

        std::env::remove_var("LOFI_STATE_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn version_one_dirs_load_as_write_grants() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "lofi_state_test_approved_v1_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LOFI_STATE_HOME", &tmp);

        let ws = Path::new("/ws/proj");
        let state = WorkspaceState::open(ws).unwrap();
        let path = state.path.clone();
        ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, br#"{"version":1,"dirs":["/data/dir"]}"#).unwrap();

        // The old format granted read and write together, so it loads as
        // write access without rewriting the file.
        assert_eq!(
            state.grant_allowing(Path::new("/data/dir"), lofi_types::Access::Write),
            Some(WorkspaceGrant {
                dir: PathBuf::from("/data/dir"),
                access: lofi_types::Access::Write
            })
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains(r#""version":1"#));

        std::env::remove_var("LOFI_STATE_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn approved_roots_survive_a_corrupt_file() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "lofi_state_test_approved_corrupt_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LOFI_STATE_HOME", &tmp);

        let ws = Path::new("/ws/proj");
        let state = WorkspaceState::open(ws).unwrap();
        let path = state.path.clone();
        ensure_private_dir(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();

        assert_eq!(
            state.grant_allowing(Path::new("/data/dir"), lofi_types::Access::Read),
            None
        );
        state
            .remember_grant(Path::new("/data/dir"), lofi_types::Access::Read)
            .unwrap();
        assert_eq!(
            state.grant_allowing(Path::new("/data/dir"), lofi_types::Access::Read),
            Some(WorkspaceGrant {
                dir: PathBuf::from("/data/dir"),
                access: lofi_types::Access::Read
            })
        );

        std::env::remove_var("LOFI_STATE_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn remember_aborts_when_the_existing_list_cannot_be_read() {
        let _env = super::STATE_ENV_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!(
            "lofi_state_test_approved_unreadable_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::env::set_var("LOFI_STATE_HOME", &tmp);

        // A directory at the file path makes the merge read fail with an
        // error other than NotFound.
        let ws = Path::new("/ws/proj");
        let state = WorkspaceState::open(ws).unwrap();
        let path = state.path.clone();
        ensure_private_dir(&path).unwrap();

        // The save must abort rather than merge into an empty list and
        // overwrite the file.
        assert!(state
            .remember_grant(Path::new("/data/dir"), lofi_types::Access::Read)
            .is_err());
        assert!(path.is_dir());

        std::env::remove_var("LOFI_STATE_HOME");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
