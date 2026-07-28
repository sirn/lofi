//! User-config loading and value resolution.
//!
//! Reads the user config file (default `$XDG_CONFIG_HOME/lofi/config.toml`,
//! overridable via `$LOFI_CONFIG`) and parses it into a
//! [`lofi_types::Config`], resolving `api_key` and `header` values along the
//! way. This tree is treated as **read-only**: lofi never creates, writes, or
//! modifies files under the user config dir. That separation lets an external
//! config manager (Nix, stow, etc.) own the tree declaratively while lofi
//! limits its writes to the agent-owned state dir (see [`crate::state`]).
//!
//! Value resolution syntax:
//! - `!cmd`  -> run `sh -c cmd` and take the trimmed stdout;
//! - `$VAR` / `${VAR}` -> read the env var (missing is an error);
//! - `$$` -> a literal `$`, `$!` -> a literal `!` (escapes so a value that
//!   would otherwise look like a command or env ref can be expressed);
//! - anything else is taken literally.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lofi_error::{Error, Result};
use lofi_types::Config;

/// Resolve the path to the user config file.
///
/// Precedence: `$LOFI_CONFIG` (used verbatim as the full file path), then the
/// platform default via [`dirs::config_dir`] (which honors `$XDG_CONFIG_HOME`
/// on Linux and falls back to `~/.config`) with `lofi/config.toml` appended.
/// If the platform's config dir cannot be resolved, fall back to `~/.config`.
///
/// # Errors
/// Returns [`Error::Config`] only when no config base directory can be
/// determined at all (e.g. `HOME` is unset) and `$LOFI_CONFIG` is not set.
pub fn user_config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("LOFI_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let mut base = dirs::config_dir();
    if base.is_none() {
        if let Ok(home) = std::env::var("HOME") {
            base = Some(PathBuf::from(home).join(".config"));
        }
    }
    let mut path = base.ok_or_else(|| Error::Config("could not resolve user config dir".into()))?;
    path.push("lofi");
    path.push("config.toml");
    Ok(path)
}

/// Resolve the path to the shell policy file.
///
/// Precedence: `$LOFI_POLICY` (used verbatim as the full file path), then
/// the same directory as the config file with `policy.toml` appended.
///
/// # Errors
/// Returns [`Error::Config`] only when no config base directory can be
/// determined and `$LOFI_POLICY` is not set.
pub fn policy_config_path(config_path: &Path) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("LOFI_POLICY") {
        return Ok(PathBuf::from(p));
    }
    let mut path = config_path
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| Error::Config("config path has no parent directory".into()))?;
    path.push("policy.toml");
    Ok(path)
}

/// Load the shell policy from `policy.toml` if it exists, otherwise return
/// the default. The file is parsed as [`lofi_types::ShellPolicyConfig`] at
/// root level (no `[shell_policy]` wrapper).
///
/// # Errors
/// [`Error::Config`] when the file exists but fails to read or parse.
pub fn load_policy_or_default(path: &Path) -> Result<lofi_types::ShellPolicyConfig> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| Error::Config(format!("failed to read {}: {e}", path.display())))?;
            toml::from_str(&content)
                .map_err(|e| Error::Config(format!("parse error in {}: {e}", path.display())))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(lofi_types::ShellPolicyConfig::default())
        }
        Err(e) => Err(Error::Config(format!(
            "policy path {}: {e}",
            path.display()
        ))),
    }
}

/// Load the configured shell policy and evaluate `command` without executing it.
///
/// # Errors
/// Returns configuration errors when the config or policy path cannot be
/// resolved, read, or parsed.
pub fn evaluate_shell_policy(command: &str) -> Result<lofi_code::policy::Decision> {
    let config_path = user_config_path()?;
    let policy_path = policy_config_path(&config_path)?;
    let config = load_policy_or_default(&policy_path)?;
    Ok(lofi_code::policy::defaults::resolve(&config).evaluate(command))
}

/// Resolve a single config value (see module docs for the syntax).
///
/// This is the low-level primitive used by [`load_config`]; callers may also
/// use it directly to resolve ad-hoc values. It is `async` because the `!cmd`
/// form spawns a subprocess via tokio.
///
/// # Errors
/// - [`Error::Config`] if an env var is missing or a shell command fails /
///   cannot be spawned.
pub async fn resolve_value(s: &str) -> Result<String> {
    if let Some(cmd) = s.strip_prefix('!') {
        run_shell(cmd).await
    } else if s == "$$" {
        Ok("$".to_string())
    } else if s == "$!" {
        Ok("!".to_string())
    } else if let Some(name) = parse_env_name(s) {
        std::env::var(name).map_err(|_| Error::Config(format!("env var not set: {name}")))
    } else {
        Ok(s.to_string())
    }
}

