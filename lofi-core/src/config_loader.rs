//! User-config loading and value resolution.
//!
//! Reads `$XDG_CONFIG_HOME/lofi/config.toml` and parses it into a
//! [`lofi_types::Config`], resolving `api_key` and `header` values along the
//! way. This tree is treated as **read-only**: lofi never creates, writes, or
//! modifies files under the user config dir. That separation lets an external
//! config manager (Nix, stow, etc.) own the tree declaratively while lofi
//! limits its writes to the agent-owned state dir (see [`crate::state`]).
//!
//! Value resolution mirrors Pi's config syntax so prompts/configs transfer:
//! - `!cmd`  -> run `sh -c cmd` and take the trimmed stdout;
//! - `$VAR` / `${VAR}` -> read the env var (missing is an error);
//! - `$$` -> a literal `$`, `$!` -> a literal `!` (escapes so a value that
//!   would otherwise look like a command or env ref can be expressed);
//! - anything else is taken literally.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use lofi_types::{Api, Config, ModelConfig, ProviderConfig};

/// Resolve the path to the user config file.
///
/// Uses [`dirs::config_dir`] (which honors `$XDG_CONFIG_HOME` on Linux and
/// falls back to `~/.config`), then appends `lofi/config.toml`. If the
/// platform's config dir cannot be resolved, fall back to `~/.config`.
///
/// # Errors
/// Returns [`Error::Config`] only when no config base directory can be
/// determined at all (e.g. `HOME` is unset).
pub fn user_config_path() -> Result<PathBuf> {
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

/// Extract `VAR` from `$VAR` or `${VAR}`; returns `None` for anything else.
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
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| Error::Config(format!("failed to spawn shell command: {e}")))?;
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
    // drained and discarded rather than retained.
    let cap = CRED_CMD_MAX_BYTES;
    let waited = tokio::time::timeout(
        std::time::Duration::from_millis(CRED_CMD_TIMEOUT_MS),
        Box::pin(async move {
            let (out, err) = tokio::join!(
                crate::tools::builtins::read_capped(&mut stdout, cap),
                crate::tools::builtins::read_capped(&mut stderr, cap),
            );
            let out = out?;
            let err = err?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, out, err))
        }),
    )
    .await;
    match waited {
        Ok(Ok((status, (out_bytes, out_truncated), (err_bytes, err_truncated)))) => {
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
        Ok(Err(e)) => Err(Error::Config(format!("shell command failed: {e}"))),
        Err(_) => Err(Error::Config(format!(
            "credential command timed out after {CRED_CMD_TIMEOUT_MS}ms: {cmd}"
        ))),
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
    resolve_config(&mut cfg).await?;
    Ok(cfg)
}

/// Resolve every `api_key` and header value inside `cfg` in place.
///
/// # Errors
/// Returns [`Error::Config`] if any value fails to resolve.
pub async fn resolve_config(cfg: &mut Config) -> Result<()> {
    for provider in cfg.providers.values_mut() {
        if let Some(key) = provider.api_key.take() {
            provider.api_key = Some(resolve_value(&key).await?);
        }
        if let Some(headers) = provider.headers.take() {
            let mut resolved = HashMap::with_capacity(headers.len());
            for (name, raw) in headers {
                resolved.insert(name, resolve_value(&raw).await?);
            }
            provider.headers = Some(resolved);
        }
    }
    Ok(())
}

/// Apply a `--api-key` CLI override: replace the resolved `api_key` of the
/// named provider with `api_key` (taken literally, no resolution).
///
/// # Errors
/// Returns [`Error::Config`] if `provider` is not present in `cfg`.
pub fn apply_api_key_override(cfg: &mut Config, provider: &str, api_key: String) -> Result<()> {
    let p = cfg
        .providers
        .get_mut(provider)
        .ok_or_else(|| Error::Config(format!("unknown provider: {provider}")))?;
    p.api_key = Some(api_key);
    Ok(())
}

/// The built-in configuration used when no user config file exists.
///
/// Zero-config usage requires only `$OPENAI_API_KEY`: the default defines a
/// single `openai` provider (Responses API) with three static models —
/// `gpt-5.6-luna`, `gpt-5.6-terra`, `gpt-5.6-sol` — and keys it off
/// `$OPENAI_API_KEY`. The key is an env expression resolved lazily by
/// [`resolve_config_lenient`]; if it is unset the provider is left keyless
/// and model selection fails with a clear message.
///
/// Supplying your own `config.toml` (or `--api-key`) replaces these
/// defaults entirely, so `OPENAI_API_KEY` is only required on the
/// zero-config path — a custom provider may use any auth you like.
#[must_use]
pub fn default_config() -> Config {
    let mut providers = HashMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api: Api::OpenAiResponses,
            api_key: Some("$OPENAI_API_KEY".to_string()),
            no_auth: false,
            headers: None,
            models: vec![
                model("gpt-5.6-luna", "GPT-5.6 Luna", 400_000, 128_000),
                model("gpt-5.6-terra", "GPT-5.6 Terra", 400_000, 128_000),
                model("gpt-5.6-sol", "GPT-5.6 Sol", 400_000, 128_000),
            ],
            discover: None,
        },
    );
    Config {
        providers,
        default_provider: None,
        default_model: None,
    }
}

