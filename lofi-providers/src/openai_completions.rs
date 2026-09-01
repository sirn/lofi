use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Message, Model, StreamingEvent};

use super::{apply_headers, send_stream_request, with_bearer, ToolSchema};
use crate::ir::{OpenAiCompletionsIr, ProtocolIr};
use crate::sse::{map_sse_response, IrSseMapper};
use lofi_error::Result;

pub(crate) struct OpenAiCompletionsProvider {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) client: reqwest::Client,
    pub(crate) response_start_timeout: std::time::Duration,
}

#[async_trait]
impl super::Provider for OpenAiCompletionsProvider {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = OpenAiCompletionsIr::build_request(model, messages, tools);
        let req = apply_headers(
            with_bearer(
                self.client
                    .post(model.base_url.as_deref().unwrap_or(&self.base_url))
                    .json(&body),
                &self.api_key,
            ),
            &self.headers,
        );
        let resp = send_stream_request(req, self.response_start_timeout).await?;
        Ok(map_sse_response(
            resp,
            IrSseMapper::<OpenAiCompletionsIr>::default(),
        ))
    }
}
