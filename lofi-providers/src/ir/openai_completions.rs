//! `OpenAI` Chat Completions request-body builder + SSE-event mapper.
//!
//! Pure translation only: the providers layer POSTs the body produced here and
//! feeds each parsed SSE `data:` JSON into [`map_openai_chat_event`].

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{Message, Model, StreamingEvent, ThinkingLevel, Usage};
use serde_json::{json, Value};

use super::block::to_openai_chat_messages;
use super::chat::ToolSchema;
use lofi_error::{Error, Result};

/// Build the `POST /chat/completions` body for a streaming turn.
#[must_use]
pub fn build_openai_chat_request(
    model: &Model,
    messages: &[Message],
    tools: &[ToolSchema],
) -> Value {
    let msgs = to_openai_chat_messages(messages);
    let mut req = json!({
        "model": model.id,
        "messages": msgs,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(mt) = model.max_tokens {
        // `max_completion_tokens` is the current Chat Completions field
        // (`max_tokens` is deprecated and rejected by newer models).
        req["max_completion_tokens"] = json!(mt);
    }
    if !tools.is_empty() {
        let tools_arr: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    },
                })
            })
            .collect();
        req["tools"] = json!(tools_arr);
    }
    if let Some(effort) = openai_effort(model.thinking) {
        // Chat Completions exposes reasoning effort as a top-level field on
        // reasoning-capable models. `off` omits it entirely so the model's
        // default behavior applies.
        req["reasoning_effort"] = json!(effort);
    }
    req
}

/// Map a thinking level to an `OpenAI` `reasoning.effort` / `reasoning_effort`
/// value. `Off` disables thinking (returns `None` so the field is omitted);
/// `XHigh` clamps to `high` since `OpenAI` exposes no higher step.
fn openai_effort(level: ThinkingLevel) -> Option<&'static str> {
    match level {
        ThinkingLevel::Off => None,
        ThinkingLevel::Low => Some("low"),
        ThinkingLevel::Medium => Some("medium"),
        ThinkingLevel::High | ThinkingLevel::XHigh => Some("high"),
    }
}

/// State accumulated across Chat Completions chunks for tool-call correlation.
///
/// `OpenAI` identifies each streaming tool call by a stable `index`; the call
/// `id` arrives only on the first delta for that index. We map `index -> id`
/// so later argument deltas (which carry `index` but not `id`) are emitted
/// against the correct [`StreamingEvent::ToolUseStart`] id rather than a
/// synthesized placeholder that [`assemble_message`](super::codec::assemble_message)
/// can't correlate.
#[derive(Default, Debug, Clone)]
pub struct ChatMapperState {
    index_to_id: std::collections::HashMap<u64, String>,
}

/// Map a single Chat Completions stream chunk to zero or more
/// [`StreamingEvent`]s.
///
/// Handles `choices[0].delta.content` (text), `choices[0].delta.tool_calls`
/// (start + input deltas, possibly several per chunk and across calls), and
/// the terminal `usage` chunk. A chunk may carry both a tool *start* and its
/// first *arguments* in the same delta, so all `tool_calls` entries are
/// accumulated rather than returning at the first one.
///
/// # Errors
///
/// Returns [`Error::Provider`] when a chunk carries a top-level `error`
/// object, so an in-stream provider error fails the round trip instead of
/// ending as a silent partial turn.
pub fn map_openai_chat_event(
    v: &Value,
    state: &mut ChatMapperState,
) -> Result<Vec<StreamingEvent>> {
    let mut out = Vec::new();
    // An in-stream error chunk (`{"error": {...}}`) must fail the round trip
    // rather than being ignored as an unrecognized chunk.
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(Value::as_str)
            .map_or_else(|| err.to_string(), str::to_string);
        return Err(Error::Provider(format!("provider stream error: {msg}")));
    }
    // The final usage chunk may carry an empty `choices` array; check it first.
    if let Some(usage) = v.get("usage") {
        if !usage.is_null() {
            out.push(StreamingEvent::Done(usage_from_openai_chat(usage)));
            return Ok(out);
        }
    }

    let Some(choices) = v.get("choices").and_then(Value::as_array) else {
        return Ok(out);
    };
    if choices.is_empty() {
        return Ok(out);
    }
    let Some(delta) = choices[0].get("delta") else {
        return Ok(out);
    };

    // OpenAI-compatible reasoning models (DeepSeek, Qwen, etc.) stream the
    // chain-of-thought as `delta.reasoning_content`, separate from `content`.
    // Emit it as a thinking delta so it lands in its own thinking block.
    if let Some(reasoning) = delta.get("reasoning_content").and_then(Value::as_str) {
        if !reasoning.is_empty() {
            out.push(StreamingEvent::ThinkingDelta(reasoning.to_string()));
        }
    }

    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            out.push(StreamingEvent::TextDelta(content.to_string()));
        }
    }

    if let Some(arr) = delta.get("tool_calls").and_then(Value::as_array) {
        for tc in arr {
            let idx = tc.get("index").and_then(Value::as_u64);
            let id = tc.get("id").and_then(Value::as_str);
            let function = tc.get("function");
            let name = function.and_then(|f| f.get("name")).and_then(Value::as_str);
            let args = function
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str);

            if let (Some(idx), Some(id)) = (idx, id) {
                state
                    .index_to_id
                    .entry(idx)
                    .or_insert_with(|| id.to_string());
            }

            if let (Some(id), Some(name)) = (id, name) {
                if !name.is_empty() {
                    out.push(StreamingEvent::ToolUseStart {
                        id: id.to_string(),
                        name: name.to_string(),
                    });
                }
            }

            if let Some(args) = args {
                if !args.is_empty() {
                    let cid = idx
                        .and_then(|i| state.index_to_id.get(&i))
                        .cloned()
                        .or_else(|| id.map(str::to_string));
                    if let Some(cid) = cid {
                        out.push(StreamingEvent::ToolUseInputDelta {
                            id: cid,
                            delta: args.to_string(),
                        });
                    }
                }
            }
        }
    }

    Ok(out)
}

