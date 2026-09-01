use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Message, Model, StreamingEvent};

use super::{apply_headers, with_bearer, ToolSchema};
use crate::ir::{OpenAiResponsesIr, ProtocolIr};
use crate::sse::{map_sse_response, IrSseMapper};
use lofi_error::{Error, Result};

pub(crate) struct OpenAiResponsesProvider {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) client: reqwest::Client,
    pub(crate) stream_idle_timeout: std::time::Duration,
}

#[async_trait]
impl super::Provider for OpenAiResponsesProvider {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = OpenAiResponsesIr::build_request(model, messages, tools);
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
        Ok(map_sse_response(
            resp,
            IrSseMapper::<OpenAiResponsesIr>::default(),
            self.stream_idle_timeout,
        ))
    }
}
