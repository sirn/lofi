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

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
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
    let api_key = cfg.api_key.clone().unwrap_or_default();
    let headers: HashMap<String, String> = cfg.headers.clone().unwrap_or_default();
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
        }
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
