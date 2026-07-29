use lofi_error::Result;
use lofi_types::{Message, Model, StreamingEvent};
use serde_json::Value;

use crate::ToolSchema;

pub mod anthropic_messages;
pub mod openai_completions;
pub mod openai_responses;

pub use anthropic_messages::AnthropicMessagesIr;
pub use openai_completions::OpenAiCompletionsIr;
pub use openai_responses::OpenAiResponsesIr;

/// Converts provider-neutral messages to one protocol's wire request and
/// maps that protocol's streaming payloads back to provider-neutral events.
pub trait ProtocolIr: Send + 'static {
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