/// Build a static [`ModelConfig`] with image support and no reasoning trace.
fn model(id: &str, name: &str, context_window: u64, max_tokens: u64) -> ModelConfig {
    ModelConfig {
        id: id.to_string(),
        name: Some(name.to_string()),
        reasoning: Some(false),
        supports_image: Some(true),
        context_window: Some(context_window),
        max_tokens: Some(max_tokens),
    }
}

/// Like [`resolve_config`] but never errors: a value that cannot be resolved
/// (missing env var, failed shell command) leaves the provider keyless
/// (`api_key = None`, header dropped) instead of aborting. Used for the
/// built-in default config so an unset `OPENAI_API_KEY` doesn't break
/// startup — the provider is simply filtered out of `available` models.
pub async fn resolve_config_lenient(cfg: &mut Config) {
    for provider in cfg.providers.values_mut() {
        if let Some(key) = provider.api_key.take() {
            provider.api_key = resolve_value(&key).await.ok();
        }
        if let Some(headers) = provider.headers.take() {
            let mut resolved = HashMap::with_capacity(headers.len());
            for (name, raw) in headers {
                if let Ok(v) = resolve_value(&raw).await {
                    resolved.insert(name, v);
                }
            }
            provider.headers = Some(resolved);
        }
    }
}

/// Load the user config from `path`, or fall back to [`default_config`] when
/// the file does not exist.
///
/// An existing file is parsed and resolved *strictly* (a typo'd `$VAR` is an
/// error, since the user authored it) and entirely replaces the defaults, so
/// `OPENAI_API_KEY` is not required when a custom provider is configured. A
/// missing file yields the built-in defaults resolved *leniently*; if the
/// single `openai` provider's key resolved, it is set as
/// [`Config::default_provider`].
///
/// # Errors
/// [`Error::Config`] only when an existing file fails to read/parse/resolve.
pub async fn load_config_or_default(path: &Path) -> Result<Config> {
    if path.exists() {
        return load_config(path).await;
    }
    let mut cfg = default_config();
    resolve_config_lenient(&mut cfg).await;
    if cfg.default_provider.is_none() {
        let pick = cfg
            .providers
            .iter()
            .find(|(_, p)| p.api_key.as_deref().is_some_and(|k| !k.is_empty()))
            .map(|(k, _)| k.clone());
        cfg.default_provider = pick;
    }
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

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
        // `pa$$word` is not a valid env ref, so it stays literal (the `$$`
        // escape only applies when it is the *entire* value).
        assert_eq!(resolve_value("pa$$word").await.unwrap(), "pa$$word");
    }

    #[tokio::test]
    async fn missing_env_error() {
        let err = resolve_value("$LOFI_TEST_DEFINITELY_MISSING_XYZ").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn empty_braces_not_an_env_ref() {
        // `${}` is rejected as an env ref and treated as a literal.
        assert_eq!(resolve_value("${}").await.unwrap(), "${}");
    }

    #[test]
    fn default_config_has_single_openai_provider() {
        let cfg = default_config();
        assert_eq!(cfg.providers.len(), 1);
        let openai = cfg.providers.get("openai").unwrap();
        assert_eq!(openai.base_url, "https://api.openai.com/v1");
        assert_eq!(openai.api, Api::OpenAiResponses);
        assert_eq!(openai.api_key.as_deref(), Some("$OPENAI_API_KEY"));
        assert!(openai.discover.is_none());
        let ids: Vec<&str> = openai.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"]);
        assert!(!cfg.providers.contains_key("anthropic"));
    }

    #[tokio::test]
    async fn resolve_config_lenient_missing_env_leaves_keyless() {
        let mut cfg = default_config();
        std::env::remove_var("OPENAI_API_KEY");
        resolve_config_lenient(&mut cfg).await;
        assert!(cfg.providers.get("openai").unwrap().api_key.is_none());
    }

    #[tokio::test]
    async fn load_config_or_default_uses_defaults_and_picks_keyed() {
        let dir = std::env::temp_dir().join("lofi-config-default-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("config.toml");

        // No key set: the single openai provider is keyless, no default chosen.
        std::env::remove_var("OPENAI_API_KEY");
        let cfg = load_config_or_default(&missing).await.unwrap();
        assert_eq!(cfg.providers.len(), 1);
        assert!(cfg.providers.get("openai").unwrap().api_key.is_none());
        assert!(cfg.default_provider.is_none());

        // OPENAI_API_KEY set: openai is keyed and chosen as default.
        std::env::set_var("OPENAI_API_KEY", "sk-test-openai");
        let cfg = load_config_or_default(&missing).await.unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
        assert_eq!(
            cfg.providers.get("openai").unwrap().api_key.as_deref(),
            Some("sk-test-openai")
        );
        std::env::remove_var("OPENAI_API_KEY");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
