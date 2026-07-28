//! Helpers to convert `ContentBlock`/`Message` into each provider's wire shape.
//!
//! These are pure data transformations — no HTTP, no async. The per-provider
//! request builders in [`crate::ir`] call into these to assemble the message
//! portion of a request body.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{ContentBlock, Message, Role};
use serde_json::{json, Value};

fn collect_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            out.push_str(text);
        }
    }
    out
}

/// Convert a conversation log to the `OpenAI` Chat Completions `messages` array.
///
/// System/user messages become `{role, content: <string>}`. Assistant tool use
/// is emitted as `tool_calls` alongside any text content. `ToolResult` blocks
/// (carried by `Role::Tool` messages) each become a separate
/// `{role:"tool", tool_call_id, content}` entry.
#[must_use]
pub fn to_openai_chat_messages(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {
                let text = collect_text(&m.blocks);
                if !text.is_empty() {
                    out.push(json!({"role": "system", "content": text}));
                }
            }
            Role::User => {
                let text = collect_text(&m.blocks);
                if !text.is_empty() {
                    out.push(json!({"role": "user", "content": text}));
                }
            }
            Role::Assistant => {
                let text = collect_text(&m.blocks);
                let tool_calls: Vec<Value> = m
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, name, input } => {
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "null".to_string());
                            Some(json!({
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": args},
                            }))
                        }
                        _ => None,
                    })
                    .collect();
                let mut msg = json!({"role": "assistant"});
                msg["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    json!(text)
                };
                if !tool_calls.is_empty() {
                    msg["tool_calls"] = json!(tool_calls);
                }
                out.push(msg);
            }
            Role::Tool => {
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = b
                    {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                }
            }
        }
    }
    out
}

/// Convert a conversation log to the `OpenAI` Responses `input` array.
///
/// System/user/assistant text becomes `{type:"message", role, content:[{type, text}]}`.
/// Assistant tool use becomes `{type:"function_call", call_id, name, arguments}`.
/// `ToolResult` blocks become `{type:"function_call_output", call_id, output}`.
#[must_use]
pub fn to_openai_responses_input(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::System | Role::User => {
                let text = collect_text(&m.blocks);
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": m.role.as_str(),
                        "content": [{"type": "input_text", "text": text}],
                    }));
                }
            }
            Role::Assistant => {
                let text = collect_text(&m.blocks);
                if !text.is_empty() {
                    out.push(json!({
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": text}],
                    }));
                }
                for b in &m.blocks {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        let args =
                            serde_json::to_string(input).unwrap_or_else(|_| "null".to_string());
                        out.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": args,
                        }));
                    }
                }
            }
            Role::Tool => {
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = b
                    {
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                    }
                }
            }
        }
    }
    out
}

/// Convert a conversation log to Anthropic Messages API parts.
///
/// Returns `(system, messages)`: `System`-role text is concatenated into the
/// top-level `system` string, and the remaining messages form the `messages`
/// array with per-block `text`/`tool_use`/`tool_result`/`thinking` content.
/// `ToolResult` blocks are emitted inside a synthetic `user` message, as the
/// Anthropic API requires.
#[must_use]
pub fn to_anthropic_request_parts(messages: &[Message]) -> (Option<String>, Vec<Value>) {
    let mut system_parts: Vec<String> = Vec::new();
    let mut out: Vec<Value> = Vec::new();
    for m in messages {
        match m.role {
            Role::System => {
                let t = collect_text(&m.blocks);
                if !t.is_empty() {
                    system_parts.push(t);
                }
            }
            Role::User | Role::Assistant => {
                let blocks: Vec<Value> = m.blocks.iter().map(block_to_anthropic).collect();
                if !blocks.is_empty() {
                    out.push(json!({"role": m.role.as_str(), "content": blocks}));
                }
            }
            Role::Tool => {
                let blocks: Vec<Value> = m
                    .blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => Some(json!({
                            "type": "tool_result",
                            "tool_use_id": tool_use_id,
                            "content": content,
                            "is_error": is_error,
                        })),
                        _ => None,
                    })
                    .collect();
                if !blocks.is_empty() {
                    out.push(json!({"role": "user", "content": blocks}));
                }
            }
        }
    }
    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };
    (system, out)
}

