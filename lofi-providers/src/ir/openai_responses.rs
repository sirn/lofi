#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::{HashMap, HashSet};

use lofi_types::{
    ContentBlock, Message, Model, Role, ServiceTier, StreamingEvent, ThinkingLevel, Usage,
};
use serde_json::{json, Value};

use super::ProtocolIr;
use crate::ToolSchema;
use lofi_error::{Error, Result};

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
                    "image_url": format!("data:{media_type};base64,{}", super::b64(bytes)),
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
pub fn to_openai_responses_input(messages: &[Message]) -> Vec<Value> {
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
                                        "image_url": format!("data:{};base64,{}", img.media_type, super::b64(&img.bytes)),
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

pub(crate) struct OpenAiResponsesIr;

impl ProtocolIr for OpenAiResponsesIr {
    type State = ResponsesMapperState;

    fn build_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
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

#[must_use]
fn build_openai_responses_request(
    model: &Model,
    messages: &[Message],
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

#[derive(Default, Debug, Clone)]
pub(crate) struct ResponsesMapperState {
    item_to_call: HashMap<String, String>,
    reasoning_items_with_deltas: HashSet<String>,
    pub(crate) saw_completed: bool,
}

/// # Errors
/// Returns [`Error::Provider`] for `response.failed`, `response.incomplete`,
/// or `error` events so a provider-reported failure fails the round trip.
fn map_response_completed(v: &Value) -> StreamingEvent {
    let response = v.get("response").unwrap_or(v);
    let usage = response.get("usage").or_else(|| v.get("usage"));
    // A completed response that still ended early carries the reason
    // in incomplete_details (e.g. max_output_tokens).
    let stop_reason = match response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(Value::as_str)
    {
        Some("max_output_tokens" | "max_tool_calls") => Some(lofi_types::StopReason::MaxTokens),
        Some(_) => Some(lofi_types::StopReason::Other),
        None => Some(lofi_types::StopReason::EndTurn),
    };
    StreamingEvent::Done {
        usage: usage.map(usage_from_openai_responses).unwrap_or_default(),
        stop_reason,
    }
}

fn map_openai_responses_event(
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
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                if !delta.is_empty() {
                    if let Some(item_id) = v.get("item_id").and_then(Value::as_str) {
                        state
                            .reasoning_items_with_deltas
                            .insert(item_id.to_string());
                    }
                    out.push(StreamingEvent::ThinkingDelta(delta.to_string()));
                }
            }
        }
        "response.reasoning_summary_part.done" => {
            let item_id = v.get("item_id").and_then(Value::as_str).unwrap_or("");
            if state.reasoning_items_with_deltas.contains(item_id) {
                out.push(StreamingEvent::ThinkingDelta("\n\n".to_string()));
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
                out.extend(map_completed_output_item(item, state));
            }
        }
        "response.completed" => {
            state.saw_completed = true;
            out.push(map_response_completed(v));
        }
        "response.failed" | "response.incomplete" | "error" => {
            return Err(Error::Provider(format!("provider stream error: {v}")));
        }
        _ => {}
    }
    Ok(out)
}

fn map_completed_output_item(item: &Value, state: &ResponsesMapperState) -> Vec<StreamingEvent> {
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => item
            .get("call_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|id| StreamingEvent::ToolUseEnd { id: id.to_string() })
            .into_iter()
            .collect(),
        Some("reasoning") => {
            // Some Responses-compatible providers omit summary deltas but
            // include the completed item. Avoid duplicating streamed text.
            // Always take `encrypted_content` from the completed item; it is
            // not streamed as a delta.
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
            let mut events = Vec::new();
            if !state.reasoning_items_with_deltas.contains(item_id) {
                if let Some(text) = reasoning_item_text(item) {
                    events.push(StreamingEvent::ThinkingDelta(text));
                }
            }
            if let Some(blob) = item
                .get("encrypted_content")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            {
                events.push(StreamingEvent::ThinkingSignature(blob.to_string()));
            }
            events
        }
        _ => Vec::new(),
    }
}

/// Extract raw text from a completed Responses reasoning item.
/// Prefer the requested summary, falling back to exposed reasoning content
/// for compatible providers that return that shape instead. Display cleanup
/// belongs to the UI; placeholders and whitespace are retained here so the
/// transcript preserves the provider's data.
fn reasoning_item_text(item: &Value) -> Option<String> {
    ["summary", "content"].into_iter().find_map(|field| {
        let text = item
            .get(field)?
            .as_array()?
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n\n");
        (!text.is_empty()).then_some(text)
    })
}

