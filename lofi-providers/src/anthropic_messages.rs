//! Anthropic Messages HTTP transport.
//!
//! POSTs to `model.base_url` (e.g. `https://api.anthropic.com/v1/messages`)
//! with `x-api-key` + `anthropic-version` headers. Unlike the `OpenAI`
//! transports, Anthropic SSE blocks carry an `event:` line, so the mapper
//! forwards the `(event, data)` pair to [`map_anthropic_event`].

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
///
/// POSTs to `model.base_url` verbatim; see
/// [`super::openai_completions::OpenAiCompletionsProvider`] for the URL
/// resolution contract.
pub(crate) struct AnthropicMessagesProvider {
    /// Provider host root, used as the fallback when a model does not carry
    /// its own `base_url`.
    pub(crate) base_url: String,
    /// Resolved `x-api-key` value.
    pub(crate) api_key: String,
    /// Extra resolved headers from config.
    pub(crate) headers: HashMap<String, String>,
    /// Shared HTTP client.
    pub(crate) client: reqwest::Client,
}

#[async_trait]
impl super::Provider for AnthropicMessagesProvider {
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
                    .post(model.base_url.as_deref().unwrap_or(&self.base_url))
                    .json(&body),
                "x-api-key",
                &self.api_key,
            )
            .header("anthropic-version", ANTHROPIC_VERSION),
            &self.headers,
        );
        let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
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
