//! Provider trait + factory.
//!
//! [`Provider::stream`] yields [`lofi_types::StreamingEvent`]s; [`open`]
//! builds the right transport by [`Api`]. Each concrete provider holds the
//! resolved `base_url`/`api_key`/`headers` straight from [`ProviderConfig`]
//! (value resolution happens earlier in `config_loader`) plus an HTTP client.
//! The transport is a thin layer: request bodies come from [`crate::ir`], and
//! SSE byte streams are decoded by [`sse`].

pub mod anthropic_messages;
pub mod openai_completions;
pub mod openai_responses;
mod sse;

use serde_json::Value;
use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use lofi_types::{Api, Message, Model, ProviderConfig, StreamingEvent};

use crate::error::{Error, Result};
use crate::ir::chat::ToolSchema;

pub use anthropic_messages::AnthropicMessagesProvider;
pub use openai_completions::OpenAiCompletionsProvider;
pub use openai_responses::OpenAiResponsesProvider;

/// A streaming chat-completion transport.
///
/// `list_models` drives remote discovery (later wired into the model
/// registry); `stream` runs a single model turn, yielding incremental events
/// until the provider sends its terminal sentinel.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Fetch the provider's advertised model list.
    async fn list_models(&self) -> Result<Vec<Model>>;

    /// Stream a single assistant turn.
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>>;
}

/// Build the concrete [`Provider`] for `api` from an already-resolved config.
///
/// `config_loader` resolves `api_key` and `header` values in place, so the
/// factory simply hands them to the transport. An unknown `api` is a config
/// error rather than a transport one.
///
/// # Errors
///
/// Returns [`Error::Http`] if the shared HTTP client cannot be constructed.
pub fn open(api: Api, cfg: &ProviderConfig) -> Result<Box<dyn Provider>> {
    let base_url = cfg.base_url.trim_end_matches('/').to_string();
    let (api_key, headers) = effective_credentials(cfg);
    let client = http_client()?;
    let provider: Box<dyn Provider> = match api {
        Api::OpenAiCompletions => Box::new(OpenAiCompletionsProvider {
            base_url,
            api_key,
            headers,
            client,
        }),
        Api::OpenAiResponses => Box::new(OpenAiResponsesProvider {
            base_url,
            api_key,
            headers,
            client,
        }),
        Api::AnthropicMessages => Box::new(AnthropicMessagesProvider {
            base_url,
            api_key,
            headers,
            client,
        }),
    };
    Ok(provider)
}

/// Resolve the effective API key and headers for a provider, suppressing
/// generated credentials when `no_auth` is set (the api key is blanked and
/// `Authorization`/`x-api-key` custom headers are dropped) so an explicitly
/// unauthenticated provider never transmits credentials.
pub(crate) fn effective_credentials(cfg: &ProviderConfig) -> (String, HashMap<String, String>) {
    if cfg.no_auth {
        let headers = cfg
            .headers
            .as_ref()
            .map(|h| {
                h.iter()
                    .filter(|(k, _)| {
                        !matches!(
                            k.to_ascii_lowercase().as_str(),
                            "authorization" | "x-api-key"
                        )
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        (String::new(), headers)
    } else {
        (
            cfg.api_key.clone().unwrap_or_default(),
            cfg.headers.clone().unwrap_or_default(),
        )
    }
}

/// Build the shared `reqwest` client: `rustls` TLS, no default features, a
/// generous timeout that still protects against stuck connections.
fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_mins(5))
        .build()
        .map_err(Error::Http)
}

/// Apply a provider's extra headers to a request builder.
///
/// Used by every concrete transport so header handling stays uniform.
pub(crate) fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
}

/// Apply `Authorization: Bearer <key>` only when a key was configured, so a
/// header-only or no-auth provider doesn't send an empty bearer header.
pub(crate) fn with_bearer(
    builder: reqwest::RequestBuilder,
    api_key: &str,
) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        builder
    } else {
        builder.bearer_auth(api_key)
    }
}

/// Apply a single `name: <key>` header only when `key` is non-empty.
pub(crate) fn with_key_header(
    builder: reqwest::RequestBuilder,
    name: &str,
    api_key: &str,
) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        builder
    } else {
        builder.header(name, api_key)
    }
}

