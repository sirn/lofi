//! Integration tests for `lofi_core::config_loader`.
//!
//! Exercises end-to-end TOML loading with `env_name` detection, `$VAR`///! `!cmd` value resolution, and the lenient-missing-env behavior. The
//! user-config loader is read-only, so each test writes a throwaway TOML
//! under a [`tempfile`] tempdir and points `load_config` at it.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use lofi_core::config_loader::load_config;
use lofi_types::{Api, Config};

#[tokio::test]
async fn load_config_resolves_env_name_and_shell_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = r#"
[providers.openai]
api_type = "openai-completions"
env_name = "LOFI_TEST_KEY"

[providers.openai.headers]
x-custom = "!echo hdr"
"#;
    std::fs::write(&path, toml).unwrap();

    std::env::set_var("LOFI_TEST_KEY", "test-secret-123");
    let cfg: Config = load_config(&path).await.unwrap();
    std::env::remove_var("LOFI_TEST_KEY");

    let p = cfg.providers.get("openai").unwrap();
    assert_eq!(p.api_type, Api::OpenAiCompletions);
    assert_eq!(p.api_key.as_deref(), Some("test-secret-123"));
    let headers: HashMap<String, String> = p.headers.clone().unwrap_or_default();
    assert_eq!(headers.get("x-custom").map(String::as_str), Some("hdr"));
}

#[tokio::test]
async fn load_config_keeps_literal_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = r#"
[providers.openai]
api_type = "openai-completions"
api_key = "sk-literal"
"#;
    std::fs::write(&path, toml).unwrap();

    let cfg = load_config(&path).await.unwrap();
    let p = cfg.providers.get("openai").unwrap();
    assert_eq!(p.api_key.as_deref(), Some("sk-literal"));
}

/// A missing env var (whether via `env_name` or `$VAR`) leaves the provider
/// keyless rather than erroring, so a fresh checkout with no keys starts up.
#[tokio::test]
async fn load_config_missing_env_leaves_keyless() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = r#"
[providers.openai]
api_type = "openai-completions"
env_name = "LOFI_TEST_DEFINITELY_MISSING_XYZ_42"

[providers.other]
api_type = "openai-completions"
api_key = "$LOFI_TEST_DEFINITELY_MISSING_XYZ_42"
"#;
    std::fs::write(&path, toml).unwrap();

    std::env::remove_var("LOFI_TEST_DEFINITELY_MISSING_XYZ_42");
    let cfg = load_config(&path).await.unwrap();
    assert!(cfg.providers.get("openai").unwrap().api_key.is_none());
    assert!(cfg.providers.get("other").unwrap().api_key.is_none());
}

#[tokio::test]
async fn load_config_accepts_kebab_api_type() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = r#"
[providers.anthropic]
api_type = "anthropic-messages"
env_name = "LOFI_TEST_ANTHROPIC"
"#;
    std::fs::write(&path, toml).unwrap();
    std::env::set_var("LOFI_TEST_ANTHROPIC", "sk");
    let cfg: Config = load_config(&path).await.unwrap();
    std::env::remove_var("LOFI_TEST_ANTHROPIC");
    assert_eq!(
        cfg.providers.get("anthropic").unwrap().api_type,
        Api::AnthropicMessages
    );
}