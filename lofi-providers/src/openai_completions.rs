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

use super::{apply_headers, with_bearer};
use crate::ir::build_request;
use crate::ir::chat::ToolSchema;
use crate::ir::codec::SseEvent;
use crate::ir::openai_completions::{map_openai_chat_event, ChatMapperState};
use crate::sse::{map_sse_response, SseMapper};
use lofi_error::{Error, Result};

/// `OpenAI` Chat Completions transport.
///
/// The provider POSTs to `model.base_url` verbatim — the full endpoint URL
/// (e.g. `https://api.openai.com/v1/chat/completions`) is resolved at config
/// load time from the provider's `base_url` joined with the api-type mapping's
/// `path`. No path suffix is appended here.
pub(crate) struct OpenAiCompletionsProvider {
    /// Provider host root, used as the fallback when a model does not carry
    /// its own `base_url`.
    pub(crate) base_url: String,
    /// Resolved bearer token.
    pub(crate) api_key: String,
    /// Extra resolved headers from config.
    pub(crate) headers: HashMap<String, String>,
    /// Shared HTTP client.
    pub(crate) client: reqwest::Client,
}

#[async_trait]
impl super::Provider for OpenAiCompletionsProvider {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = build_request(Api::OpenAiCompletions, model, messages, tools);
        let req = apply_headers(
            with_bearer(
                self.client
                    .post(model.base_url.as_deref().unwrap_or(&self.base_url))
                    .json(&body),
                &self.api_key,
            ),
            &self.headers,
        );
        let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let resp = super::ensure_ok(resp).await?;
        Ok(map_sse_response(resp, OpenAiChatMapper::default()))
    }
}

/// Mapper that parses each Chat Completions `data:` JSON and forwards it to
/// [`map_openai_chat_event`].
#[derive(Default)]
struct OpenAiChatMapper {
    state: ChatMapperState,
}

impl SseMapper for OpenAiChatMapper {
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
        // Chat Completions has no `event:` field; only the data payload is
        // interesting. `event.data` is already the stripped payload, so parse
        // it directly as JSON (re-running `parse_sse_lines` here would find
        // no `data:` prefixes and drop every event).
        let v: Value = serde_json::from_str(&event.data)
            .map_err(|e| Error::Provider(format!("malformed SSE data: {e}")))?;
        map_openai_chat_event(&v, &mut self.state)
    }

    fn on_eof(&mut self) -> Result<()> {
        // Reached only when no `data: [DONE]` sentinel was seen (the decoder
        // sets `done` itself on `[DONE]`, skipping `on_eof`). A disconnect
        // before the sentinel is an error, not a silent partial turn.
        Err(Error::Provider(
            "stream ended before [DONE] sentinel".into(),
        ))
    }
}
