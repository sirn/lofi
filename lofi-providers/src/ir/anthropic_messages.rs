//! Anthropic Messages request-body builder + SSE-event mapper.
//!
//! Anthropic SSE blocks carry an `event:` line (`message_start`,
//! `content_block_start`, `content_block_delta`, `content_block_stop`,
//! `message_delta`, `message_stop`) and a JSON `data:` payload. The providers
//! layer parses the SSE stream and passes `(event, data)` here.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;

use lofi_types::{Message, Model, StreamingEvent, ThinkingLevel, Usage};
use serde_json::{json, Value};

use super::block::to_anthropic_request_parts;
use super::chat::ToolSchema;
use lofi_error::{Error, Result};

/// Build the `POST /v1/messages` body for a streaming turn.
///
/// `max_tokens` defaults to `4096` when the model doesn't specify one — the
/// Anthropic API requires it.
#[must_use]
pub fn build_anthropic_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
    let (system, msgs) = to_anthropic_request_parts(messages);
    // Anthropic requires `max_tokens` and requires it to exceed the thinking
    // budget when thinking is enabled. Default to 4096, then bump to leave
    // room for the reasoning budget + a response allowance.
    let mut max_tokens = model.max_tokens.unwrap_or(4096);
    let mut req = json!({
        "model": model.id,
        "messages": msgs,
        "stream": true,
        "max_tokens": max_tokens,
    });
    if let Some(budget) = anthropic_budget(model.thinking) {
        // Reserve `budget` tokens for thinking and at least 2048 for the
        // visible response so the API doesn't reject the request.
        max_tokens = max_tokens.max(budget + 2048);
        req["max_tokens"] = json!(max_tokens);
        req["thinking"] = json!({ "type": "enabled", "budget_tokens": budget });
    }
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

/// Map a thinking level to an Anthropic `thinking.budget_tokens` value.
/// `Off` returns `None` (thinking disabled — the field is omitted); the other
/// levels scale the reasoning budget the API allocates.
fn anthropic_budget(level: ThinkingLevel) -> Option<u64> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some(1024),
        ThinkingLevel::Medium => Some(4096),
        ThinkingLevel::High => Some(10_000),
        ThinkingLevel::XHigh => Some(32_000),
    }
}

/// State accumulated across Anthropic events.
///
/// Tool-use blocks are correlated by the block `index`; the real tool `id`
/// arrives in `content_block_start`, so we map `index -> id` for later
/// `input_json_delta` and `content_block_stop` events. Usage is assembled
/// across `message_start` (input/cache) and `message_delta` (cumulative
/// output) and emitted once at `message_stop`.
#[derive(Default, Debug, Clone)]
pub struct AnthropicMapperState {
    index_to_id: HashMap<u64, String>,
    usage: Option<Usage>,
    pub(crate) saw_stop: bool,
}

/// Map a single Anthropic SSE event to zero or more [`StreamingEvent`]s.
///
/// # Errors
///
/// Returns [`Error::Provider`] for an `error` event so a provider-reported
/// failure fails the round trip.
pub fn map_anthropic_event(
    event: Option<&str>,
    data: &Value,
    state: &mut AnthropicMapperState,
) -> Result<Vec<StreamingEvent>> {
    let mut out = Vec::new();
    let Some(ty) = event else {
        return Ok(out);
    };
    match ty {
        "message_start" => {
            if let Some(u) = data.get("message").and_then(|m| m.get("usage")) {
                state.usage = Some(usage_from_anthropic(u));
            }
        }
        "content_block_start" => {
            let block = data.get("content_block");
            if let Some(block) = block {
                if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if let Some(idx) = data.get("index").and_then(Value::as_u64) {
                        if !id.is_empty() {
                            state.index_to_id.insert(idx, id.clone());
                        }
                    }
                    if !name.is_empty() && !id.is_empty() {
                        out.push(StreamingEvent::ToolUseStart { id, name });
                    }
                }
            }
        }
        "content_block_delta" => {
            map_content_block_delta(data, state, &mut out);
        }
        "content_block_stop" => {
            if let Some(idx) = data.get("index").and_then(Value::as_u64) {
                let id = state
                    .index_to_id
                    .get(&idx)
                    .cloned()
                    .or_else(|| Some(idx.to_string()));
                if let Some(id) = id {
                    out.push(StreamingEvent::ToolUseEnd { id });
                }
            }
        }
        "message_delta" => {
            merge_message_delta_usage(data, state);
        }
        "message_stop" => {
            state.saw_stop = true;
            out.push(StreamingEvent::Done(state.usage.unwrap_or_default()));
        }
        "error" => {
            return Err(Error::Provider(format!("provider stream error: {data}")));
        }
        _ => {}
    }
    Ok(out)
}

