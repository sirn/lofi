//! Anthropic Messages HTTP transport.
//!
//! POSTs to `/v1/messages` with `x-api-key` + `anthropic-version` headers.
//! Unlike the `OpenAI` transports, Anthropic SSE blocks carry an `event:`
//! line, so the mapper forwards the `(event, data)` pair to
//! [`map_anthropic_event`].

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Api, Message, Model, StreamingEvent};
use serde_json::Value;

use super::{apply_headers, with_key_header};
use crate::ir::anthropic_messages::{map_anthropic_event, AnthropicMapperState};
use crate::ir::build_request;
use crate::ir::chat::ToolSchema;
use crate::ir::codec::SseEvent;
use crate::sse::{map_sse_response, SseMapper};
use lofi_error::{Error, Result};

/// Anthropic API version header value. Pinned to the documented stable date.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Anthropic Messages transport.
pub struct AnthropicMessagesProvider {
    /// Base URL with no trailing slash.
    pub base_url: String,
    /// Resolved `x-api-key` value.
    pub api_key: String,
    /// Extra resolved headers from config.
    pub headers: HashMap<String, String>,
    /// Shared HTTP client.
    pub client: reqwest::Client,
}

#[async_trait]
impl super::Provider for AnthropicMessagesProvider {
    async fn list_models(&self) -> Result<Vec<Model>> {
        let req = apply_headers(
            with_key_header(
                self.client.get(format!("{}/v1/models", self.base_url)),
                "x-api-key",
                &self.api_key,
            )
            .header("anthropic-version", ANTHROPIC_VERSION),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = super::ensure_ok(resp).await?;
        let body: Value = crate::read_json_capped(resp, crate::MAX_DISCOVERY_BODY_BYTES).await?;
        Ok(parse_anthropic_models(&body))
    }

    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = build_request(Api::AnthropicMessages, model, messages, tools);
        let req = apply_headers(
            with_key_header(
                self.client
                    .post(format!("{}/v1/messages", self.base_url))
                    .json(&body),
                "x-api-key",
                &self.api_key,
            )
            .header("anthropic-version", ANTHROPIC_VERSION),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = super::ensure_ok(resp).await?;
        Ok(map_sse_response(resp, AnthropicMapper::default()))
    }
}

/// Mapper that forwards each Anthropic `(event, data)` pair to
/// [`map_anthropic_event`].
#[derive(Default)]
struct AnthropicMapper {
    state: AnthropicMapperState,
}

impl SseMapper for AnthropicMapper {
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
        let v: Value = serde_json::from_str(&event.data)
            .map_err(|e| Error::Provider(format!("malformed SSE data: {e}")))?;
        map_anthropic_event(event.event.as_deref(), &v, &mut self.state)
    }

    fn on_eof(&mut self) -> Result<()> {
        if self.state.saw_stop {
            Ok(())
        } else {
            Err(Error::Provider("stream ended before message_stop".into()))
        }
    }

    fn handles_done_marker(&self) -> bool {
        false
    }
}

/// Parse the Anthropic `/v1/models` response (`{ "data": [ { "id": "..." } ] }`).
fn parse_anthropic_models(body: &Value) -> Vec<Model> {
    let mut out = Vec::new();
    let Some(arr) = body.get("data").and_then(Value::as_array) else {
        return out;
    };
    for entry in arr {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        out.push(Model {
            id: id.to_string(),
            name: id.to_string(),
            provider: String::new(),
            api: Api::AnthropicMessages,
            reasoning: false,
            supports_image: false,
            context_window: None,
            max_tokens: None,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;

    #[test]
    fn parses_anthropic_models_data_array() {
        let body = json!({
            "data": [
                {"id": "claude-opus-4"},
            ]
        });
        let models = parse_anthropic_models(&body);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-opus-4");
        assert_eq!(models[0].api, Api::AnthropicMessages);
    }

    #[test]
    fn parse_anthropic_models_missing_data_is_empty() {
        assert!(parse_anthropic_models(&json!({})).is_empty());
    }
}
