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

/// Precedence: `$LOFI_CONFIG` (used verbatim as the full file path), then
/// `$XDG_CONFIG_HOME`, then `~/.config`, with `lofi/config.toml` appended.
/// [`dirs::config_dir`] is not used: on macOS it resolves to
/// `~/Library/Application Support`, but the config belongs in `~/.config`.
/// # Errors
/// Returns [`Error::Config`] only when no `$XDG_CONFIG_HOME` and no `HOME` can
/// be determined and `$LOFI_CONFIG` is not set.
pub fn user_config_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("LOFI_CONFIG") {
        return Ok(PathBuf::from(p));
    }
    let mut base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty());
    if base.is_none() {
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            base = Some(PathBuf::from(home).join(".config"));
        }
    }
    let mut path = base.ok_or_else(|| Error::Config("could not resolve user config dir".into()))?;
    path.push("lofi");
    path.push("config.toml");
    Ok(path)
}

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

/// # Errors
/// Returns configuration errors when the config or policy path cannot be
/// resolved, read, or parsed.
pub fn evaluate_shell_policy(command: &str) -> Result<lofi_code::policy::Decision> {
    let config_path = user_config_path()?;
    let policy_path = policy_config_path(&config_path)?;
    let config = load_policy_or_default(&policy_path)?;
    Ok(lofi_code::policy::defaults::resolve(&config).evaluate(command))
}

/// This is the low-level primitive used by [`load_config`]; callers may also
/// use it directly to resolve ad-hoc values. It is `async` because the `!cmd`
/// form spawns a subprocess via tokio.
/// # Errors
/// - [`Error::Config`] if an env var is missing or a shell command fails /
///   cannot be spawned.
pub async fn resolve_value(s: &str, timeout: std::time::Duration) -> Result<String> {
    if let Some(cmd) = s.strip_prefix('!') {
        run_shell(cmd, timeout).await
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

/// Parse a NAME[=VALUE] env spec from --env. A bare NAME forwards the
/// parent shell's value; NAME=VALUE sets it explicitly. Returns the pair
/// ready to be applied to the process environment.
/// # Errors
/// `Error::Config` when a bare NAME is not set in the parent environment.
pub fn parse_env_spec(spec: &str) -> Result<(String, String)> {
    let Some((name, value)) = spec.split_once('=') else {
        let value =
            std::env::var(spec).map_err(|_| Error::Config(format!("env var not set: {spec}")))?;
        return Ok((spec.to_string(), value));
    };
    if name.is_empty() {
        return Err(Error::Config(format!("invalid env spec: {spec}")));
    }
    Ok((name.to_string(), value.to_string()))
}

/// Apply --env specs to the process environment, returning a guard that
/// restores the prior values on drop. The specs are applied lazily (before
/// config resolution) so env-based config values and the agent's bash
/// environment all observe them.
#[must_use]
pub fn apply_env_specs(specs: &[(String, String)]) -> EnvRestoreGuard {
    let mut guard = EnvRestoreGuard::default();
    for (name, value) in specs {
        guard.capture(name);
        std::env::set_var(name, value);
    }
    guard
}

/// Restores env vars mutated by `apply_env_specs` on drop.
#[derive(Default)]
pub struct EnvRestoreGuard {
    prev: Vec<(String, Option<String>)>,
}

impl EnvRestoreGuard {
    fn capture(&mut self, name: &str) {
        self.prev.push((name.to_string(), std::env::var(name).ok()));
    }
}

impl Drop for EnvRestoreGuard {
    fn drop(&mut self) {
        for (name, prev) in self.prev.drain(..).rev() {
            match prev {
                Some(v) => std::env::set_var(name, v),
                None => std::env::remove_var(name),
            }
        }
    }
}

/// Cap captured credential-helper output so a misbehaving command can't
/// fill memory within the timeout window.
const CRED_CMD_MAX_BYTES: usize = 64 * 1024;

async fn run_shell(cmd: &str, timeout: std::time::Duration) -> Result<String> {
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
        timeout,
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
                "credential command timed out after {}ms: {cmd}",
                timeout.as_millis()
            )))
        }
    }
}