/// Handle a `content_block_delta` event: text, tool-input, or thinking.
fn map_content_block_delta(
    data: &Value,
    state: &AnthropicMapperState,
    out: &mut Vec<StreamingEvent>,
) {
    let delta = data.get("delta");
    let dt = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
    match dt {
        Some("text_delta") => {
            if let Some(text) = delta.and_then(|d| d.get("text")).and_then(Value::as_str) {
                out.push(StreamingEvent::TextDelta(text.to_string()));
            }
        }
        Some("input_json_delta") => {
            let idx = data.get("index").and_then(Value::as_u64);
            let d = delta
                .and_then(|d| d.get("partial_json"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let id = idx
                .and_then(|i| state.index_to_id.get(&i))
                .cloned()
                .or_else(|| idx.map(|n| n.to_string()));
            if let Some(id) = id {
                out.push(StreamingEvent::ToolUseInputDelta {
                    id,
                    delta: d.to_string(),
                });
            }
        }
        Some("thinking_delta") => {
            if let Some(text) = delta
                .and_then(|d| d.get("thinking"))
                .and_then(Value::as_str)
            {
                out.push(StreamingEvent::ThinkingDelta(text.to_string()));
            }
        }
        Some("signature_delta") => {
            // The signature must be replayed on tool-use turns; capture it
            // onto the thinking block via assemble_message.
            if let Some(sig) = delta
                .and_then(|d| d.get("signature"))
                .and_then(Value::as_str)
            {
                out.push(StreamingEvent::ThinkingSignature(sig.to_string()));
            }
        }
        _ => {}
    }
}

/// Merge `message_delta.usage` (cumulative output tokens) into the usage
/// captured at `message_start`.
fn merge_message_delta_usage(data: &Value, state: &mut AnthropicMapperState) {
    let Some(u) = data.get("usage") else {
        return;
    };
    let merged = state.usage.get_or_insert(Usage::default());
    let incoming = usage_from_anthropic(u);
    if incoming.output_tokens > 0 {
        merged.output_tokens = incoming.output_tokens;
    }
    if incoming.input_tokens > 0 {
        merged.input_tokens = incoming.input_tokens;
    }
    if incoming.cache_read_tokens > 0 {
        merged.cache_read_tokens = incoming.cache_read_tokens;
    }
    if incoming.cache_write_tokens > 0 {
        merged.cache_write_tokens = incoming.cache_write_tokens;
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
            thinking: lofi_types::ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: Some(1024),
            base_url: None,
            input_price: None,
            output_price: None,
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
            map_anthropic_event(
                Some("content_block_delta"),
                &data,
                &mut AnthropicMapperState::default()
            )
            .unwrap(),
            vec![StreamingEvent::TextDelta("hi".to_string())]
        );
    }

    #[test]
    fn maps_tool_use_lifecycle() {
        let mut state = AnthropicMapperState::default();
        let start =
            json!({"index":0,"content_block":{"type":"tool_use","id":"tu_0","name":"exec"}});
        assert_eq!(
            map_anthropic_event(Some("content_block_start"), &start, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseStart {
                id: "tu_0".to_string(),
                name: "exec".to_string()
            }]
        );
        // `input_json_delta` carries the block `index`, not the tool id; the
        // mapper must translate index 0 -> `tu_0`.
        let delta = json!({
            "index": 0,
            "delta": {"type":"input_json_delta","partial_json":"{\"a\":"}
        });
        assert_eq!(
            map_anthropic_event(Some("content_block_delta"), &delta, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseInputDelta {
                id: "tu_0".to_string(),
                delta: "{\"a\":".to_string()
            }]
        );
        let stop = json!({"index": 0});
        assert_eq!(
            map_anthropic_event(Some("content_block_stop"), &stop, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseEnd {
                id: "tu_0".to_string()
            }]
        );
    }

    #[test]
    fn maps_thinking_delta() {
        let data = json!({"delta":{"type":"thinking_delta","thinking":"hmm"}});
        assert_eq!(
            map_anthropic_event(
                Some("content_block_delta"),
                &data,
                &mut AnthropicMapperState::default()
            )
            .unwrap(),
            vec![StreamingEvent::ThinkingDelta("hmm".to_string())]
        );
    }

    #[test]
    fn merges_usage_across_start_delta_stop() {
        let mut state = AnthropicMapperState::default();
        let start = json!({"message":{"usage":{"input_tokens":4,"output_tokens":1,"cache_read_input_tokens":1,"cache_creation_input_tokens":2}}});
        map_anthropic_event(Some("message_start"), &start, &mut state).unwrap();
        let delta = json!({"usage":{"output_tokens":9}});
        map_anthropic_event(Some("message_delta"), &delta, &mut state).unwrap();
        let stop = json!({});
        let out = map_anthropic_event(Some("message_stop"), &stop, &mut state).unwrap();
        let StreamingEvent::Done(u) = out.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(u.input_tokens, 4);
        assert_eq!(u.output_tokens, 9);
        assert_eq!(u.cache_read_tokens, 1);
        assert_eq!(u.cache_write_tokens, 2);
    }

    #[test]
    fn returns_empty_for_unknown_event() {
        let data = json!({});
        assert_eq!(
            map_anthropic_event(Some("ping"), &data, &mut AnthropicMapperState::default()).unwrap(),
            vec![]
        );
        assert_eq!(
            map_anthropic_event(None, &data, &mut AnthropicMapperState::default()).unwrap(),
            vec![]
        );
    }
}