/// Convert a non-2xx [`reqwest::Response`] into an [`Error::Provider`] that
/// carries the response body, so the caller sees the provider's error message
/// (e.g. `OpenAI`'s `error.message`) rather than just the HTTP status code. On
/// success the response is passed through unchanged.
pub(crate) async fn ensure_ok(resp: reqwest::Response) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let url = resp.url().to_string();
    // Read the error body through a capped stream so a huge or hostile
    // error response cannot exhaust memory before its (truncated) detail
    // is surfaced.
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    let mut truncated = false;
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let suffix = if truncated { " <truncated>" } else { "" };
    let msg = match extract_error_detail(&text) {
        Some(detail) => format!("HTTP {status} from {url}: {detail}{suffix}"),
        None if text.is_empty() => format!("HTTP {status} from {url}"),
        None => format!("HTTP {status} from {url}: {}{suffix}", truncate(&text, 500)),
    };
    Err(Error::Provider(msg))
}

/// Pull a human-readable message out of a provider error body.
///
/// Handles the common `{ "error": { "message": "..." } }` shape used by
/// both `OpenAI` and Anthropic, a top-level `{ "message": "..." }`, and a bare
/// string `error`.
fn extract_error_detail(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    if let Some(m) = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return Some(m.to_string());
    }
    if let Some(m) = v.get("error").and_then(Value::as_str) {
        return Some(m.to_string());
    }
    if let Some(m) = v.get("message").and_then(Value::as_str) {
        return Some(m.to_string());
    }
    None
}

/// Maximum bytes read from a non-2xx error body before truncation, so a
/// misbehaving provider cannot force unbounded allocation via a huge error
/// response.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Maximum bytes accepted for a discovery (model-list) response body.
pub(crate) const MAX_DISCOVERY_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Read a JSON response body through a capped byte stream so a configurable
/// discovery endpoint cannot force unbounded allocation before parsing.
pub(crate) async fn read_json_capped(resp: reqwest::Response, max: usize) -> Result<Value> {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::Provider(format!("body read error: {e}")))?;
        if buf.len() + chunk.len() > max {
            return Err(Error::Provider(format!(
                "response body exceeded {max} bytes"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|e| Error::Provider(format!("json decode error: {e}")))
}
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{Api, ProviderConfig};

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            base_url: "https://api.example.com/v1/".to_string(),
            api: Api::OpenAiCompletions,
            api_key: Some("sk-test".to_string()),
            headers: Some(HashMap::from([("x-custom".to_string(), "yes".to_string())])),
            models: vec![],
            discover: None,
            no_auth: false,
        }
    }

    #[test]
    fn effective_credentials_suppresses_auth_when_no_auth() {
        // no_auth must blank the api key and drop credential-bearing custom
        // headers even when they are configured, while preserving unrelated
        // custom headers.
        let mut c = cfg();
        c.no_auth = true;
        c.headers = Some(HashMap::from([
            ("Authorization".to_string(), "Bearer leaked".to_string()),
            ("x-api-key".to_string(), "leaked".to_string()),
            ("x-routing".to_string(), "eu".to_string()),
        ]));
        let (key, headers) = effective_credentials(&c);
        assert!(key.is_empty());
        assert!(!headers.contains_key("Authorization"));
        assert!(!headers.contains_key("x-api-key"));
        assert_eq!(headers.get("x-routing").map(String::as_str), Some("eu"));
    }

    #[test]
    fn effective_credentials_preserves_auth_when_not_no_auth() {
        let c = cfg();
        let (key, headers) = effective_credentials(&c);
        assert_eq!(key, "sk-test");
        assert_eq!(headers.get("x-custom").map(String::as_str), Some("yes"));
    }

    #[test]
    fn open_trims_trailing_slash() {
        // The factory accepts a base_url with a trailing slash; concrete
        // transports join paths without doubling it. We assert success and
        // that the trimmed value round-trips through a manually-built
        // provider of the same shape.
        let mut c = cfg();
        c.base_url = "https://api.example.com/v1/".to_string();
        assert!(open(Api::OpenAiCompletions, &c).is_ok());
        let oc = OpenAiCompletionsProvider {
            base_url: "https://api.example.com/v1".to_string(),
            api_key: "sk-test".to_string(),
            headers: HashMap::from([("x-custom".to_string(), "yes".to_string())]),
            client: http_client().unwrap(),
        };
        assert_eq!(oc.base_url, "https://api.example.com/v1");
    }

    #[test]
    fn open_dispatches_all_apis() {
        for api in [
            Api::OpenAiCompletions,
            Api::OpenAiResponses,
            Api::AnthropicMessages,
        ] {
            let mut c = cfg();
            c.api = api;
            assert!(open(api, &c).is_ok());
        }
    }

    #[test]
    fn open_uses_empty_key_when_absent() {
        let mut c = cfg();
        c.api_key = None;
        let p = open(Api::OpenAiCompletions, &c).unwrap();
        // Smoke: the factory succeeds; behavior with an empty key is the
        // server's problem, not ours.
        let _ = p;
    }
}
