//! Agent-owned state tree.
//!
//! Resolves the one directory lofi is allowed to write to —
//! `$XDG_STATE_HOME/lofi/` (via [`dirs::state_dir`], falling back to
//! `~/.local/state/lofi/`) — and exposes the paths lofi cares about. This is
//! kept strictly separate from the read-only user config tree (see
//! [`crate::config_loader`]) so that a config manager like Nix can own the
//! config dir declaratively while lofi still has a writable home for its
//! discovery cache, future sessions, and logs.

use std::path::PathBuf;

use crate::error::{Error, Result};

/// Resolve the agent-owned state directory (`$XDG_STATE_HOME/lofi`).
///
/// Honors `$XDG_STATE_HOME` on Linux via [`dirs::state_dir`]; if that returns
/// `None`, fall back to `~/.local/state` before appending `lofi`. The
/// directory is *not* created here — call [`ensure_state_dir`] before writing.
///
/// # Errors
/// Returns [`Error::State`] only when no base state directory can be
/// determined (e.g. `HOME` is unset and `dirs::state_dir` returns `None`).
pub fn state_dir() -> Result<PathBuf> {
    let mut base = dirs::state_dir();
    if base.is_none() {
        if let Ok(home) = std::env::var("HOME") {
            base = Some(PathBuf::from(home).join(".local").join("state"));
        }
    }
    let mut path = base.ok_or_else(|| Error::State("could not resolve state dir".into()))?;
    path.push("lofi");
    Ok(path)
}

/// Path to the cached remote model-discovery file (`<state>/discovery.json`).
///
/// # Errors
/// Propagates [`state_dir`]'s error if the base directory cannot be resolved.
pub fn discovery_cache_path() -> Result<PathBuf> {
    let mut path = state_dir()?;
    path.push("discovery.json");
    Ok(path)
}

/// Ensure the state directory exists, creating it (and parents) if needed,
/// and return it.
///
/// # Errors
/// Returns [`Error::Io`] on filesystem failure, or [`Error::State`] if the
/// base directory cannot be resolved.
pub fn ensure_state_dir() -> Result<PathBuf> {
    let path = state_dir()?;
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
        // `dirs::state_dir` honors `XDG_STATE_HOME` on Linux; point it at a
        // temp dir so the test is hermetic and does not touch the real state
        // tree.
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
}
