//! `build_request` dispatch by [`Api`] to the per-provider request builders.
//!
//! [`ToolSchema`] is the minimal tool-description shape the `ir` layer needs:
//! the agent loop (later step) builds the `exec` tool schema and passes it in
//! here. Keeping it local avoids duplicating `lofi-types` data and keeps the
//! `ir` layer self-contained.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{Api, Message, Model};
use serde::Serialize;
use serde_json::Value;

use super::anthropic_messages::build_anthropic_request;
use super::openai_completions::build_openai_chat_request;
use super::openai_responses::build_openai_responses_request;

/// A tool advertised to the model.
///
/// `input_schema` is a JSON Schema object describing the tool's arguments; the
/// agent builds this for the `exec` tool and any future tools.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSchema {
    /// Tool name as seen by the model.
    pub name: String,
    /// Human-readable description.
    pub description: String,
    /// JSON Schema describing the tool's input object.
    pub input_schema: Value,
}

/// Build a streaming request body for `api` carrying `messages` and `tools`.
///
/// Dispatches to the per-provider builder; each builder sets `stream: true`
/// and any provider-specific streaming flags.
#[must_use]
pub fn build_request(api: Api, model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
    match api {
        Api::OpenAiCompletions => build_openai_chat_request(model, messages, tools),
        Api::OpenAiResponses => build_openai_responses_request(model, messages, tools),
        Api::AnthropicMessages => build_anthropic_request(model, messages, tools),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofi_types::Role;
    use serde_json::json;

    fn model(api: Api) -> Model {
        Model {
            id: "m".to_string(),
            name: "m".to_string(),
            provider: "p".to_string(),
            api,
            reasoning: false,
            thinking: lofi_types::ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: None,
            base_url: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        }
    }

    fn msgs() -> Vec<Message> {
        vec![Message {
            role: Role::User,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "hi".to_string(),
            }],
        }]
    }

    #[test]
    fn dispatch_openai_chat() {
        let req = build_request(
            Api::OpenAiCompletions,
            &model(Api::OpenAiCompletions),
            &msgs(),
            &[],
        );
        assert_eq!(req["model"], "m");
        assert_eq!(req["stream"], true);
        assert_eq!(req["stream_options"]["include_usage"], true);
        assert!(req.get("messages").is_some());
    }

    #[test]
    fn dispatch_openai_responses() {
        let req = build_request(
            Api::OpenAiResponses,
            &model(Api::OpenAiResponses),
            &msgs(),
            &[],
        );
        assert_eq!(req["model"], "m");
        assert_eq!(req["stream"], true);
        assert!(req.get("input").is_some());
    }

    #[test]
    fn dispatch_anthropic() {
        let req = build_request(
            Api::AnthropicMessages,
            &model(Api::AnthropicMessages),
            &msgs(),
            &[],
        );
        assert_eq!(req["model"], "m");
        assert_eq!(req["stream"], true);
        assert_eq!(req["max_tokens"], 4096);
    }

    #[test]
    fn tool_schema_serializes() {
        let t = ToolSchema {
            name: "exec".to_string(),
            description: "run code".to_string(),
            input_schema: json!({"type": "object"}),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert_eq!(v["name"], "exec");
        assert_eq!(v["input_schema"]["type"], "object");
    }
}
