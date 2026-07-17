//! Anthropic Messages request-body builder + SSE-event mapper.
//!
//! Anthropic SSE blocks carry an `event:` line (`message_start`,
//! `content_block_start`, `content_block_delta`, `content_block_stop`,
//! `message_delta`, `message_stop`) and a JSON `data:` payload. The providers
//! layer parses the SSE stream and passes `(event, data)` here.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{Message, Model, StreamingEvent, Usage};
use serde_json::{json, Value};

use super::block::to_anthropic_request_parts;
use super::chat::ToolSchema;

/// Build the `POST /v1/messages` body for a streaming turn.
///
/// `max_tokens` defaults to `4096` when the model doesn't specify one — the
/// Anthropic API requires it.
#[must_use]
pub fn build_anthropic_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
    let (system, msgs) = to_anthropic_request_parts(messages);
    let max_tokens = model.max_tokens.unwrap_or(4096);
    let mut req = json!({
        "model": model.id,
        "messages": msgs,
        "stream": true,
        "max_tokens": max_tokens,
    });
    if let Some(sys) = system {
        req["system"] = json!(sys);
    }
    if !tools.is_empty() {
        let tools_arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();
        req["tools"] = json!(tools_arr);
    }
    req
}

/// Map a single Anthropic SSE event to a [`StreamingEvent`].
///
/// `content_block_stop` emits a `ToolUseEnd` keyed by the block `index`; for
/// non-tool blocks this is a no-op in the accumulator (it closes nothing when
/// no tool is in flight). Tool-input deltas use the block `index` as their id
/// because Anthropic doesn't repeat the tool id on each delta — the accumulator
/// falls back to the most recent unfinished tool.
#[must_use]
pub fn map_anthropic_event(event: Option<&str>, data: &Value) -> Option<StreamingEvent> {
    let ty = event?;
    match ty {
        "content_block_start" => {
            let block = data.get("content_block")?;
            let bt = block.get("type").and_then(serde_json::Value::as_str)?;
            if bt == "tool_use" {
                let id = block
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = block
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_string();
                return Some(StreamingEvent::ToolUseStart { id, name });
            }
            None
        }
        "content_block_delta" => {
            let delta = data.get("delta")?;
            let dt = delta.get("type").and_then(serde_json::Value::as_str)?;
            match dt {
                "text_delta" => {
                    let text = delta.get("text").and_then(serde_json::Value::as_str)?;
                    Some(StreamingEvent::TextDelta(text.to_string()))
                }
                "input_json_delta" => {
                    let idx = data
                        .get("index")
                        .and_then(serde_json::Value::as_u64)
                        .map_or_else(|| "0".to_string(), |n| n.to_string());
                    let d = delta
                        .get("partial_json")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    Some(StreamingEvent::ToolUseInputDelta {
                        id: idx,
                        delta: d.to_string(),
                    })
                }
                "thinking_delta" => {
                    let text = delta.get("thinking").and_then(serde_json::Value::as_str)?;
                    Some(StreamingEvent::ThinkingDelta(text.to_string()))
                }
                _ => None,
            }
        }
        "content_block_stop" => {
            let idx = data
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map_or_else(|| "0".to_string(), |n| n.to_string());
            Some(StreamingEvent::ToolUseEnd { id: idx })
        }
        "message_delta" => data
            .get("usage")
            .map(usage_from_anthropic)
            .map(StreamingEvent::Done),
        _ => None,
    }
}

/// Extract [`Usage`] from an Anthropic `usage` object.
fn usage_from_anthropic(v: &Value) -> Usage {
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
            .get("cache_read_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_write_tokens: v
            .get("cache_creation_input_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofi_types::Role;
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "claude".to_string(),
            name: "claude".to_string(),
            provider: "p".to_string(),
            api: lofi_types::Api::AnthropicMessages,
            reasoning: false,
            supports_image: false,
            context_window: None,
            max_tokens: Some(1024),
        }
    }

    #[test]
    fn request_pulls_system_to_top_level() {
        let msgs = [
            Message {
                role: Role::System,
                blocks: vec![lofi_types::ContentBlock::Text {
                    text: "sys".to_string(),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![lofi_types::ContentBlock::Text {
                    text: "hi".to_string(),
                }],
            },
        ];
        let req = build_anthropic_request(&model(), &msgs, &[]);
        assert_eq!(req["model"], "claude");
        assert_eq!(req["system"], "sys");
        assert_eq!(req["max_tokens"], 1024);
        assert_eq!(req["messages"][0]["role"], "user");
    }

    #[test]
    fn maps_text_delta() {
        let data = json!({"delta":{"type":"text_delta","text":"hi"}});
        assert_eq!(
            map_anthropic_event(Some("content_block_delta"), &data),
            Some(StreamingEvent::TextDelta("hi".to_string()))
        );
    }

    #[test]
    fn maps_tool_use_lifecycle() {
        let start = json!({"content_block":{"type":"tool_use","id":"tu_0","name":"exec"}});
        assert_eq!(
            map_anthropic_event(Some("content_block_start"), &start),
            Some(StreamingEvent::ToolUseStart {
                id: "tu_0".to_string(),
                name: "exec".to_string()
            })
        );
        let delta = json!({
            "index": 0,
            "delta": {"type":"input_json_delta","partial_json":"{\"a\":"}
        });
        assert_eq!(
            map_anthropic_event(Some("content_block_delta"), &delta),
            Some(StreamingEvent::ToolUseInputDelta {
                id: "0".to_string(),
                delta: "{\"a\":".to_string()
            })
        );
        let stop = json!({"index": 0});
        assert_eq!(
            map_anthropic_event(Some("content_block_stop"), &stop),
            Some(StreamingEvent::ToolUseEnd {
                id: "0".to_string()
            })
        );
    }

    #[test]
    fn maps_thinking_delta() {
        let data = json!({"delta":{"type":"thinking_delta","thinking":"hmm"}});
        assert_eq!(
            map_anthropic_event(Some("content_block_delta"), &data),
            Some(StreamingEvent::ThinkingDelta("hmm".to_string()))
        );
    }

    #[test]
    fn maps_message_delta_usage() {
        let data = json!({"usage":{"input_tokens":4,"output_tokens":9,"cache_read_input_tokens":1,"cache_creation_input_tokens":2}});
        let done = map_anthropic_event(Some("message_delta"), &data);
        let StreamingEvent::Done(u) = done.unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(u.input_tokens, 4);
        assert_eq!(u.output_tokens, 9);
        assert_eq!(u.cache_read_tokens, 1);
        assert_eq!(u.cache_write_tokens, 2);
    }

    #[test]
    fn returns_none_for_unknown_event() {
        let data = json!({});
        assert_eq!(map_anthropic_event(Some("ping"), &data), None);
        assert_eq!(map_anthropic_event(None, &data), None);
    }
}