fn usage_from_openai_responses(v: &Value) -> Usage {
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
    use lofi_types::PromptKind;
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "gpt-4o".to_string(),
            name: "gpt".to_string(),
            provider: "p".to_string(),
            api: lofi_types::Api::OpenAiResponses,
            reasoning: false,
            thinking: lofi_types::ThinkingLevel::Off,
            service_tier: lofi_types::ServiceTier::Auto,
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
    fn request_uses_input_array() {
        let req = build_openai_responses_request(&model(), &[], &[]);
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], true);
        assert_eq!(req["store"], false);
        assert!(req.get("input").is_some());
        assert_eq!(
            req["prompt_cache_key"].as_str().map(str::len),
            Some("lofi:".len() + 16)
        );
        assert!(req.get("tools").is_none());
        assert!(req.get("reasoning").is_none());
        assert_eq!(req["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn request_cache_key_is_stable_while_history_is_appended() {
        let system = Message {
            role: lofi_types::Role::System,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "stable instructions".to_string(),
            }],
            kind: PromptKind::default(),
        };
        let first_user = Message {
            role: lofi_types::Role::User,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "initial prompt".to_string(),
            }],
            kind: PromptKind::default(),
        };
        let appended = Message {
            role: lofi_types::Role::Assistant,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: "new response".to_string(),
            }],
            kind: PromptKind::default(),
        };
        let first =
            build_openai_responses_request(&model(), &[system.clone(), first_user.clone()], &[]);
        let second = build_openai_responses_request(&model(), &[system, first_user, appended], &[]);
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
    }

    #[test]
    fn request_cache_key_changes_when_prefix_is_rebuilt() {
        let message = |role, text: &str| Message {
            role,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: text.to_string(),
            }],
            kind: PromptKind::default(),
        };
        let first = build_openai_responses_request(
            &model(),
            &[
                message(lofi_types::Role::System, "shared system"),
                message(lofi_types::Role::User, "prefix one"),
            ],
            &[],
        );
        let second = build_openai_responses_request(
            &model(),
            &[
                message(lofi_types::Role::System, "shared system"),
                message(lofi_types::Role::User, "prefix two"),
            ],
            &[],
        );
        assert_ne!(first["prompt_cache_key"], second["prompt_cache_key"]);
    }

    #[test]
    fn request_forwards_configured_reasoning_effort_verbatim() {
        for (level, expected) in [
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Custom("minimal".to_string()), "minimal"),
        ] {
            let mut model = model();
            model.thinking = level;
            let req = build_openai_responses_request(&model, &[], &[]);
            assert_eq!(
                req["reasoning"],
                json!({"effort": expected, "summary": "auto"})
            );
        }
    }

    #[test]
    fn request_forwards_service_tier_but_omits_auto() {
        for tier in [ServiceTier::Flex, ServiceTier::Priority] {
            let mut model = model();
            model.service_tier = tier.clone();
            let req = build_openai_responses_request(&model, &[], &[]);
            assert_eq!(req["service_tier"], tier.as_str());
        }
        assert!(build_openai_responses_request(&model(), &[], &[])
            .get("service_tier")
            .is_none());
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
    fn maps_completed_reasoning_item_when_deltas_are_absent() {
        let ev = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "summary":[
                    {"type":"summary_text","text":"Checked the inputs."},
                    {"type":"summary_text","text":"Selected the result."}
                ]
            }
        });
        assert_eq!(
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![StreamingEvent::ThinkingDelta(
                "Checked the inputs.\n\nSelected the result.".to_string()
            )]
        );
    }

    #[test]
    fn completed_reasoning_item_preserves_raw_placeholder_parts() {
        let ev = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "summary":[
                    {"type":"summary_text","text":" **Checking**\n<!-- --> "},
                    {"type":"summary_text","text":"Kept <!-- --> literally."},
                    {"type":"summary_text","text":" **Done**\nResult "}
                ]
            }
        });
        assert_eq!(
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![StreamingEvent::ThinkingDelta(
                " **Checking**\n<!-- --> \n\nKept <!-- --> literally.\n\n **Done**\nResult "
                    .to_string()
            )]
        );
    }

    #[test]
    fn summary_part_done_separates_streamed_parts() {
        let mut state = ResponsesMapperState::default();
        let first = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"First"
        });
        let part_done = json!({
            "type":"response.reasoning_summary_part.done",
            "item_id":"rs_1"
        });
        let second = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"Second"
        });
        map_openai_responses_event(&first, &mut state).unwrap();
        assert_eq!(
            map_openai_responses_event(&part_done, &mut state).unwrap(),
            vec![StreamingEvent::ThinkingDelta("\n\n".to_string())]
        );
        assert_eq!(
            map_openai_responses_event(&second, &mut state).unwrap(),
            vec![StreamingEvent::ThinkingDelta("Second".to_string())]
        );
    }

    #[test]
    fn completed_reasoning_item_does_not_duplicate_streamed_summary() {
        let mut state = ResponsesMapperState::default();
        let delta = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"Checked the inputs."
        });
        map_openai_responses_event(&delta, &mut state).unwrap();
        let done = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "summary":[{"type":"summary_text","text":"Checked the inputs."}]
            }
        });
        assert!(map_openai_responses_event(&done, &mut state)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn completed_reasoning_item_emits_encrypted_content_as_signature() {
        let ev = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "encrypted_content":"enc_blob",
                "summary":[{"type":"summary_text","text":"Checked the inputs."}]
            }
        });
        assert_eq!(
            map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
            vec![
                StreamingEvent::ThinkingDelta("Checked the inputs.".to_string()),
                StreamingEvent::ThinkingSignature("enc_blob".to_string()),
            ]
        );
    }

    #[test]
    fn streamed_reasoning_still_captures_encrypted_content() {
        let mut state = ResponsesMapperState::default();
        let delta = json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":"rs_1",
            "delta":"Checked the inputs."
        });
        map_openai_responses_event(&delta, &mut state).unwrap();
        let done = json!({
            "type":"response.output_item.done",
            "item":{
                "type":"reasoning",
                "id":"rs_1",
                "encrypted_content":"enc_blob",
                "summary":[{"type":"summary_text","text":"Checked the inputs."}]
            }
        });
        assert_eq!(
            map_openai_responses_event(&done, &mut state).unwrap(),
            vec![StreamingEvent::ThinkingSignature("enc_blob".to_string())]
        );
    }

    #[test]
    fn encrypted_reasoning_replays_before_function_call() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    text: "Need a tool.".to_string(),
                    signature: Some("enc_blob".to_string()),
                    redacted: false,
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "exec".to_string(),
                    input: json!({"code": "1"}),
                },
            ],
            kind: PromptKind::default(),
        }];
        let input = to_openai_responses_input(&msgs);
        assert_eq!(
            input[0],
            json!({
                "type": "reasoning",
                "encrypted_content": "enc_blob",
                "summary": [{"type": "summary_text", "text": "Need a tool."}],
            })
        );
        assert_eq!(input[1]["type"], json!("function_call"));
        assert_eq!(input[1]["call_id"], json!("call_1"));
    }

    #[test]
    fn plaintext_thinking_is_not_replayed() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    text: "local only".to_string(),
                    signature: None,
                    redacted: false,
                },
                ContentBlock::Text {
                    text: "done".to_string(),
                },
            ],
            kind: PromptKind::default(),
        }];
        let input = to_openai_responses_input(&msgs);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], json!("message"));
        assert_eq!(
            input[0]["content"][0],
            json!({"type": "output_text", "text": "done"})
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
        let StreamingEvent::Done { usage: u, .. } = done.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(u.input_tokens, 2);
        assert_eq!(u.output_tokens, 7);
        assert_eq!(u.cache_read_tokens, 1);
    }

    #[test]
    fn maps_incomplete_details_reason_into_done() {
        for (details, expected) in [
            (None, lofi_types::StopReason::EndTurn),
            (Some("max_output_tokens"), lofi_types::StopReason::MaxTokens),
            (Some("content_filter"), lofi_types::StopReason::Other),
        ] {
            let ev = match details {
                Some(reason) => json!({
                    "type":"response.completed",
                    "response":{"status":"incomplete","incomplete_details":{"reason":reason}}
                }),
                None => json!({
                    "type":"response.completed",
                    "response":{"status":"completed"}
                }),
            };
            let done =
                map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap();
            let StreamingEvent::Done { stop_reason, .. } = done.into_iter().next().unwrap() else {
                panic!("expected Done");
            };
            assert_eq!(stop_reason, Some(expected), "details {details:?}");
        }
    }

    #[test]
    fn tool_result_with_images_serializes_output_as_items() {
        let msgs = [Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".to_string(),
                content: "read image.png".to_string(),
                is_error: false,
                images: vec![lofi_types::ToolResultImage {
                    bytes: vec![1, 2, 3],
                    media_type: "image/png".to_string(),
                }],
            }],
            kind: PromptKind::default(),
        }];
        let input = to_openai_responses_input(&msgs);
        // The function_call_output keeps a plain-string output; the image is
        // carried on a following user `message` in the universally supported
        // position, because some Responses gateways drop images inside an
        // array-valued tool output.
        assert_eq!(input[0]["type"], json!("function_call_output"));
        assert_eq!(input[0]["output"], json!("read image.png"));
        assert_eq!(input[1]["type"], json!("message"));
        assert_eq!(input[1]["role"], json!("user"));
        assert_eq!(
            input[1]["content"][0],
            json!({
                "type": "input_image",
                "detail": "auto",
                "image_url": "data:image/png;base64,AQID",
            })
        );
    }

    #[test]
    fn tool_result_without_images_stays_string() {
        let msgs = [Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "c1".to_string(),
                content: "plain".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        }];
        let input = to_openai_responses_input(&msgs);
        assert_eq!(input[0]["output"], json!("plain"));
    }

    #[test]
    fn user_image_block_serializes_as_input_image() {
        let msgs = [Message {
            role: Role::User,
            blocks: vec![
                ContentBlock::Text {
                    text: "describe".to_string(),
                },
                ContentBlock::Image {
                    bytes: vec![1, 2, 3],
                    media_type: "image/jpeg".to_string(),
                },
            ],
            kind: PromptKind::default(),
        }];
        let req = build_openai_responses_request(&model(), &msgs, &[]);
        let content = &req["input"][0]["content"];
        assert_eq!(
            content[0],
            json!({"type": "input_text", "text": "describe"})
        );
        assert_eq!(
            content[1],
            json!({
                "type": "input_image",
                "image_url": "data:image/jpeg;base64,AQID",
            })
        );
    }
}
