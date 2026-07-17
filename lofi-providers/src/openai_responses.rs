//! `OpenAI` Responses HTTP transport.
//!
//! Mirrors [`super::openai_completions`] but POSTs to `/responses` and maps
//! events via [`map_openai_responses_event`].

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Api, Message, Model, StreamingEvent};
use serde_json::Value;

use super::{apply_headers, with_bearer};
use crate::ir::build_request;
use crate::ir::chat::ToolSchema;
use crate::ir::codec::SseEvent;
use crate::ir::openai_responses::{map_openai_responses_event, ResponsesMapperState};
use crate::sse::{map_sse_response, SseMapper};
use lofi_error::{Error, Result};

/// `OpenAI` Responses transport.
///
/// POSTs to `model.base_url` verbatim; see
/// [`super::openai_completions::OpenAiCompletionsProvider`] for the URL
/// resolution contract.
pub struct OpenAiResponsesProvider {
    /// Provider host root, used as the fallback when a model does not carry
    /// its own `base_url`.
    pub base_url: String,
    /// Resolved bearer token.
    pub api_key: String,
    /// Extra resolved headers from config.
    pub headers: HashMap<String, String>,
    /// Shared HTTP client.
    pub client: reqwest::Client,
}

#[async_trait]
impl super::Provider for OpenAiResponsesProvider {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = build_request(Api::OpenAiResponses, model, messages, tools);
        let req = apply_headers(
            with_bearer(
                self.client
                    .post(model.base_url.as_deref().unwrap_or(&self.base_url))
                    .json(&body),
                &self.api_key,
            ),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = super::ensure_ok(resp).await?;
        Ok(map_sse_response(resp, OpenAiResponsesMapper::default()))
    }
}

/// Mapper that parses each Responses `data:` JSON and forwards it to
/// [`map_openai_responses_event`].
#[derive(Default)]
struct OpenAiResponsesMapper {
    state: ResponsesMapperState,
}

impl SseMapper for OpenAiResponsesMapper {
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
        // `event.data` is already the stripped payload; parse it directly as
        // JSON rather than re-running the SSE line reader.
        let v: Value = serde_json::from_str(&event.data)
            .map_err(|e| Error::Provider(format!("malformed SSE data: {e}")))?;
        map_openai_responses_event(&v, &mut self.state)
    }

    fn on_eof(&mut self) -> Result<()> {
        if self.state.saw_completed {
            Ok(())
        } else {
            Err(Error::Provider(
                "stream ended before response.completed".into(),
            ))
        }
    }
}
