//! Child-process environment policy for the `bash` native tool.
//!
//! The default is *deny*: a `bash` child starts from a minimal baseline
//! (`PATH`, `HOME`, locale, …) so inherited credentials never reach a
//! model-run shell. [`BashEnv`] holds the resolved policy (baseline +
//! opt-in extras) and the set of secret values to redact from captured
//! output, so a command may *use* an approved secret without its value
//! leaking into the transcript.

use tokio::process::Command;

use lofi_types::BashConfig;

/// The marker substituted in place of a redacted secret value.
pub(super) const REDACTED: &str = "[redacted]";

/// Names copied from the parent env into the stripped baseline. Anything not
/// listed here (and not in `pass_env`/`env_file`) is dropped when `strip_env`
/// is on. `LC_*` is matched by prefix.
const BASELINE_NAMES: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TERM", "TZ", "LANG", "TMPDIR",
];

/// Resolved environment policy for `bash` children.
///
/// `baseline` is the minimal env used when `strip_env` is on. `extras` are
/// opt-in entries from `pass_env` (resolved from the parent env) and
/// `env_file`, applied on top of the baseline (or the inherited env when not
/// stripping). `redact` holds the secret values to scrub from output,
/// longest-first so a secret that is a prefix of another is not half-replaced.
#[derive(Debug, Clone, Default)]
pub struct BashEnv {
    /// Start from `baseline` only (`env_clear` + baseline) when true.
    pub strip_env: bool,
    /// `(name, value)` baseline pairs captured from the parent env.
    pub baseline: Vec<(String, String)>,
    /// `(name, value)` opt-in pairs from `pass_env` + `env_file`.
    pub extras: Vec<(String, String)>,
    /// Secret values to redact from output, longest-first, non-empty, deduped.
    pub redact: Vec<String>,
}

impl BashEnv {
    /// Resolve a [`BashConfig`] into a concrete policy by reading the parent
    /// environment and (best-effort) the `env_file`. A missing or unreadable
    /// `env_file` is silently skipped so a transiently-absent secrets file
    /// does not break `bash` — only the listed extras are absent.
    #[must_use]
    pub fn from_config(cfg: &BashConfig) -> Self {
        let mut env = Self {
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

    /// Apply the policy to a `Command`: clear+baseline when stripping, then
    /// the opt-in extras (extras override baseline/inherited).
    pub(super) fn apply(&self, command: &mut Command) {
        if self.strip_env {
            command.env_clear();
            for (k, v) in &self.baseline {
                command.env(k, v);
            }
        }
        for (k, v) in &self.extras {
            command.env(k, v);
        }
    }

    /// Replace every redacted secret value in `s` with `[redacted]`.
    pub(super) fn redact(&self, s: &mut String) {
        for v in &self.redact {
            // `str::replace` allocates; redact sets are tiny.
            *s = s.replace(v, REDACTED);
        }
    }
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
        // A non-baseline var must not appear.
        assert!(!b.iter().any(|(k, _)| k == "LOFI_BASH_TEST_BASELINE"));
        std::env::remove_var("LOFI_BASH_TEST_BASELINE");
        // PATH is present on every sane host.
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

    #[test]
    fn redact_replaces_values_longest_first() {
        let env = BashEnv {
            strip_env: true,
            baseline: Vec::new(),
            extras: Vec::new(),
            redact: vec!["abcdef".to_string(), "abc".to_string()],
        };
        let mut s = "token=abcdef and abc".to_string();
        env.redact(&mut s);
        assert_eq!(s, "token=[redacted] and [redacted]");
    }

    /// End-to-end: `apply()` injects an approved secret into the child env so a
    /// command can use it, and `redact()` scrubs the value from the captured
    /// output so it never reaches the model. Runs a real `sh -c printenv`.
    #[tokio::test]
    async fn apply_injects_and_redact_scrubs_real_sh_output() {
        let secret = "shh-topsecret-value";
        let env = BashEnv {
            strip_env: true,
            baseline: vec![(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )],
            extras: vec![("LOFI_TEST_SECRET".to_string(), secret.to_string())],
            redact: vec![secret.to_string()],
        };
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printenv LOFI_TEST_SECRET");
        env.apply(&mut cmd);
        let out = cmd.output().await.unwrap();
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        // The secret was injected (apply works) ...
        assert_eq!(raw.trim(), secret);
        // ... but redaction removes it from what the model would see.
        let mut redacted = raw;
        env.redact(&mut redacted);
        assert_eq!(redacted.trim(), "[redacted]");
    }

    /// With `strip_env` on and a secret not in `pass_env`, it is absent from
    /// the child env entirely (the default deny posture).
    #[tokio::test]
    async fn strip_env_drops_unapproved_secret() {
        // Set a secret-like var in the test process; the child must not see it.
        std::env::set_var("LOFI_TEST_API_KEY", "leak-if-inherited");
        let env = BashEnv {
            strip_env: true,
            baseline: vec![(
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            )],
            extras: Vec::new(),
            redact: Vec::new(),
        };
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("printenv LOFI_TEST_API_KEY; true");
        env.apply(&mut cmd);
        let out = cmd.output().await.unwrap();
        std::env::remove_var("LOFI_TEST_API_KEY");
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(raw.trim().is_empty(), "unapproved secret leaked: {raw:?}");
    }
}