/// Prefix for environment-based config overrides.
/// `LOFI__SECTION__KEY=VALUE` replaces `[section] key` (deeper nesting uses
/// more `__` separators, e.g. `LOFI__PROVIDERS__OPENAI__API_KEY`). Segment
/// names are lowercased; the value parses as TOML when possible (`200`,
/// `true`, `["a", "b"]`), otherwise it is used as a plain string.
const ENV_OVERRIDE_PREFIX: &str = "LOFI__";

/// Apply every `LOFI__*` environment variable over the parsed config
/// document, then deserialize. Overrides exist so single values can be
/// replaced without editing the file — including by tests lowering the
/// credential-command timeout.
/// # Errors
/// [`Error::Config`] when an override names an empty path segment or walks
/// through a non-table value.
fn apply_env_overrides(doc: &mut toml::Value) -> Result<()> {
    for (name, value) in std::env::vars() {
        let Some(rest) = name.strip_prefix(ENV_OVERRIDE_PREFIX) else {
            continue;
        };
        let path: Vec<String> = rest.split("__").map(str::to_lowercase).collect();
        if path.iter().any(String::is_empty) {
            return Err(Error::Config(format!("env override {name}: empty segment")));
        }
        let value = toml::from_str::<toml::Table>(&format!("v = {value}"))
            .ok()
            .and_then(|mut t| t.remove("v"))
            .unwrap_or(toml::Value::String(value));
        let mut node = &mut *doc;
        for seg in &path[..path.len() - 1] {
            match node {
                toml::Value::Table(table) => {
                    node = table
                        .entry(seg.clone())
                        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                }
                _ => {
                    return Err(Error::Config(format!(
                        "env override {name}: {seg} is not a table"
                    )))
                }
            }
        }
        match node {
            toml::Value::Table(table) => {
                table.insert(path[path.len() - 1].clone(), value);
            }
            _ => {
                return Err(Error::Config(format!(
                    "env override {name}: target is not a table"
                )))
            }
        }
    }
    Ok(())
}

/// # Errors
/// - [`Error::Config`] if the file cannot be read, fails to parse as TOML, or
///   an env override is malformed.
pub async fn load_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("failed to read {}: {e}", path.display())))?;
    let mut doc: toml::Value =
        toml::from_str(&content).map_err(|e| Error::Config(format!("parse error: {e}")))?;
    apply_env_overrides(&mut doc)?;
    let mut cfg: Config = doc
        .try_into()
        .map_err(|e| Error::Config(format!("parse error: {e}")))?;
    let policy_path = policy_config_path(path)?;
    cfg.shell_policy = load_policy_or_default(&policy_path)?;
    resolve_config(&mut cfg).await?;
    Ok(cfg)
}