fn parse_env_name(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('$')?;
    if let Some(inner) = rest.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        // Reject empty `${}` and names with a closing brace in the middle.
        if inner.is_empty() || inner.contains('}') {
            return None;
        }
        return Some(inner);
    }
    // Bare `$VAR`: only accept if every char is a plausible env name char so
    // that strings like `pa$$word` stay literal rather than being misread as
    // env refs.
    if rest.is_empty() || !rest.chars().all(is_env_name_char) {
        return None;
    }
    Some(rest)
}

fn is_env_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Default timeout for a `!cmd` credential helper (30s). A hung helper
/// must not block startup indefinitely.
const CRED_CMD_TIMEOUT_MS: u64 = 30_000;
/// Cap captured credential-helper output so a misbehaving command can't
/// fill memory within the timeout window.
const CRED_CMD_MAX_BYTES: usize = 64 * 1024;

async fn run_shell(cmd: &str) -> Result<String> {
    let mut command = tokio::process::Command::new("sh");
    command
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| Error::Config(format!("failed to spawn shell command: {e}")))?;
    // Run in its own process group and guard it so a timeout or read failure
    // kills the *whole* tree (a helper that backgrounds a descendant can't
    // leave it running), not just the `sh` leader.
    let mut guard = lofi_code::tools::PgrpKillGuard::new(child.id());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::Config("shell command: stdout pipe unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::Config("shell command: stderr pipe unavailable".into()))?;
    // Read stdout/stderr with a bounded buffer concurrently so a misbehaving
    // helper can't exhaust memory during the timeout window; excess bytes are
    // drained and discarded rather than retained. The wait is inside the
    // timeout so a helper that closes its pipes early but keeps running can't
    // outlive the deadline.
    let cap = CRED_CMD_MAX_BYTES;
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(CRED_CMD_TIMEOUT_MS),
        Box::pin(async {
            let (out, err, status) = tokio::try_join!(
                lofi_code::tools::read_capped(&mut stdout, cap),
                lofi_code::tools::read_capped(&mut stderr, cap),
                child.wait(),
            )?;
            Ok::<_, std::io::Error>((status, out, err))
        }),
    )
    .await;
    match waited {
        Ok(Ok((status, (out_bytes, out_truncated), (err_bytes, err_truncated)))) => {
            guard.disarm();
            if !status.success() {
                let stderr = String::from_utf8_lossy(&err_bytes);
                let stderr = stderr.trim();
                let stderr = if err_truncated {
                    format!("{stderr}\n<output truncated>")
                } else {
                    stderr.to_string()
                };
                return Err(Error::Config(format!(
                    "shell command exited {status}: {stderr}"
                )));
            }
            let stdout = if out_truncated {
                // A truncated credential is guaranteed invalid; surfacing the
                // literal marker as a key would just produce a confusing 401.
                return Err(Error::Config(format!(
                    "credential command output exceeded {CRED_CMD_MAX_BYTES} bytes"
                )));
            } else {
                String::from_utf8_lossy(&out_bytes).trim().to_string()
            };
            Ok(stdout)
        }
        Ok(Err(e)) => {
            drop(guard);
            let _ = child.wait().await;
            Err(Error::Config(format!("shell command failed: {e}")))
        }
        Err(_) => {
            drop(guard);
            let _ = child.wait().await;
            Err(Error::Config(format!(
                "credential command timed out after {CRED_CMD_TIMEOUT_MS}ms: {cmd}"
            )))
        }
    }
}

/// Load and parse the user config from `path`, resolving `api_key` and header
/// values in place.
///
/// The returned [`Config`] has every `api_key` and header value replaced by
/// its resolved form; downstream code can treat them as literals.
///
/// # Errors
/// - [`Error::Config`] if the file cannot be read, fails to parse as TOML, or
///   any value fails to resolve.
pub async fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("failed to read {}: {e}", path.display())))?;
    let mut cfg: Config =
        toml::from_str(&content).map_err(|e| Error::Config(format!("parse error: {e}")))?;
    let policy_path = policy_config_path(path)?;
    cfg.shell_policy = load_policy_or_default(&policy_path)?;
    resolve_config(&mut cfg).await?;
    Ok(cfg)
}

