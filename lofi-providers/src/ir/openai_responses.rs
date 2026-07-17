//! `OpenAI` Responses request-body builder + SSE-event mapper.
//!
//! The Responses API streams typed events keyed by a `type` field inside the
//! JSON `data:` payload (e.g. `response.output_text.delta`,
//! `response.function_call_arguments.delta`, `response.completed`). This module
//! maps those to [`StreamingEvent`]s.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;

use lofi_types::{Message, Model, StreamingEvent, ThinkingLevel, Usage};
use serde_json::{json, Value};

use super::block::to_openai_responses_input;
use super::chat::ToolSchema;
use lofi_error::{Error, Result};

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
    if let Some(mt) = model.max_tokens {
        req["max_output_tokens"] = json!(mt);
    }
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
    if let Some(effort) = openai_effort(model.thinking) {
        // The Responses API nests effort under `reasoning`.
        req["reasoning"] = json!({ "effort": effort });
    }
    req
}

/// Map a thinking level to an `OpenAI` `reasoning.effort` value. `Off` returns
/// `None` (field omitted); `XHigh` clamps to `high`.
fn openai_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::XHigh => Some("high"),
    }
}

/// State accumulated across Responses events for tool-call correlation.
///
/// Tool *start*/*end* events carry `call_id`, but argument *delta* events
/// carry the distinct output-item `item_id`. We record `item.id -> call_id`
/// at `response.output_item.added` and translate subsequent `item_id` deltas
/// through that map, so interleaved function calls don't have their arguments
/// appended to the wrong tool.
#[derive(Default, Debug, Clone)]
pub struct ResponsesMapperState {
    item_to_call: HashMap<String, String>,
    pub(crate) saw_completed: bool,
}

/// Map a single Responses stream event payload to zero or more
/// [`StreamingEvent`]s.
///
/// Recognized types: `response.output_item.added` (tool-use start),
/// `response.function_call_arguments.delta` (tool input), `response.output_item.done`
/// (tool-use end), `response.output_text.delta` (text), and `response.completed`
/// (terminal usage). `response.failed`/`response.incomplete`/`error` become
/// errors. Other event types are ignored.
///
/// # Errors
///
/// Returns [`Error::Provider`] for `response.failed`, `response.incomplete`,
/// or `error` events so a provider-reported failure fails the round trip.
pub fn map_openai_responses_event(
    v: &Value,
    state: &mut ResponsesMapperState,
) -> Result<Vec<StreamingEvent>> {
    let mut out = Vec::new();
    let Some(ty) = v.get("type").and_then(Value::as_str) else {
        return Ok(out);
    };
    match ty {
        "response.output_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.push(StreamingEvent::TextDelta(delta.to_string()));
                }
            }
        }
        // Reasoning summaries (o-series / GPT-5 with `reasoning.summary` set)
        // arrive as their own delta stream; route them to a thinking block.
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    out.push(StreamingEvent::ThinkingDelta(delta.to_string()));
                }
            }
        }
        "response.output_item.added" => {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let item_id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !item_id.is_empty() && !call_id.is_empty() {
                        state.item_to_call.insert(item_id, call_id.clone());
                    }
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !name.is_empty() && !call_id.is_empty() {
                        out.push(StreamingEvent::ToolUseStart { id: call_id, name });
                    }
                }
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = v
                .get("item_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    // Translate the output-item id to the call id so
                    // `assemble_message` correlates args with the right call.
                    let id = state.item_to_call.get(&item_id).cloned().unwrap_or(item_id);
                    out.push(StreamingEvent::ToolUseInputDelta {
                        id,
                        delta: delta.to_string(),
                    });
                }
            }
        }
        "response.output_item.done" => {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if !id.is_empty() {
                        out.push(StreamingEvent::ToolUseEnd { id });
                    }
                }
            }
        }
        "response.completed" => {
            state.saw_completed = true;
            let usage = v
                .get("response")
                .and_then(|r| r.get("usage"))
                .or_else(|| v.get("usage"));
            out.push(StreamingEvent::Done(
                usage.map(usage_from_openai_responses).unwrap_or_default(),
            ));
        }
        "response.failed" | "response.incomplete" | "error" => {
            return Err(Error::Provider(format!("provider stream error: {v}")));
        }
        _ => {}
    }
    Ok(out)
}

/// Extract [`Usage`] from a Responses `usage` object.
fn usage_from_openai_responses(v: &Value) -> Usage {
    // Responses reports `input_tokens` as the full prompt (cached + non-cached)
    // and the cached slice in `input_tokens_details`. Store the non-cached
    // portion in `input_tokens` so `input + cache_read` is the prompt size.
    let prompt = v
        .get("input_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cached = v
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: v
            .get("output_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: cached,
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
            thinking: lofi_types::ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: None,
            base_url: None,
            input_price: None,
            output_price: None,
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
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![StreamingEvent::TextDelta("hi".to_string())]
        );
    }

    #[test]
    fn maps_tool_use_lifecycle() {
        let mut state = ResponsesMapperState::default();
        let added = json!({
            "type":"response.output_item.added",
            "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
        });
        assert_eq!(
            map_openai_responses_event(&added, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string()
            }]
        );
        // Argument deltas carry `item_id` (the output-item id), not `call_id`;
        // the mapper must translate `fc_1` -> `call_1`.
        let d = json!({
            "type":"response.function_call_arguments.delta",
            "item_id":"fc_1",
            "delta":"{\"x\":1}"
        });
        assert_eq!(
            map_openai_responses_event(&d, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseInputDelta {
                id: "call_1".to_string(),
                delta: "{\"x\":1}".to_string()
            }]
        );
        let done = json!({
            "type":"response.output_item.done",
            "item":{"type":"function_call","call_id":"call_1","name":"exec","arguments":"{\"x\":1}"}
        });
        assert_eq!(
            map_openai_responses_event(&done, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseEnd {
                id: "call_1".to_string()
            }]
        );
    }

    #[test]
    fn failed_event_is_error() {
        let ev = json!({"type":"response.failed","error":{"message":"boom"}});
        let r = map_openai_responses_event(&ev, &mut ResponsesMapperState::default());
        assert!(r.is_err());
    }

    #[test]
    fn maps_completed_usage() {
        let ev = json!({
            "type":"response.completed",
            "response":{"usage":{"input_tokens":3,"output_tokens":7,"input_tokens_details":{"cached_tokens":1}}}
        });
        let done = map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap();
        let StreamingEvent::Done(u) = done.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        // input_tokens excludes the cached slice: input_tokens(3) - cached(1)
        assert_eq!(u.input_tokens, 2);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, 1);
    }
}