/// Resolve every provider's credentials and headers inside `cfg` in place.
/// Resolution is **lenient** so a missing environment variable never aborts
/// startup: a provider whose key cannot be resolved is left keyless (and
/// therefore not available for selection). An explicit `api_key` (literal,
/// `$VAR`, or `!cmd`) overrides `env_name`; otherwise `env_name` names the
/// environment variable to read. Header values that fail to resolve are
/// dropped rather than fatal.
/// # Errors
/// Never errors in the current lenient implementation; the `Result` is kept
/// for signature stability and future strict modes.
pub async fn resolve_config(cfg: &mut Config) -> Result<()> {
    let timeout = std::time::Duration::from_millis(cfg.credential.timeout_ms);
    for provider in cfg.providers.values_mut() {
        if let Some(key) = provider.api_key.take() {
            provider.api_key = resolve_value(&key, timeout).await.ok().filter(|s| !s.is_empty());
        } else if let Some(name) = provider.env_name.as_ref() {
            provider.api_key = std::env::var(name).ok().filter(|s| !s.is_empty());
        }
        if let Some(base) = provider.base_url.take() {
            provider.base_url = resolve_value(&base, timeout).await.ok().filter(|s| !s.is_empty());
        }
        for model in provider.models.values_mut() {
            if let Some(base) = model.base_url.take() {
                model.base_url = resolve_value(&base, timeout).await.ok().filter(|s| !s.is_empty());
            }
        }
        if let Some(headers) = provider.headers.take() {
            let mut resolved = HashMap::with_capacity(headers.len());
            for (name, raw) in headers {
                if let Ok(v) = resolve_value(&raw, timeout).await {
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
/// Defines `OpenAI` Responses, Anthropic Messages, and Google Generative AI
/// providers. Each is keyed off its standard API-key environment variable.
/// They are resolved *leniently* by [`resolve_config`]:
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

[providers.google]
env_name = "GEMINI_API_KEY"
api_type = "google-generative-ai"

[providers.google.models."gemini-3.7-flash"]
reasoning = true
supports_image = true
thinking_levels = ["low", "medium", "high"]
"#;

/// # Panics
/// The shipped [`DEFAULT_CONFIG_TOML`] is a compile-time constant; this
/// panics only if it is malformed, which is a bug, not a runtime condition.
#[must_use]
pub fn default_config() -> Config {
    toml::from_str(DEFAULT_CONFIG_TOML).unwrap_or_else(|e| {
        panic!("built-in default config must parse: {e}");
    })
}

/// An existing file is parsed and resolved (leniently — see
/// [`resolve_config`]); a missing file yields the built-in default config.
/// In both cases a provider whose key is unset is left keyless, so the call
/// itself never fails for a missing environment variable — that surfaces
/// downstream as "no models configured". Both paths apply `LOFI__*` env
/// overrides.
/// # Errors
/// [`Error::Config`] when an existing file fails to read or parse as TOML,
/// or an env override is malformed.
pub async fn load_config_or_default(path: &Path) -> Result<Config> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => load_config(path).await,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut doc: toml::Value = toml::from_str(DEFAULT_CONFIG_TOML).map_err(|e| {
                Error::Config(format!("built-in default config must parse: {e}"))
            })?;
            apply_env_overrides(&mut doc)?;
            let mut cfg: Config = doc
                .try_into()
                .map_err(|e| Error::Config(format!("parse error: {e}")))?;
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
    use std::time::Duration;

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
        assert_eq!(resolve_value("hello world", Duration::from_secs(30)).await.unwrap(), "hello world");
        assert_eq!(resolve_value("", Duration::from_secs(30)).await.unwrap(), "");
    }

    #[tokio::test]
    async fn env_var_dollar() {
        std::env::set_var("LOFI_TEST_RESOLVE_BARE", "abc123");
        assert_eq!(
            resolve_value("$LOFI_TEST_RESOLVE_BARE", Duration::from_secs(30)).await.unwrap(),
            "abc123"
        );
    }

    #[tokio::test]
    async fn env_var_braces() {
        std::env::set_var("LOFI_TEST_RESOLVE_BRACED", "brval");
        assert_eq!(
            resolve_value("${LOFI_TEST_RESOLVE_BRACED}", Duration::from_secs(30)).await.unwrap(),
            "brval"
        );
    }

    #[tokio::test]
    async fn shell_command() {
        let v = resolve_value("!echo hello", Duration::from_secs(30)).await.unwrap();
        assert_eq!(v, "hello");
    }

    #[tokio::test]
    async fn shell_command_trims_output() {
        let v = resolve_value("!printf '  hi  \\n'", Duration::from_secs(30)).await.unwrap();
        assert_eq!(v, "hi");
    }

    #[tokio::test]
    async fn shell_command_failure_errors() {
        assert!(resolve_value("!false", Duration::from_secs(30)).await.is_err());
    }

    #[tokio::test]
    async fn escapes_dollar_and_bang() {
        assert_eq!(resolve_value("$$", Duration::from_secs(30)).await.unwrap(), "$");
        assert_eq!(resolve_value("$!", Duration::from_secs(30)).await.unwrap(), "!");
    }

    #[tokio::test]
    async fn literal_with_embedded_dollar_stays_literal() {
        assert_eq!(resolve_value("pa$$word", Duration::from_secs(30)).await.unwrap(), "pa$$word");
    }

    #[tokio::test]
    async fn missing_env_error() {
        let err = resolve_value("$LOFI_TEST_DEFINITELY_MISSING_XYZ", Duration::from_secs(30)).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn empty_braces_not_an_env_ref() {
        assert_eq!(resolve_value("${}", Duration::from_secs(30)).await.unwrap(), "${}");
    }

    #[test]
    fn default_config_has_native_providers() {
        let cfg = default_config();
        assert_eq!(cfg.providers.len(), 3);
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
        let google = cfg.providers.get("google").unwrap();
        assert_eq!(google.default_api(), Api::GoogleGenerativeAi);
        assert_eq!(google.env_name.as_deref(), Some("GEMINI_API_KEY"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn resolve_config_lenient_missing_env_leaves_keyless() {
        let _g = env_lock();
        let _o = capture_env("OPENAI_API_KEY");
        let _a = capture_env("ANTHROPIC_API_KEY");
        let _gk = capture_env("GEMINI_API_KEY");
        let mut cfg = default_config();
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("GEMINI_API_KEY");
        resolve_config(&mut cfg).await.unwrap();
        assert!(cfg.providers.get("openai").unwrap().api_key.is_none());
        assert!(cfg.providers.get("anthropic").unwrap().api_key.is_none());
        assert!(cfg.providers.get("google").unwrap().api_key.is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn load_config_or_default_detects_env_keys() {
        let _g = env_lock();
        let _o = capture_env("OPENAI_API_KEY");
        let _a = capture_env("ANTHROPIC_API_KEY");
        let _gk = capture_env("GEMINI_API_KEY");
        let dir = std::env::temp_dir().join("lofi-config-default-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("config.toml");

        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
        std::env::remove_var("GEMINI_API_KEY");
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

    #[test]
    fn xdg_config_home_is_used() {
        let _g = env_lock();
        let _l = capture_env("LOFI_CONFIG");
        let _x = capture_env("XDG_CONFIG_HOME");
        std::env::remove_var("LOFI_CONFIG");
        std::env::set_var("XDG_CONFIG_HOME", "/xdg-conf");
        let p = user_config_path().unwrap();
        assert_eq!(p, std::path::PathBuf::from("/xdg-conf/lofi/config.toml"));
    }

    #[test]
    fn config_falls_back_to_home_dot_config() {
        let _g = env_lock();
        let _l = capture_env("LOFI_CONFIG");
        let _x = capture_env("XDG_CONFIG_HOME");
        std::env::remove_var("LOFI_CONFIG");
        // An empty XDG_CONFIG_HOME is ignored per the XDG spec, so HOME drives
        // the fallback. This is the macOS path (no platform config dir there).
        std::env::set_var("XDG_CONFIG_HOME", "");
        let home = std::env::var("HOME").unwrap();
        let p = user_config_path().unwrap();
        assert_eq!(
            p,
            std::path::PathBuf::from(home).join(".config/lofi/config.toml")
        );
    }

    #[test]
    fn parse_env_spec_explicit_value() {
        assert_eq!(
            parse_env_spec("EXAMPLE_API_KEY=abc").unwrap(),
            ("EXAMPLE_API_KEY".to_string(), "abc".to_string())
        );
    }

    #[test]
    fn parse_env_spec_value_with_equals() {
        assert_eq!(
            parse_env_spec("EXAMPLE_BASE_URL=http://a=b/c").unwrap(),
            ("EXAMPLE_BASE_URL".to_string(), "http://a=b/c".to_string())
        );
    }

    #[test]
    fn parse_env_spec_bare_forwards_parent() {
        let _g = env_lock();
        let _v = capture_env("LOFI_TEST_PARSE_BARE");
        std::env::set_var("LOFI_TEST_PARSE_BARE", "parent-value");
        assert_eq!(
            parse_env_spec("LOFI_TEST_PARSE_BARE").unwrap(),
            (
                "LOFI_TEST_PARSE_BARE".to_string(),
                "parent-value".to_string()
            )
        );
    }

    #[test]
    fn parse_env_spec_bare_missing_errors() {
        let _g = env_lock();
        let _v = capture_env("LOFI_TEST_PARSE_MISSING");
        std::env::remove_var("LOFI_TEST_PARSE_MISSING");
        assert!(parse_env_spec("LOFI_TEST_PARSE_MISSING").is_err());
    }

    #[test]
    fn parse_env_spec_empty_name_errors() {
        assert!(parse_env_spec("=value").is_err());
    }

    #[test]
    fn apply_env_specs_sets_and_restores() {
        let _g = env_lock();
        let _v = capture_env("LOFI_TEST_APPLY");
        std::env::remove_var("LOFI_TEST_APPLY");
        {
            let guard = apply_env_specs(&[("LOFI_TEST_APPLY".to_string(), "set".to_string())]);
            assert_eq!(std::env::var("LOFI_TEST_APPLY").unwrap(), "set");
            drop(guard);
        }
        assert!(std::env::var("LOFI_TEST_APPLY").is_err());
    }

    #[test]
    fn apply_env_specs_restores_prior_value() {
        let _g = env_lock();
        let _v = capture_env("LOFI_TEST_APPLY_PRIOR");
        std::env::set_var("LOFI_TEST_APPLY_PRIOR", "prior");
        {
            let guard =
                apply_env_specs(&[("LOFI_TEST_APPLY_PRIOR".to_string(), "new".to_string())]);
            assert_eq!(std::env::var("LOFI_TEST_APPLY_PRIOR").unwrap(), "new");
            drop(guard);
        }
        assert_eq!(std::env::var("LOFI_TEST_APPLY_PRIOR").unwrap(), "prior");
        #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn load_config_honors_env_overrides() {
        let _g = env_lock();
        let _r = capture_env("LOFI__RETRY__MAX_RETRIES");
        let _b = capture_env("LOFI__PROVIDERS__P__BASE_URL");
        let _s = capture_env("LOFI__PROVIDERS__P__NO_AUTH");
        let _d = capture_env("LOFI__DEFAULT_MODEL");
        std::env::set_var("LOFI__RETRY__MAX_RETRIES", "42");
        // TOML literal strings keep the override free of double quotes.
        std::env::set_var("LOFI__PROVIDERS__P__BASE_URL", "'https://override.invalid'");
        std::env::set_var("LOFI__PROVIDERS__P__NO_AUTH", "true");
        // A value that is not TOML is taken as a plain string.
        std::env::set_var("LOFI__DEFAULT_MODEL", "p/plain-model");
        let dir = std::env::temp_dir().join("lofi-config-env-override-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"[providers.p]
api_type = "openai-completions"
base_url = "https://example.invalid"
"#,
        )
        .unwrap();
        let cfg = load_config(&path).await.unwrap();
        assert_eq!(cfg.retry.max_retries, 42);
        let p = cfg.providers.get("p").unwrap();
        assert_eq!(p.base_url.as_deref(), Some("https://override.invalid"));
        assert!(p.no_auth);
        assert_eq!(cfg.default_model.as_deref(), Some("p/plain-model"));
        let _ = std::fs::remove_dir_all(&dir);
    }


    /// Env overrides mutate the parsed document before deserialization.
    /// The round trip must preserve table order: model fallback selection
    /// depends on provider declaration order (the toml `preserve_order`
    /// feature is required).
    #[test]
    fn value_roundtrip_preserves_declaration_order() {
        let toml_text = "default_model = \"mock/chat\"\n\n[retry]\nmax_retries = 2\nbase_delay_ms = 1\nmax_delay_ms = 1\n\n[providers.mock]\nbase_url = \"http://127.0.0.1:1\"\napi_type = \"openai-completions\"\nno_auth = true\n\n[providers.mock.models.chat]\nname = \"A Chat\"\ncontext_window = 100000\nreasoning = true\nsupports_image = true\nthinking_level = \"medium\"\nthinking_levels = [\"low\", \"medium\", \"high\", \"xhigh\"]\nservice_tier = \"flex\"\nservice_tiers = [\"flex\", \"priority\"]\n\n[providers.anthropic]\nbase_url = \"http://127.0.0.1:1\"\napi_type = \"anthropic-messages\"\nno_auth = true\n\n[providers.anthropic.models.tools]\nname = \"D Anthropic\"\ncontext_window = 100000\nreasoning = true\nthinking_level = \"medium\"\nthinking_levels = [\"low\", \"medium\", \"high\", \"xhigh\"]\n";
        let direct: Config = toml::from_str(toml_text).unwrap();
        let doc: toml::Value = toml::from_str(toml_text).unwrap();
        let roundtrip: Config = doc.try_into().unwrap();
        assert_eq!(
            direct.providers.keys().collect::<Vec<_>>(),
            roundtrip.providers.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            direct.providers.get("mock").unwrap().default_api(),
            roundtrip.providers.get("mock").unwrap().default_api()
        );
    }
}
