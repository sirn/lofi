//! Resolution of [`BashConfig`] into a [`BashEnv`] for the code sandbox.
//!
//! The I/O — reading the parent environment and any configured env file —
//! lives here in the service layer. `lofi-code` receives the resolved
//! [`BashEnv`] as plain data so the sandbox crate stays free of config I/O.

use lofi_code::BashEnv;
use lofi_types::BashConfig;

/// Names copied from the parent env into the stripped baseline. Anything not
/// listed here (and not in `pass_env`/`env_file`) is dropped when `strip_env`
/// is on. `LC_*` is matched by prefix.
const BASELINE_NAMES: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "TZ", "LANG", "TMPDIR",
];

/// Resolve a [`BashConfig`] into a concrete [`BashEnv`] by reading the
/// parent environment and (best-effort) the `env_file`. A missing or
/// unreadable `env_file` is silently skipped so a transiently-absent secrets
/// file does not break `bash` — only the listed extras are absent.
#[must_use]
pub fn resolve_bash_env(cfg: &BashConfig) -> BashEnv {
    let mut env = BashEnv {
        strip_env: cfg.strip_env,
        baseline: Vec::new(),
        extras: Vec::new(),
        redact: Vec::new(),
    };
    if cfg.strip_env {
        env.baseline = baseline_from_parent();
    }
    // pass_env: copy from the parent env, recording values for redaction.
    for name in &cfg.pass_env {
        if let Ok(val) = std::env::var(name) {
            env.extras.push((name.clone(), val.clone()));
            env.redact.push(val);
        }
    }
    // env_file: override pass_env; redact its values too.
    if let Some(path) = &cfg.env_file {
        let expanded = expand_tilde(path);
        if let Ok(text) = std::fs::read_to_string(&expanded) {
            for (k, v) in parse_env_file(&text) {
                env.extras.push((k, v.clone()));
                env.redact.push(v);
            }
        }
    }
    env.redact.sort_by_key(|v| std::cmp::Reverse(v.len()));
    env.redact.dedup();
    env.redact.retain(|v| !v.is_empty());
    env
}

/// Collect the baseline `(name, value)` pairs present in the parent env.
fn baseline_from_parent() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (k, v) in std::env::vars() {
        if BASELINE_NAMES.contains(&k.as_str()) || k.starts_with("LC_") {
            out.push((k, v));
        }
    }
    out
}

/// Expand a leading `~` (alone or `~/...`) to `$HOME`. Other paths are
/// returned unchanged (relative paths resolve against the process CWD).
fn expand_tilde(p: &std::path::Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if s == "~" {
        return match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home),
            None => p.to_path_buf(),
        };
    }
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let mut joined = std::path::PathBuf::from(home);
            joined.push(rest);
            return joined;
        }
    }
    p.to_path_buf()
}

/// Parse `KEY=VALUE` lines from an env file. Blank lines and `#` comments are
/// skipped; a single surrounding layer of matching `"`/`'` quotes is stripped
/// from the value. Lines without `=` are skipped.
fn parse_env_file(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_string();
        if key.is_empty() {
            continue;
        }
        let mut val = v.trim().to_string();
        if val.len() >= 2 {
            let bytes = val.as_bytes();
            let first = bytes[0];
            let last = bytes[val.len() - 1];
            if (first == b'"' || first == b'\'') && first == last {
                val = val[1..val.len() - 1].to_string();
            }
        }
        out.push((key, val));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn baseline_includes_path_and_locale() {
        std::env::set_var("LOFI_BASH_TEST_BASELINE", "x");
        let b = baseline_from_parent();
        assert!(!b.iter().any(|(k, _)| k == "LOFI_BASH_TEST_BASELINE"));
        std::env::remove_var("LOFI_BASH_TEST_BASELINE");
        assert!(b.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn parse_env_file_skips_comments_and_quotes() {
        let text = "# comment\nFOO=bar\n\nEMPTY=\nQUOTED=\"a b\"\nNOSIGN\nKEY='v'\n";
        let pairs = parse_env_file(text);
        let map: std::collections::HashMap<&str, &str> = pairs
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        assert_eq!(map.get("FOO"), Some(&"bar"));
        assert_eq!(map.get("EMPTY"), Some(&""));
        assert_eq!(map.get("QUOTED"), Some(&"a b"));
        assert_eq!(map.get("KEY"), Some(&"v"));
        assert!(!map.contains_key("NOSIGN"));
        assert!(!map.contains_key("comment"));
    }
}