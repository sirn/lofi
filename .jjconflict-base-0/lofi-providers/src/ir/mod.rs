use lofi_error::Result;
use lofi_types::{Message, Model, StreamingEvent};
use serde_json::Value;

use crate::ToolSchema;

pub(crate) mod anthropic_messages;
pub(crate) mod openai_completions;
pub(crate) mod openai_responses;

pub(crate) use anthropic_messages::AnthropicMessagesIr;
pub(crate) use openai_completions::OpenAiCompletionsIr;
pub(crate) use openai_responses::OpenAiResponsesIr;

/// Base64-encodes an image payload for the wire. All three protocols take
/// image bytes base64-encoded; the in-memory block stores raw bytes, so the
/// encoding happens here at the IR boundary rather than on the block itself.
pub(crate) fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Converts provider-neutral messages to one protocol's wire request and
/// maps that protocol's streaming payloads back to provider-neutral events.
pub(crate) trait ProtocolIr: Send + 'static {
    type State: Default + Send + 'static;

    fn build_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value;

    fn map_event(
        event: Option<&str>,
        data: &Value,
        state: &mut Self::State,
    ) -> Result<Vec<StreamingEvent>>;

    fn on_eof(_state: &Self::State) -> Result<()> {
        Ok(())
    }

    fn handles_done_marker() -> bool {
        true
    }

    fn defer_done_until_transport_end() -> bool {
        false
    }
}
