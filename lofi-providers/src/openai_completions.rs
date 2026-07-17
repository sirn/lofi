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

#[derive(Default)]
struct OpenAiChatMapper {
    state: ChatMapperState,
}

impl SseMapper for OpenAiChatMapper {
    fn map(&mut self, event: SseEvent) -> Result<Vec<StreamingEvent>> {
        let v: Value = serde_json::from_str(&event.data)
            .map_err(|e| Error::Provider(format!("malformed SSE data: {e}")))?;
        map_openai_chat_event(&v, &mut self.state)
    }

    fn on_eof(&mut self) -> Result<()> {
        Err(Error::Provider(
            "stream ended before [DONE] sentinel".into(),
        ))
    }

    fn defer_done_until_transport_end(&self) -> bool {
        true
    }
}
