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
    std::fs::create_dir_all(&path)?;
    Ok(path)
}

/// Create a fresh per-session tmp directory under `<state>/tmp/` and return
/// its path. Used as the backing store for bash full-output logs so the model
/// can page through truncated output via `lofi.bash_read`.
/// # Errors
/// Returns [`Error::Io`] on filesystem failure, or [`Error::State`] if the
/// base directory cannot be resolved.
pub fn create_session_tmp_dir() -> Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut path = ensure_state_dir()?;
    path.push("tmp");
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    path.push(format!("{nanos:016x}"));
    std::fs::create_dir_all(&path)?;
    Ok(path)
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
}