/// Resolve every provider's credentials and headers inside `cfg` in place.
///
/// Resolution is **lenient** so a missing environment variable never aborts
/// startup: a provider whose key cannot be resolved is left keyless (and
/// therefore not available for selection). An explicit `api_key` (literal,
/// `$VAR`, or `!cmd`) overrides `env_name`; otherwise `env_name` names the
/// environment variable to read. Header values that fail to resolve are
/// dropped rather than fatal.
///
/// # Errors
/// Never errors in the current lenient implementation; the `Result` is kept
/// for signature stability and future strict modes.
pub async fn resolve_config(cfg: &mut Config) -> Result<()> {
    for provider in cfg.providers.values_mut() {
        if let Some(key) = provider.api_key.take() {
            provider.api_key = resolve_value(&key).await.ok().filter(|s| !s.is_empty());
        } else if let Some(name) = provider.env_name.as_ref() {
            provider.api_key = std::env::var(name).ok().filter(|s| !s.is_empty());
        }
        if let Some(headers) = provider.headers.take() {
            let mut resolved = HashMap::with_capacity(headers.len());
            for (name, raw) in headers {
                if let Ok(v) = resolve_value(&raw).await {
                    if !v.is_empty() {
                        resolved.insert(name, v);
                    }
                }
            }
            provider.headers = Some(resolved);
        }
    }
    Ok(())
}

/// The built-in default configuration, shipped as TOML so the default
/// provider/model set is expressed in the same format the user edits.
///
/// Defines `openai` (Responses API) and `anthropic` (Messages API) providers,
/// each keyed off an environment variable (`OPENAI_API_KEY` /
/// `ANTHROPIC_API_KEY`). Both are resolved *leniently* by [`resolve_config`]:
/// if the variable is unset the provider is left keyless and simply not
/// available, so a fresh checkout with no keys starts up and reports "no
/// models configured" rather than aborting.
const DEFAULT_CONFIG_TOML: &str = r#"
[agent]
thinking_level = "medium"

[providers.openai]
env_name = "OPENAI_API_KEY"
api_type = "openai-responses"

[providers.openai.models."gpt-5.6-luna"]
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.openai.models."gpt-5.6-terra"]
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.openai.models."gpt-5.6-sol"]
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.anthropic]
env_name = "ANTHROPIC_API_KEY"
api_type = "anthropic-messages"

[providers.anthropic.models."claude-fable-5"]
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.anthropic.models."claude-opus-4-8"]
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.anthropic.models."claude-sonnet-5"]
thinking_levels = ["low", "medium", "high", "xhigh"]
"#;

/// The built-in configuration used when no user config file exists.
///
/// Returns the parsed (but not yet resolved) [`Config`] from
/// [`DEFAULT_CONFIG_TOML`]; [`load_config_or_default`] resolves the env keys
/// on top. Parsing the embedded default is infallible — a panic here means
/// the shipped default TOML is malformed and is a bug, not a runtime
/// condition.
///
/// # Panics
/// The shipped [`DEFAULT_CONFIG_TOML`] is a compile-time constant; this
/// panics only if it is malformed, which is a bug, not a runtime condition.
#[must_use]
pub fn default_config() -> Config {
    toml::from_str(DEFAULT_CONFIG_TOML).unwrap_or_else(|e| {
        panic!("built-in default config must parse: {e}");
    })
}

