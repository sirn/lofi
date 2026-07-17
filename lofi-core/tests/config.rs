//! Integration tests for `lofi_core::config_loader`.
//!
//! Exercises end-to-end TOML loading with value resolution (`$VAR` api keys
//! and `!cmd` headers) plus the `--api-key` override path. The user-config
//! loader is read-only, so each test writes a throwaway TOML under a
//! [`tempfile`] tempdir and points `load_config` at it.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;

use lofi_core::config_loader::{apply_api_key_override, load_config};
use lofi_types::{Api, Config};

/// Write a TOML config with a `$VAR` api key and a `!cmd` header, then assert
/// both are resolved after [`load_config`].
#[tokio::test]
async fn load_config_resolves_env_key_and_shell_header() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = concat!(
        "default_provider = \"openai\"\n",
        "\n",
        "[providers.openai]\n",
        "base_url = \"https://api.openai.com/v1\"\n",
        "api = \"openai_completions\"\n",
        "api_key = \"$LOFI_TEST_KEY\"\n",
        "\n",
        "[providers.openai.headers]\n",
        "x-custom = \"!echo hdr\"\n",
    );
    std::fs::write(&path, toml).unwrap();

    std::env::set_var("LOFI_TEST_KEY", "test-secret-123");
    let cfg: Config = load_config(&path).await.unwrap();
    std::env::remove_var("LOFI_TEST_KEY");

    let p = cfg.providers.get("openai").unwrap();
    assert_eq!(p.api, Api::OpenAiCompletions);
    assert_eq!(p.api_key.as_deref(), Some("test-secret-123"));
    let headers: HashMap<String, String> = p.headers.clone().unwrap_or_default();
    assert_eq!(headers.get("x-custom").map(String::as_str), Some("hdr"));
    assert_eq!(cfg.default_provider.as_deref(), Some("openai"));
}

/// A literal api key (no `$` / `!`) survives load unchanged.
#[tokio::test]
async fn load_config_keeps_literal_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = concat!(
        "[providers.openai]\n",
        "base_url = \"https://api.openai.com/v1\"\n",
        "api = \"openai_completions\"\n",
        "api_key = \"sk-literal\"\n",
    );
    std::fs::write(&path, toml).unwrap();

    let cfg = load_config(&path).await.unwrap();
    let p = cfg.providers.get("openai").unwrap();
    assert_eq!(p.api_key.as_deref(), Some("sk-literal"));
}

/// `apply_api_key_override` replaces the resolved key for the named provider
/// and errors for an unknown provider.
#[tokio::test]
async fn apply_api_key_override_replaces_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = concat!(
        "[providers.openai]\n",
        "base_url = \"https://api.openai.com/v1\"\n",
        "api = \"openai_completions\"\n",
        "api_key = \"$LOFI_TEST_OVERRIDE_KEY\"\n",
    );
    std::fs::write(&path, toml).unwrap();

    std::env::set_var("LOFI_TEST_OVERRIDE_KEY", "from-env");
    let mut cfg = load_config(&path).await.unwrap();
    std::env::remove_var("LOFI_TEST_OVERRIDE_KEY");

    assert_eq!(
        cfg.providers.get("openai").unwrap().api_key.as_deref(),
        Some("from-env")
    );

    apply_api_key_override(&mut cfg, "openai", "override-key".to_string()).unwrap();
    assert_eq!(
        cfg.providers.get("openai").unwrap().api_key.as_deref(),
        Some("override-key")
    );

    // Unknown provider is a config error, not a panic.
    assert!(apply_api_key_override(&mut cfg, "nope", "k".to_string()).is_err());
}

/// A missing env var in the api key surfaces as a config error rather than a
/// silent empty value.
#[tokio::test]
async fn load_config_missing_env_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let toml = concat!(
        "[providers.openai]\n",
        "base_url = \"https://api.openai.com/v1\"\n",
        "api = \"openai_completions\"\n",
        "api_key = \"$LOFI_TEST_DEFINITELY_MISSING_XYZ_42\"\n",
    );
    std::fs::write(&path, toml).unwrap();

    std::env::remove_var("LOFI_TEST_DEFINITELY_MISSING_XYZ_42");
    assert!(load_config(&path).await.is_err());
}
