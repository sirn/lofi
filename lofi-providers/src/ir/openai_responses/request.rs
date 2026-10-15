use lofi_types::{ContentBlock, Message, Model, Role, ServiceTier, ThinkingLevel};
use serde_json::{json, Value};

use crate::ToolSchema;

fn collect_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            out.push_str(text);
        }
    }
    out
}

fn input_message(message: &Message) -> Option<Value> {
    let text = collect_text(&message.blocks);
    let mut content = Vec::new();
    if !text.is_empty() {
        content.push(json!({"type": "input_text", "text": text}));
    }
    if message.role == Role::User {
        for block in &message.blocks {
            if let ContentBlock::Image { bytes, media_type } = block {
                content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{}", crate::ir::b64(bytes)),
                }));
            }
        }
    }
    (!content.is_empty()).then(|| {
        json!({
            "type": "message",
            "role": message.role.as_str(),
            "content": content,
        })
    })
}

#[must_use]
pub(super) fn to_openai_responses_input<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::System | Role::User => {
                if let Some(message) = input_message(m) {
                    out.push(message);
                }
            }
            Role::Assistant => {
                for b in &m.blocks {
                    match b {
                        ContentBlock::Thinking {
                            text,
                            signature: Some(sig),
                            ..
                        } if !sig.is_empty() => {
                            // Responses needs the encrypted blob on later
                            // turns when `store` is false. Plaintext-only
                            // traces (no blob) stay local.
                            let summary = if text.is_empty() {
                                Vec::new()
                            } else {
                                vec![json!({"type": "summary_text", "text": text})]
                            };
                            out.push(json!({
                                "type": "reasoning",
                                "encrypted_content": sig,
                                "summary": summary,
                            }));
                        }
                        ContentBlock::Text { text } if !text.is_empty() => {
                            out.push(json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": text}],
                            }));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            let args =
                                serde_json::to_string(input).unwrap_or_else(|_| "null".to_string());
                            out.push(json!({
                                "type": "function_call",
                                "call_id": id,
                                "name": name,
                                "arguments": args,
                            }));
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        images,
                        ..
                    } = b
                    {
                        // Keep `output` a plain string and carry any images on a
                        // following user `message`. Some OpenAI-compatible
                        // Responses gateways accept a request where
                        // `function_call_output.output` is an array of content
                        // items but silently drop the image items; a user-role
                        // `message` with `input_image` content is the
                        // universally supported position for an image.
                        out.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": content,
                        }));
                        if !images.is_empty() {
                            let content: Vec<Value> = images
                                .iter()
                                .map(|img| {
                                    json!({
                                        "type": "input_image",
                                        "detail": "auto",
                                        "image_url": format!("data:{};base64,{}", img.media_type, crate::ir::b64(&img.bytes)),
                                    })
                                })
                                .collect();
                            out.push(json!({
                                "type": "message",
                                "role": "user",
                                "content": content,
                            }));
                        }
                    }
                }
            }
        }
    }
    out
}

#[must_use]
pub(super) fn build_openai_responses_request<'a>(
    model: &Model,
    messages: impl IntoIterator<Item = &'a Message>,
    tools: &[ToolSchema],
) -> Value {
    let input = to_openai_responses_input(messages);
    let mut req = json!({
        "model": model.id,
        "input": input,
        "stream": true,
        "store": false,
    });
    // Stateless Responses (store=false / ZDR) needs the encrypted reasoning
    // blob on later turns. OpenAI now emits it by default; `include` remains
    // for older and compatible endpoints.
    req["include"] = json!(["reasoning.encrypted_content"]);
    req["prompt_cache_key"] = json!(prompt_cache_key(model, &input));
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
    if let Some(effort) = openai_effort(&model.thinking) {
        req["reasoning"] = json!({ "effort": effort, "summary": "auto" });
    }
    if model.service_tier != ServiceTier::Auto {
        req["service_tier"] = json!(model.service_tier.as_str());
    }
    req
}

fn prompt_cache_key(model: &Model, input: &[Value]) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let mut hash = FNV_OFFSET;
    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    update(model.id.as_bytes());
    if let Some(first) = input.first() {
        update(first.to_string().as_bytes());
    }
    if let Some(first_user) = input
        .iter()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))
    {
        update(first_user.to_string().as_bytes());
    }
    format!("lofi:{hash:016x}")
}

fn openai_effort(level: &ThinkingLevel) -> Option<&str> {
    (level != &ThinkingLevel::Off).then(|| level.as_str())
}
