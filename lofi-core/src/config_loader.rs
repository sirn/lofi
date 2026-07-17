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
use lofi_types::Config;

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

async fn run_shell(cmd: &str) -> Result<String> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .await
        .map_err(|e| Error::Config(format!("failed to spawn shell command: {e}")))?;
    if !output.status.success() {
        return Err(Error::Config(format!(
            "shell command exited {status}: {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
}
