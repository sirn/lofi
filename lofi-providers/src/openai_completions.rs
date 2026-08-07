use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_types::{Message, Model, StreamingEvent};

use super::{apply_headers, with_bearer, ToolSchema};
use crate::ir::{OpenAiCompletionsIr, ProtocolIr};
use crate::sse::{map_sse_response, IrSseMapper};
use lofi_error::{Error, Result};

pub(crate) struct OpenAiCompletionsProvider {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
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
        let body = OpenAiCompletionsIr::build_request(model, messages, tools);
        {
            let has_img = body.to_string().contains("\"image_url\"");
            let m = format!("CHAT_PROVIDER_HAS_FIX has_image={has_img}");
            match std::fs::write("/tmp/LOFI_CHAT_MARKER.json", &m) {
                Ok(()) => eprintln!("[lofi-chat-marker] {m}"),
                Err(e) => eprintln!("[lofi-chat-marker] WRITE FAILED {e}"),
            }
        }
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
            IrSseMapper::<OpenAiCompletionsIr>::default(),
        ))
    }
}
