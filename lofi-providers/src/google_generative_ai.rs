//! Native Google Generative AI streaming transport.

use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_error::Result;
use lofi_types::{Message, Model, StreamingEvent};

use super::{apply_headers, send_stream_request, with_key_header, ToolSchema};
use crate::ir::{GoogleGenerativeAiIr, ProtocolIr};
use crate::sse::{map_sse_response, IrSseMapper};

pub(crate) struct GoogleGenerativeAiProvider {
    pub(crate) base_url: String,
    pub(crate) api_key: String,
    pub(crate) headers: HashMap<String, String>,
    pub(crate) client: reqwest::Client,
    pub(crate) response_start_timeout: std::time::Duration,
}

#[async_trait]
impl super::Provider for GoogleGenerativeAiProvider {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>> {
        let body = GoogleGenerativeAiIr::build_request(model, messages, tools);
        let base = model.base_url.as_deref().unwrap_or(&self.base_url);
        let url = stream_url(base, &model.id);
        let request = apply_headers(
            with_key_header(
                self.client.post(url).json(&body),
                "x-goog-api-key",
                &self.api_key,
            ),
            &self.headers,
        );
        let response = send_stream_request(request, self.response_start_timeout).await?;
        Ok(map_sse_response(
            response,
            IrSseMapper::<GoogleGenerativeAiIr>::new(model),
        ))
    }
}

fn stream_url(base: &str, model_id: &str) -> String {
    if base.contains(":streamGenerateContent") {
        return base.to_string();
    }
    let model_id = model_id.strip_prefix("models/").unwrap_or(model_id);
    format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        base.trim_end_matches('/'),
        model_id
    )
}

#[cfg(test)]
mod tests {
    use super::stream_url;

    #[test]
    fn builds_native_stream_url() {
        assert_eq!(
            stream_url("https://generativelanguage.googleapis.com/v1beta/", "gemini-3-flash"),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-flash:streamGenerateContent?alt=sse"
        );
    }
}
