//! The default is *deny*: a `bash` child starts from a minimal baseline
//! (`PATH`, `HOME`, locale, …) so inherited credentials never reach a
//! model-run shell. [`BashEnv`] holds the resolved policy (baseline +
//! opt-in extras) and the set of secret values to redact from captured
//! output, so a command may *use* an approved secret without its value
//! leaking into the transcript.

use tokio::process::Command;

pub(super) const REDACTED: &str = "[redacted]";

#[derive(Debug, Clone, Default)]
pub struct BashEnv {
    pub strip_env: bool,
    pub baseline: Vec<(String, String)>,
    pub extras: Vec<(String, String)>,
    pub redact: Vec<String>,
}

impl BashEnv {
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

    pub(super) fn redact(&self, s: &mut String) {
        for v in &self.redact {
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
        assert_eq!(raw.trim(), secret);
        let mut redacted = raw;
        env.redact(&mut redacted);
        assert_eq!(redacted.trim(), "[redacted]");
    }

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
