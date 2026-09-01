//! POSTs to `model.base_url` (e.g. `https://api.anthropic.com/v1/messages`)
//! with `x-api-key` + `anthropic-version` headers. Unlike the `OpenAI`
//! transports, Anthropic SSE blocks carry an `event:` line, so the mapper
//! forwards the `(event, data)` pair to [`map_anthropic_event`].

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Message, Model, StreamingEvent};

use super::{apply_headers, with_key_header, ToolSchema};
use crate::ir::{AnthropicMessagesIr, ProtocolIr};
use crate::sse::{map_sse_response, IrSseMapper};
use lofi_error::{Error, Result};

pub const ANTHROPIC_VERSION: &str = "2023-06-01";

pub(crate) struct AnthropicMessagesProvider {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) client: reqwest::Client,
    pub(crate) stream_idle_timeout: std::time::Duration,
}

#[async_trait]
impl super::Provider for AnthropicMessagesProvider {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = AnthropicMessagesIr::build_request(model, messages, tools);
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
        Ok(map_sse_response(
            resp,
            IrSseMapper::<AnthropicMessagesIr>::default(),
            self.stream_idle_timeout,
        ))
    }
}