/// Map a single [`ContentBlock`] to its Anthropic content-block object.
fn block_to_anthropic(b: &ContentBlock) -> Value {
    match b {
        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ContentBlock::ToolUse { id, name, input } => {
            json!({"type": "tool_use", "id": id, "name": name, "input": input})
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id,
            "content": content,
            "is_error": is_error,
        }),
        ContentBlock::Thinking { text, signature } => {
            let mut obj = json!({"type": "thinking", "thinking": text});
            if let Some(sig) = signature {
                obj["signature"] = json!(sig);
            }
            obj
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_msg(role: Role, text: &str) -> Message {
        Message {
            role,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn openai_chat_text_messages() {
        let msgs = [text_msg(Role::System, "sys"), text_msg(Role::User, "hi")];
        let out = to_openai_chat_messages(&msgs);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "system");
        assert_eq!(out[0]["content"], "sys");
        assert_eq!(out[1]["role"], "user");
        assert_eq!(out[1]["content"], "hi");
    }

    #[test]
    fn openai_chat_assistant_with_tool_use() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "thinking".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "c1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"code": "1"}),
                },
            ],
        }];
        let out = to_openai_chat_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "assistant");
        assert_eq!(out[0]["content"], "thinking");
        assert_eq!(out[0]["tool_calls"][0]["id"], "c1");
        assert_eq!(out[0]["tool_calls"][0]["function"]["name"], "exec");
        assert_eq!(
            out[0]["tool_calls"][0]["function"]["arguments"],
            "{\"code\":\"1\"}"
        );
    }

    #[test]
    fn openai_chat_tool_result_emits_tool_role() {
        let msgs = [Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".to_string(),
                content: "2".to_string(),
                is_error: false,
            }],
        }];
        let out = to_openai_chat_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "c1");
        assert_eq!(out[0]["content"], "2");
    }

    #[test]
    fn openai_responses_input_shapes() {
        let msgs = [
            text_msg(Role::User, "hi"),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "c1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"code": "1"}),
                }],
            },
            Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "c1".to_string(),
                    content: "1".to_string(),
                    is_error: false,
                }],
            },
        ];
        let out = to_openai_responses_input(&msgs);
        assert_eq!(out[0]["type"], "message");
        assert_eq!(out[0]["content"][0]["type"], "input_text");
        assert_eq!(out[1]["type"], "function_call");
        assert_eq!(out[1]["call_id"], "c1");
        assert_eq!(out[2]["type"], "function_call_output");
        assert_eq!(out[2]["call_id"], "c1");
    }

    #[test]
    fn anthropic_splits_system_from_messages() {
        let msgs = [
            text_msg(Role::System, "sys-a"),
            text_msg(Role::System, "sys-b"),
            text_msg(Role::User, "hi"),
        ];
        let (system, messages) = to_anthropic_request_parts(&msgs);
        assert_eq!(system.as_deref(), Some("sys-a\n\nsys-b"));
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
    }

    #[test]
    fn anthropic_tool_result_in_user_message() {
        let msgs = [Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".to_string(),
                content: "ok".to_string(),
                is_error: false,
            }],
        }];
        let (system, messages) = to_anthropic_request_parts(&msgs);
        assert!(system.is_none());
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "tool_result");
        assert_eq!(messages[0]["content"][0]["tool_use_id"], "tu_1");
    }

    #[test]
    fn anthropic_thinking_block_shape() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: "hmm".to_string(),
                signature: None,
            }],
        }];
        let (_, messages) = to_anthropic_request_parts(&msgs);
        assert_eq!(messages[0]["content"][0]["type"], "thinking");
        assert_eq!(messages[0]["content"][0]["thinking"], "hmm");
    }
}
