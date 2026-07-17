//! `OpenAI` Responses HTTP transport.
//!
//! Mirrors [`super::openai_completions`] but POSTs to `/responses` and maps
//! events via [`map_openai_responses_event`].

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
use crate::ir::openai_responses::map_openai_responses_event;
use crate::providers::openai_completions::parse_openai_models;
use crate::providers::sse::{map_sse_response, SseMapper};

/// `OpenAI` Responses transport.
pub struct OpenAiResponsesProvider {
    /// Base URL with no trailing slash.
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
    async fn list_models(&self) -> Result<Vec<Model>> {
        let req = apply_headers(
            self.client
                .get(format!("{}/models", self.base_url))
                .bearer_auth(&self.api_key),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = resp.error_for_status().map_err(Error::Http)?;
        let body: Value = resp.json().await.map_err(Error::Http)?;
        // The Responses API shares the OpenAI `/models` endpoint shape, but
        // discovered models speak the Responses wire protocol.
        let mut models = parse_openai_models(&body);
        for m in &mut models {
            m.api = Api::OpenAiResponses;
        }
        Ok(models)
    }

    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = build_request(Api::OpenAiResponses, model, messages, tools);
        let req = apply_headers(
            self.client
                .post(format!("{}/responses", self.base_url))
                .json(&body)
                .bearer_auth(&self.api_key),
            &self.headers,
        );
        let resp = req.send().await.map_err(Error::Http)?;
        let resp = resp.error_for_status().map_err(Error::Http)?;
        Ok(map_sse_response(resp, OpenAiResponsesMapper))
    }
}

/// Mapper that parses each Responses `data:` JSON and forwards it to
/// [`map_openai_responses_event`].
struct OpenAiResponsesMapper;

impl SseMapper for OpenAiResponsesMapper {
    fn map(&mut self, event: SseEvent) -> Option<StreamingEvent> {
        // `event.data` is already the stripped payload; parse it directly as
        // JSON rather than re-running the SSE line reader.
        let v: Value = serde_json::from_str(&event.data).ok()?;
        map_openai_responses_event(&v)
    }
}
