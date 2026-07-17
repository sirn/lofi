//! `OpenAI` Responses request-body builder + SSE-event mapper.
//!
//! The Responses API streams typed events keyed by a `type` field inside the
//! JSON `data:` payload (e.g. `response.output_text.delta`,
//! `response.function_call_arguments.delta`, `response.completed`). This module
//! maps those to [`StreamingEvent`]s.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{Message, Model, StreamingEvent, Usage};
use serde_json::{json, Value};

use super::block::to_openai_responses_input;
use super::chat::ToolSchema;

/// Build the `POST /responses` body for a streaming turn.
#[must_use]
pub fn build_openai_responses_request(
    model: &Model,
    messages: &[Message],
    tools: &[ToolSchema],
) -> Value {
    let input = to_openai_responses_input(messages);
    let mut req = json!({
        "model": model.id,
        "input": input,
        "stream": true,
    });
    if !tools.is_empty() {
        let tools_arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        req["tools"] = json!(tools_arr);
    }
    req
}

/// Map a single Responses stream event payload to a [`StreamingEvent`].
///
/// Recognized types: `response.output_item.added` (tool-use start),
/// `response.function_call_arguments.delta` (tool input), `response.output_item.done`
/// (tool-use end), `response.output_text.delta` (text), and `response.completed`
/// (terminal usage). Other event types are ignored.
#[must_use]
pub fn map_openai_responses_event(v: &Value) -> Option<StreamingEvent> {
    let ty = v.get("type")?.as_str()?;
    match ty {
        "response.output_text.delta" => {
            let delta = v.get("delta").and_then(serde_json::Value::as_str)?;
            if delta.is_empty() {
                return None;
            }
            Some(StreamingEvent::TextDelta(delta.to_string()))
        }
        "response.output_item.added" => {
            let item = v.get("item")?;
            if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call") {
                let id = item
                    .get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    return Some(StreamingEvent::ToolUseStart { id, name });
                }
            }
            None
        }
        "response.function_call_arguments.delta" => {
            let item_id = v
                .get("item_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let delta = v.get("delta").and_then(serde_json::Value::as_str)?;
            Some(StreamingEvent::ToolUseInputDelta {
                id: item_id,
                delta: delta.to_string(),
            })
        }
        "response.output_item.done" => {
            let item = v.get("item")?;
            if item.get("type").and_then(serde_json::Value::as_str) == Some("function_call") {
                let id = item
                    .get("call_id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                return Some(StreamingEvent::ToolUseEnd { id });
            }
            None
        }
        "response.completed" => {
            let usage = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .or_else(|| v.get("usage"))?;
            Some(StreamingEvent::Done(usage_from_openai_responses(usage)))
        }
        _ => None,
    }
}

/// Extract [`Usage`] from a Responses `usage` object.
fn usage_from_openai_responses(v: &Value) -> Usage {
    Usage {
        input_tokens: v
            .get("input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output_tokens: v
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: v
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "gpt-4o".to_string(),
            name: "gpt".to_string(),
            provider: "p".to_string(),
            api: lofi_types::Api::OpenAiResponses,
            reasoning: false,
            supports_image: false,
            context_window: None,
            max_tokens: None,
        }
    }

    #[test]
    fn request_uses_input_array() {
        let req = build_openai_responses_request(&model(), &[], &[]);
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], true);
        assert!(req.get("input").is_some());
        assert!(req.get("tools").is_none());
    }

    #[test]
    fn maps_text_delta() {
        let ev = json!({"type":"response.output_text.delta","delta":"hi"});
        assert_eq!(
            map_openai_responses_event(&ev),
            Some(StreamingEvent::TextDelta("hi".to_string()))
        );
    }

    #[test]
    fn maps_tool_use_lifecycle() {
        let added = json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","call_id":"call_1","name":"exec","arguments":""}
        });
        assert_eq!(
            map_openai_responses_event(&added),
            Some(StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string()
            })
        );
        let d = json!({
            "type":"response.function_call_arguments.delta",
            "item_id":"fc_1",
            "delta":"{\"x\":1}"
        });
        assert_eq!(
            map_openai_responses_event(&d),
            Some(StreamingEvent::ToolUseInputDelta {
                id: "fc_1".to_string(),
                delta: "{\"x\":1}".to_string()
            })
        );
        let done = json!({
            "type":"response.output_item.done",
            "item":{"type":"function_call","call_id":"call_1","name":"exec","arguments":"{\"x\":1}"}
        });
        assert_eq!(
            map_openai_responses_event(&done),
            Some(StreamingEvent::ToolUseEnd {
                id: "call_1".to_string()
            })
        );
    }

    #[test]
    fn maps_completed_usage() {
        let ev = json!({
            "type":"response.completed",
            "response":{"usage":{"input_tokens":3,"output_tokens":7,"input_tokens_details":{"cached_tokens":1}}}
        });
        let done = map_openai_responses_event(&ev);
        let StreamingEvent::Done(u) = done.unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(u.input_tokens, 3);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, 1);
    }
}
