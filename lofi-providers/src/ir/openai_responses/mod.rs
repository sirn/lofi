use lofi_types::{Message, Model, StreamingEvent};
use serde_json::Value;

use lofi_error::{Error, Result};

use super::ProtocolIr;
use crate::ToolSchema;
use events::{map_openai_responses_event, ResponsesMapperState};
use request::build_openai_responses_request;

mod events;
mod request;
#[cfg(test)]
mod tests;

pub(crate) struct OpenAiResponsesIr;

impl ProtocolIr for OpenAiResponsesIr {
    type State = ResponsesMapperState;

    fn build_request_inner<'a>(
        model: &Model,
        messages: impl Iterator<Item = &'a Message>,
        tools: &[ToolSchema],
    ) -> Value {
        build_openai_responses_request(model, messages, tools)
    }

    fn map_event(
        _event: Option<&str>,
        data: &Value,
        state: &mut Self::State,
    ) -> Result<Vec<StreamingEvent>> {
        map_openai_responses_event(data, state)
    }

    fn on_eof(state: &Self::State) -> Result<()> {
        if state.saw_completed {
            Ok(())
        } else {
            Err(Error::Provider(
                "stream ended before response.completed".into(),
            ))
        }
    }

    fn defer_done_until_transport_end() -> bool {
        true
    }
}
