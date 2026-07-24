//! Child-process environment policy for the `bash` native tool.
//!
//! The default is *deny*: a `bash` child starts from a minimal baseline
//! (`PATH`, `HOME`, locale, …) so inherited credentials never reach a
//! model-run shell. [`BashEnv`] holds the resolved policy (baseline +
//! opt-in extras) and the set of secret values to redact from captured
//! output, so a command may *use* an approved secret without its value
//! leaking into the transcript.

use tokio::process::Command;

/// The marker substituted in place of a redacted secret value.
pub(super) const REDACTED: &str = "[redacted]";

/// Resolved environment policy for `bash` children.
///
/// `baseline` is the minimal env used when `strip_env` is on. `extras` are
/// opt-in entries from `pass_env` (resolved from the parent env) and
/// `env_file`, applied on top of the baseline (or the inherited env when not
/// stripping). `redact` holds the secret values to scrub from output,
/// longest-first so a secret that is a prefix of another is not half-replaced.
///
/// Constructed by [`lofi_core::bash_env::resolve_bash_env`] in the service
/// layer; the sandbox receives it as plain data.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

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
