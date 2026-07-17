//! `OpenAI` Chat Completions HTTP transport.
//!
//! Thin layer over [`crate::ir`]: the request body comes from
//! [`build_request`](crate::ir::chat::build_request), and each SSE `data:`
//! JSON is mapped by [`map_openai_chat_event`]. SSE byte-stream decoding is
//! shared via [`super::sse`].

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Api, Message, Model, StreamingEvent};
use serde_json::Value;

use super::apply_headers;
use crate::error::{Error, Result};
use crate::ir::build_request;
use crate::ir::chat::ToolSchema;
use crate::ir::codec::SseEvent;
use crate::ir::openai_completions::map_openai_chat_event;
use crate::providers::sse::{map_sse_response, SseMapper};

/// `OpenAI` Chat Completions transport.
pub struct OpenAiCompletionsProvider {
    /// Base URL with no trailing slash (e.g. `https://api.openai.com/v1`).
    pub base_url: String,
    /// Resolved bearer token.
    pub api_key: String,
    /// Extra resolved headers from config.
    pub headers: HashMap<String, String>,
    /// Shared HTTP client.
    pub client: reqwest::Client,
}

#[async_trait]
impl super::Provider for OpenAiCompletionsProvider {
    async fn list_models(&self) -> Result<Vec<Model>> {
        let req = apply_headers(
            self.client
                .get(format!("{}/models", self.base_url))
                .bearer_auth(&self.api_key),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = super::ensure_ok(resp).await?;
        let body: Value = resp.json().await.map_err(Error::Http)?;
        Ok(parse_openai_models(&body))
    }

    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = build_request(Api::OpenAiCompletions, model, messages, tools);
        let req = apply_headers(
            self.client
                .post(format!("{}/chat/completions", self.base_url))
                .json(&body)
                .bearer_auth(&self.api_key),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = super::ensure_ok(resp).await?;
        Ok(map_sse_response(resp, OpenAiChatMapper))
    }
}

/// Mapper that parses each Chat Completions `data:` JSON and forwards it to
/// [`map_openai_chat_event`].
struct OpenAiChatMapper;

impl SseMapper for OpenAiChatMapper {
    fn map(&mut self, event: SseEvent) -> Option<StreamingEvent> {
        // Chat Completions has no `event:` field; only the data payload is
        // interesting. `event.data` is already the stripped payload, so parse
        // it directly as JSON (re-running `parse_sse_lines` here would find
        // no `data:` prefixes and drop every event).
        let v: Value = serde_json::from_str(&event.data).ok()?;
        map_openai_chat_event(&v)
    }
}

/// Parse the `OpenAI` `/models` response (`{ "data": [ { "id": "..." } ] }`).
pub(crate) fn parse_openai_models(body: &Value) -> Vec<Model> {
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
            api: Api::OpenAiCompletions,
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
    fn parses_openai_models_data_array() {
        let body = json!({
            "data": [
                {"id": "gpt-4o"},
                {"id": "gpt-4o-mini"},
            ]
        });
        let models = parse_openai_models(&body);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4o");
        assert_eq!(models[0].api, Api::OpenAiCompletions);
        assert_eq!(models[1].name, "gpt-4o-mini");
    }

    #[test]
    fn parse_openai_models_missing_data_is_empty() {
        assert!(parse_openai_models(&json!({})).is_empty());
        assert!(parse_openai_models(&json!({"data": "oops"})).is_empty());
    }
}