/// Load the user config from `path`, or fall back to [`default_config`] when
/// the file does not exist.
///
/// An existing file is parsed and resolved (leniently — see
/// [`resolve_config`]); a missing file yields the built-in default config.
/// In both cases a provider whose key is unset is left keyless, so the call
/// itself never fails for a missing environment variable — that surfaces
/// downstream as "no models configured".
///
/// # Errors
/// [`Error::Config`] only when an existing file fails to read or parse as
/// TOML.
pub async fn load_config_or_default(path: &Path) -> Result<Config> {
    // Fall back to defaults only when the path is genuinely absent. A
    // permission error or dangling symlink must surface as an error, not
    // silently activate the built-in configuration.
    match std::fs::symlink_metadata(path) {
        Ok(_) => load_config(path).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut cfg = default_config();
            resolve_config(&mut cfg).await?;
            Ok(cfg)
        }
        Err(e) => Err(Error::Config(format!(
            "config path {}: {e}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::Api;

    /// Serializes tests that mutate process-global environment variables and
    /// restores the prior value on drop, so they neither race with each other
    /// nor leak into the caller's environment. The mutex guard is held until
    /// after the value is restored (custom `Drop` runs before field teardown).
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Acquire the serialization mutex once for tests that need to capture
    /// several env keys; pair with [`capture_env`] (which does not lock) so a
    /// multi-key test does not self-deadlock on the non-reentrant mutex.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTEX.lock().unwrap()
    }

    fn capture_env(key: &'static str) -> EnvRestore {
        EnvRestore {
            key,
            prev: std::env::var(key).ok(),
        }
    }

    struct EnvRestore {
        key: &'static str,
        prev: Option<String>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    async fn literal_value() {
        assert_eq!(resolve_value("hello world").await.unwrap(), "hello world");
        assert_eq!(resolve_value("").await.unwrap(), "");
    }

    #[tokio::test]
    async fn env_var_dollar() {
        std::env::set_var("LOFI_TEST_RESOLVE_BARE", "abc123");
        assert_eq!(
            resolve_value("$LOFI_TEST_RESOLVE_BARE").await.unwrap(),
            "abc123"
        );
    }

    #[tokio::test]
    async fn env_var_braces() {
        std::env::set_var("LOFI_TEST_RESOLVE_BRACED", "brval");
        assert_eq!(
            resolve_value("${LOFI_TEST_RESOLVE_BRACED}").await.unwrap(),
            "brval"
        );
    }

    #[tokio::test]
    async fn shell_command() {
        let v = resolve_value("!echo hello").await.unwrap();
        assert_eq!(v, "hello");
    }

    #[tokio::test]
    async fn shell_command_trims_output() {
        let v = resolve_value("!printf '  hi  \\n'").await.unwrap();
        assert_eq!(v, "hi");
    }

    #[tokio::test]
    async fn shell_command_failure_errors() {
        assert!(resolve_value("!false").await.is_err());
    }

    #[tokio::test]
    async fn escapes_dollar_and_bang() {
        assert_eq!(resolve_value("$$").await.unwrap(), "$");
        assert_eq!(resolve_value("$!").await.unwrap(), "!");
    }

    #[tokio::test]
    async fn literal_with_embedded_dollar_stays_literal() {
        assert_eq!(resolve_value("pa$$word").await.unwrap(), "pa$$word");
    }

    #[tokio::test]
    async fn missing_env_error() {
        let err = resolve_value("$LOFI_TEST_DEFINITELY_MISSING_XYZ").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn empty_braces_not_an_env_ref() {
        assert_eq!(resolve_value("${}").await.unwrap(), "${}");
    }

    #[test]
    fn default_config_has_openai_and_anthropic() {
        let cfg = default_config();
        assert_eq!(cfg.providers.len(), 2);
        assert!(cfg.providers.contains_key("openai"));
        assert!(cfg.providers.contains_key("anthropic"));
        let openai = cfg.providers.get("openai").unwrap();
        assert_eq!(openai.default_api(), Api::OpenAiResponses);
        assert_eq!(openai.env_name.as_deref(), Some("OPENAI_API_KEY"));
        assert!(openai.base_url.is_none());
        assert!(openai.api_key.is_none()); // unresolved until resolve_config
        let ids: Vec<&str> = openai.models.keys().map(String::as_str).collect();
        assert_eq!(ids, ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"]);
        let anthropic = cfg.providers.get("anthropic").unwrap();
        assert_eq!(anthropic.default_api(), Api::AnthropicMessages);
        assert_eq!(anthropic.env_name.as_deref(), Some("ANTHROPIC_API_KEY"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn resolve_config_lenient_missing_env_leaves_keyless() {
        let _g = env_lock();
        let _o = capture_env("OPENAI_API_KEY");
        let _a = capture_env("ANTHROPIC_API_KEY");
        let mut cfg = default_config();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        resolve_config(&mut cfg).await.unwrap();
        assert!(cfg.providers.get("openai").unwrap().api_key.is_none());
        assert!(cfg.providers.get("anthropic").unwrap().api_key.is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn load_config_or_default_detects_env_keys() {
        let _g = env_lock();
        let _o = capture_env("OPENAI_API_KEY");
        let _a = capture_env("ANTHROPIC_API_KEY");
        let dir = std::env::temp_dir().join("lofi-config-default-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("config.toml");

        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        let cfg = load_config_or_default(&missing).await.unwrap();
        assert!(cfg.providers.get("openai").unwrap().api_key.is_none());
        assert!(cfg.providers.get("anthropic").unwrap().api_key.is_none());

        std::env::set_var("OPENAI_API_KEY", "sk-test-openai");
        let cfg = load_config_or_default(&missing).await.unwrap();
        assert_eq!(
            cfg.providers.get("openai").unwrap().api_key.as_deref(),
            Some("sk-test-openai")
        );
        assert!(cfg.providers.get("anthropic").unwrap().api_key.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lofi_config_overrides_user_config_path() {
        let _g = env_lock();
        let _l = capture_env("LOFI_CONFIG");
        let _x = capture_env("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", "/this/should/not/be/used");
        std::env::set_var("LOFI_CONFIG", "tmp/config.toml");
        let p = user_config_path().unwrap();
        assert_eq!(p, std::path::PathBuf::from("tmp/config.toml"));
    }
}
