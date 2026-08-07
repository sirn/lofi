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
        if let Ok(path) = std::env::var("LOFI_DUMP_REQUEST") {
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap_or_default());
        }
        // Unconditional marker dump: proves this build's stream() runs and
        // records whether any image rides the request. /tmp is shared.
        {
            let has_img = body.to_string().contains("input_image");
            let _ = std::fs::write(
                "/tmp/LOFI_STREAM_MARKER_ZZ.json",
                format!("BUILD_WITH_DUMP_CODE has_image={has_img}"),
            );
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
            IrSseMapper::<OpenAiResponsesIr>::default(),
        ))
    }
}