/// Extract [`Usage`] from a Chat Completions `usage` object.
fn usage_from_openai_chat(v: &Value) -> Usage {
    // Chat Completions reports `prompt_tokens` as the full prompt (cached +
    // non-cached) and the cached slice separately in `prompt_tokens_details`.
    // Store the non-cached portion in `input_tokens` so `input + cache_read`
    // reconstructs the prompt without double-counting (input_tokens is the
    // Anthropic-style normalization where `input` is the cache miss).
    let prompt = v
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let cached = v
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    Usage {
        input_tokens: prompt.saturating_sub(cached),
        output_tokens: v
            .get("completion_tokens")
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
            api: lofi_types::Api::OpenAiCompletions,
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

    #[test]
    fn request_includes_stream_and_usage() {
        let req = build_openai_chat_request(&model(), &[], &[]);
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], true);
        assert_eq!(req["stream_options"]["include_usage"], true);
    }

    #[test]
    fn request_serializes_tools() {
        let tools = [ToolSchema {
            name: "exec".to_string(),
            description: "run".to_string(),
            input_schema: json!({"type": "object"}),
        }];
        let req = build_openai_chat_request(&model(), &[], &tools);
        assert_eq!(req["tools"][0]["type"], "function");
        assert_eq!(req["tools"][0]["function"]["name"], "exec");
    }

    #[test]
    fn maps_text_delta() {
        let chunk = json!({"choices":[{"delta":{"content":"hi"}}]});
        assert_eq!(
            map_openai_chat_event(&chunk, &mut ChatMapperState::default()).unwrap(),
            vec![StreamingEvent::TextDelta("hi".to_string())]
        );
    }

    #[test]
    fn maps_tool_use_start_and_input() {
        let mut state = ChatMapperState::default();
        let start = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","type":"function","function":{"name":"exec","arguments":""}}
        ]}}]});
        assert_eq!(
            map_openai_chat_event(&start, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string()
            }]
        );
        // Argument deltas carry `index` but not `id`; they must correlate to
        // the real call id, not a placeholder.
        let args = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"a\":"}}
        ]}}]});
        assert_eq!(
            map_openai_chat_event(&args, &mut state).unwrap(),
            vec![StreamingEvent::ToolUseInputDelta {
                id: "call_1".to_string(),
                delta: "{\"a\":".to_string()
            }]
        );
    }

    #[test]
    fn maps_parallel_tool_calls_in_one_chunk() {
        // Two tool calls in a single delta: both starts and both argument
        // fragments must be preserved (the old mapper returned at the first).
        let chunk = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_a","type":"function","function":{"name":"exec","arguments":"{\"x\":"}},
            {"index":1,"id":"call_b","type":"function","function":{"name":"exec","arguments":"{\"y\":"}}
        ]}}]});
        let out = map_openai_chat_event(&chunk, &mut ChatMapperState::default()).unwrap();
        assert_eq!(out.len(), 4);
        assert!(matches!(&out[0], StreamingEvent::ToolUseStart { id, .. } if id == "call_a"));
        assert!(matches!(&out[1], StreamingEvent::ToolUseInputDelta { id, .. } if id == "call_a"));
        assert!(matches!(&out[2], StreamingEvent::ToolUseStart { id, .. } if id == "call_b"));
        assert!(matches!(&out[3], StreamingEvent::ToolUseInputDelta { id, .. } if id == "call_b"));
    }

    #[test]
    fn maps_usage_done() {
        let chunk = json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "prompt_tokens_details": {"cached_tokens": 2}
            }
        });
        let done = map_openai_chat_event(&chunk, &mut ChatMapperState::default()).unwrap();
        let StreamingEvent::Done(u) = done.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        // input_tokens excludes the cached slice: prompt_tokens(10) - cached(2)
        assert_eq!(u.input_tokens, 8);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.cache_read_tokens, 2);
    }

    #[test]
    fn returns_none_for_bare_finish_reason() {
        let chunk = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        assert_eq!(
            map_openai_chat_event(&chunk, &mut ChatMapperState::default()).unwrap(),
            vec![]
        );
    }

    #[test]
    fn in_stream_error_chunk_is_fatal() {
        let chunk = json!({"error":{"message":"rate limited","type":"rate_limit_error"}});
        let r = map_openai_chat_event(&chunk, &mut ChatMapperState::default());
        assert!(r.is_err());
        let msg = r.unwrap_err().to_string();
        assert!(msg.contains("rate limited"), "{msg}");
    }
}
