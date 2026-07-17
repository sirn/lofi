//! `OpenAI` Chat Completions request-body builder + SSE-event mapper.
//!
//! Pure translation only: the providers layer POSTs the body produced here and
//! feeds each parsed SSE `data:` JSON into [`map_openai_chat_event`].

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{Message, Model, StreamingEvent, Usage};
use serde_json::{json, Value};

use super::block::to_openai_chat_messages;
use super::chat::ToolSchema;

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
    req
}

/// Map a single Chat Completions stream chunk to a [`StreamingEvent`].
///
/// Handles `choices[0].delta.content` (text), `choices[0].delta.tool_calls`
/// (start + input deltas), and the terminal `usage` chunk. Returns `None` for
/// keep-alive or intermediate chunks (e.g. a bare `finish_reason` delta).
#[must_use]
pub fn map_openai_chat_event(v: &Value) -> Option<StreamingEvent> {
    // The final usage chunk may carry an empty `choices` array; check it first.
    if let Some(usage) = v.get("usage") {
        if !usage.is_null() {
            return Some(StreamingEvent::Done(usage_from_openai_chat(usage)));
        }
    }

    let choices = v.get("choices")?.as_array()?;
    if choices.is_empty() {
        return None;
    }
    let delta = choices[0].get("delta")?;

    if let Some(content) = delta.get("content").and_then(|c| c.as_str()) {
        if !content.is_empty() {
            return Some(StreamingEvent::TextDelta(content.to_string()));
        }
    }

    if let Some(arr) = delta
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
    {
        for tc in arr {
            let id = tc.get("id").and_then(serde_json::Value::as_str);
            let function = tc.get("function");
            let name = function
                .and_then(|f| f.get("name"))
                .and_then(serde_json::Value::as_str);
            let args = function
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str());

            if let (Some(id), Some(name)) = (id, name) {
                return Some(StreamingEvent::ToolUseStart {
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
            if let Some(args) = args {
                if !args.is_empty() {
                    let id = id.map_or_else(
                        || {
                            tc.get("index")
                                .and_then(serde_json::Value::as_u64)
                                .map_or_else(|| "0".to_string(), |n| n.to_string())
                        },
                        str::to_string,
                    );
                    return Some(StreamingEvent::ToolUseInputDelta {
                        id,
                        delta: args.to_string(),
                    });
                }
            }
        }
    }

    None
}

/// Extract [`Usage`] from a Chat Completions `usage` object.
fn usage_from_openai_chat(v: &Value) -> Usage {
    Usage {
        input_tokens: v
            .get("prompt_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        output_tokens: v
            .get("completion_tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        cache_read_tokens: v
            .get("prompt_tokens_details")
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
            api: lofi_types::Api::OpenAiCompletions,
            reasoning: false,
            supports_image: false,
            context_window: None,
            max_tokens: None,
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
            map_openai_chat_event(&chunk),
            Some(StreamingEvent::TextDelta("hi".to_string()))
        );
    }

    #[test]
    fn maps_tool_use_start_and_input() {
        let start = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"id":"call_1","type":"function","function":{"name":"exec","arguments":""}}
        ]}}]});
        assert_eq!(
            map_openai_chat_event(&start),
            Some(StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string()
            })
        );
        let args = json!({"choices":[{"delta":{"tool_calls":[
            {"index":0,"function":{"arguments":"{\"a\":"}}
        ]}}]});
        assert_eq!(
            map_openai_chat_event(&args),
            Some(StreamingEvent::ToolUseInputDelta {
                id: "0".to_string(),
                delta: "{\"a\":".to_string()
            })
        );
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
        let done = map_openai_chat_event(&chunk);
        let StreamingEvent::Done(u) = done.unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(u.input_tokens, 10);
        assert_eq!(u.output_tokens, 5);
        assert_eq!(u.cache_read_tokens, 2);
    }

    #[test]
    fn returns_none_for_bare_finish_reason() {
        let chunk = json!({"choices":[{"delta":{},"finish_reason":"stop"}]});
        assert_eq!(map_openai_chat_event(&chunk), None);
    }
}
