#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{
    ContentBlock, Message, Model, PartSignatureFormat, Role, ServiceTier, StreamingEvent,
    ThinkingLevel, Usage,
};
use serde_json::{json, Value};

use super::ProtocolIr;
use crate::ToolSchema;
use lofi_error::{Error, Result};

// Field names that carry chain-of-thought on chat-completions streams.
// DeepSeek/Qwen use `reasoning_content`, GLM/Zhipu uses `thinking`, other
// vendors use `reasoning` or `reasoning_text`. The same list drives
// capture (the first non-empty field wins per message) and replay
// (assistant messages re-emit thinking text under the recorded name), so
// both sites reference this single source.
const REASONING_FIELDS: [&str; 4] = [
    "reasoning_content",
    "thinking",
    "reasoning",
    "reasoning_text",
];

fn collect_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            out.push_str(text);
        }
    }
    out
}

fn push_system(m: &Message, out: &mut Vec<Value>) {
    let text = collect_text(&m.blocks);
    if !text.is_empty() {
        out.push(json!({"role": "system", "content": text}));
    }
}

fn push_user(m: &Message, out: &mut Vec<Value>) {
    let text = collect_text(&m.blocks);
    let has_image = m
        .blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. }));
    if has_image {
        // Multipart content array: text parts plus one image_url
        // part per attached image, as a data: URL.
        let mut parts: Vec<Value> = Vec::new();
        if !text.is_empty() {
            parts.push(json!({"type": "text", "text": text}));
        }
        for b in &m.blocks {
            if let ContentBlock::Image { bytes, media_type } = b {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{media_type};base64,{}", super::b64(bytes)),
                    },
                }));
            }
        }
        out.push(json!({"role": "user", "content": parts}));
    } else if !text.is_empty() {
        out.push(json!({"role": "user", "content": text}));
    }
}

fn push_assistant(model: &Model, m: &Message, out: &mut Vec<Value>) {
    let text = collect_text(&m.blocks);
    let mut tool_calls = Vec::new();
    let mut reasoning_details = Vec::new();
    let mut reasoning: Vec<(&str, &str)> = Vec::new();
    for (index, block) in m.blocks.iter().enumerate() {
        // A Thinking block whose signature is one of the field names the
        // mapper records is plaintext chain-of-thought (chat-completions).
        // Real blobs (Anthropic signature, Responses encrypted_content)
        // never equal one of these keys, so this match cannot misfire.
        if let ContentBlock::Thinking {
            text,
            signature: Some(field),
        } = block
        {
            // Trim-check on echo (Pi's rule): whitespace-only reasoning has
            // no semantic content the model needs back.
            if REASONING_FIELDS.contains(&field.as_str()) && !text.trim().is_empty() {
                reasoning.push((field.as_str(), text.as_str()));
            }
        }
        if let ContentBlock::ToolUse { id, name, input } = block {
            let args = serde_json::to_string(input).unwrap_or_else(|_| "null".to_string());
            let mut tool_call = json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": args},
            });
            if let Some(ContentBlock::PartSignature {
                provider,
                model: signed_model,
                format,
                signature,
            }) = m.blocks.get(index + 1)
            {
                if provider == &model.provider && signed_model == &model.id {
                    match format {
                        PartSignatureFormat::OpenAiExtraContent { namespace } => {
                            tool_call["extra_content"] = Value::Object(
                                [(namespace.clone(), json!({"thought_signature": signature}))]
                                    .into_iter()
                                    .collect(),
                            );
                        }
                        PartSignatureFormat::OpenAiReasoningDetail => {
                            if let Ok(detail) = serde_json::from_str::<Value>(signature) {
                                reasoning_details.push(detail);
                            }
                        }
                        PartSignatureFormat::Google => {}
                    }
                }
            }
            tool_calls.push(tool_call);
        }
    }
    let mut msg = json!({"role": "assistant"});
    msg["content"] = if text.is_empty() {
        Value::Null
    } else {
        json!(text)
    };
    if !tool_calls.is_empty() {
        msg["tool_calls"] = json!(tool_calls);
    }
    if !reasoning_details.is_empty() {
        msg["reasoning_details"] = json!(reasoning_details);
    }
    // The wire form carries one reasoning channel per assistant message, so
    // thinking blocks (one per round of tool-use / text interleave) are
    // joined with "\n" under the field the server first used (mirrors Pi).
    if let Some(&(field, _)) = reasoning.first() {
        let joined = reasoning
            .iter()
            .map(|(_, t)| *t)
            .collect::<Vec<_>>()
            .join("\n");
        msg[field] = json!(joined);
    }
    out.push(msg);
}

fn push_tool(m: &Message, out: &mut Vec<Value>) {
    for b in &m.blocks {
        if let ContentBlock::ToolResult {
            tool_use_id,
            content,
            images,
            ..
        } = b
        {
            // Chat-completions `tool` messages carry only string
            // content — an image cannot ride the tool result. Emit it on a
            // following user message instead, the universally supported
            // position for an image, so the model sees it in the same round
            // as the result.
            out.push(json!({
                "role": "tool",
                "tool_call_id": tool_use_id,
                "content": content,
            }));
            if !images.is_empty() {
                let parts: Vec<Value> = images
                    .iter()
                    .map(|img| {
                        json!({
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:{};base64,{}", img.media_type, super::b64(&img.bytes)),
                            },
                        })
                    })
                    .collect();
                out.push(json!({
                    "role": "user",
                    "content": parts,
                }));
            }
        }
    }
}

#[must_use]
pub fn to_openai_chat_messages(model: &Model, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::System => push_system(m, &mut out),
            Role::User => push_user(m, &mut out),
            Role::Assistant => push_assistant(model, m, &mut out),
            Role::Tool => push_tool(m, &mut out),
        }
    }
    out
}

pub(crate) struct OpenAiCompletionsIr;

impl ProtocolIr for OpenAiCompletionsIr {
    type State = ChatMapperState;

    fn build_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
        build_openai_chat_request(model, messages, tools)
    }

    fn new_state(model: &Model) -> Self::State {
        ChatMapperState {
            provider: model.provider.clone(),
            model: model.id.clone(),
            ..ChatMapperState::default()
        }
    }

    fn map_event(
        _event: Option<&str>,
        data: &Value,
        state: &mut Self::State,
    ) -> Result<Vec<StreamingEvent>> {
        map_openai_chat_event(data, state)
    }

    fn on_eof(_state: &Self::State) -> Result<()> {
        Err(Error::Provider(
            "stream ended before [DONE] sentinel".into(),
        ))
    }

    fn defer_done_until_transport_end() -> bool {
        true
    }
}

#[must_use]
fn build_openai_chat_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
    let msgs = to_openai_chat_messages(model, messages);
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
    if let Some(effort) = openai_effort(&model.thinking) {
        // Chat Completions exposes reasoning effort as a top-level field on
        // reasoning-capable models. `off` omits it entirely so the model's
        // default behavior applies.
        req["reasoning_effort"] = json!(effort);
    }
    if model.service_tier != ServiceTier::Auto {
        req["service_tier"] = json!(model.service_tier.as_str());
    }
    req
}

/// Return the configured effort verbatim, omitting only `off`.
fn openai_effort(level: &ThinkingLevel) -> Option<&str> {
    (level != &ThinkingLevel::Off).then(|| level.as_str())
}

/// `OpenAI` identifies each streaming tool call by a stable `index`; the call
/// `id` arrives only on the first delta for that index. We map `index -> id`
/// so later argument deltas (which carry `index` but not `id`) are emitted
/// against the correct [`StreamingEvent::ToolUseStart`] id rather than a
/// synthesized placeholder that [`assemble_message`](super::codec::assemble_message)
/// can't correlate.
#[derive(Default, Debug, Clone)]
pub(crate) struct ChatMapperState {
    index_to_id: std::collections::HashMap<u64, String>,
    pending_signature_by_index: std::collections::HashMap<u64, (String, String)>,
    // The delta key the server used to stream its chain-of-thought for the
    // current block (see the reasoning-delta branch in map_openai_chat_event).
    reasoning_field: Option<String>,
    provider: String,
    model: String,
}

/// Handles `choices[0].delta.content` (text), `choices[0].delta.tool_calls`
/// (start + input deltas, possibly several per chunk and across calls), and
/// the terminal `usage` chunk. A chunk may carry both a tool *start* and its
/// first *arguments* in the same delta, so all `tool_calls` entries are
/// accumulated rather than returning at the first one.
/// # Errors
/// Returns [`Error::Provider`] when a chunk carries a top-level `error`
/// object, so an in-stream provider error fails the round trip instead of
/// ending as a silent partial turn.
fn map_openai_chat_event(v: &Value, state: &mut ChatMapperState) -> Result<Vec<StreamingEvent>> {
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
    // DeepSeek-style providers attach `usage` to every chunk; it marks turn
    // end only on the final usage-only frame (empty `choices`), otherwise deltas
    // must still be parsed and the accounting held for the terminator.
    let done = v
        .get("usage")
        .filter(|u| !u.is_null())
        .map(usage_from_openai_chat);

    // Emit the held accounting only when the frame produces no content. A
    // frame carrying both deltas and usage keeps its stream flowing; the usage
    // on a contentful frame is per-chunk accounting, not turn end.
    let drain = |out: &mut Vec<StreamingEvent>| {
        if out.is_empty() {
            if let Some(done) = done {
                out.push(StreamingEvent::Done(done));
            }
        }
    };

    let Some(choices) = v.get("choices").and_then(Value::as_array) else {
        drain(&mut out);
        return Ok(out);
    };
    if choices.is_empty() {
        drain(&mut out);
        return Ok(out);
    }
    let Some(delta) = choices[0].get("delta") else {
        drain(&mut out);
        return Ok(out);
    };

    // The field the server actually used is also its replay field — record
    // it once so the next request can put the thinking text back under the
    // same name, the way DeepSeek's multi-round tool-calling contract requires.
    for field in REASONING_FIELDS {
        let Some(reasoning) = delta.get(field).and_then(Value::as_str) else {
            continue;
        };
        if reasoning.is_empty() {
            break;
        }
        if state.reasoning_field.is_none() {
            state.reasoning_field = Some(field.to_string());
            out.push(StreamingEvent::ThinkingSignature(field.to_string()));
        }
        out.push(StreamingEvent::ThinkingDelta(reasoning.to_string()));
        break;
    }

    if let Some(content) = delta.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            out.push(StreamingEvent::TextDelta(content.to_string()));
        }
    }

    map_tool_calls(delta, state, &mut out);
    map_reasoning_details(delta, state, &mut out);

    drain(&mut out);
    Ok(out)
}

fn map_tool_calls(delta: &Value, state: &mut ChatMapperState, out: &mut Vec<StreamingEvent>) {
    let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
        return;
    };
    for call in calls {
        map_tool_call(call, state, out);
    }
}

fn map_tool_call(call: &Value, state: &mut ChatMapperState, out: &mut Vec<StreamingEvent>) {
    let index = call.get("index").and_then(Value::as_u64);
    let id = call.get("id").and_then(Value::as_str);
    let function = call.get("function");
    let name = function
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str);
    let args = function
        .and_then(|value| value.get("arguments"))
        .and_then(Value::as_str);

    if let (Some(index), Some(id)) = (index, id) {
        state
            .index_to_id
            .entry(index)
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

    map_tool_signature(call, index, id, state, out);
    if let Some(args) = args.filter(|args| !args.is_empty()) {
        if let Some(id) = tool_call_id(index, id, state) {
            out.push(StreamingEvent::ToolUseInputDelta {
                id,
                delta: args.to_string(),
            });
        }
    }
}

fn map_tool_signature(
    call: &Value,
    index: Option<u64>,
    id: Option<&str>,
    state: &mut ChatMapperState,
    out: &mut Vec<StreamingEvent>,
) {
    let signature = extra_content_signature(call)
        .map(|(namespace, signature)| (namespace.to_string(), signature.to_string()));
    if let (Some(index), Some(signature)) = (index, signature.as_ref()) {
        if id.is_none() && !state.index_to_id.contains_key(&index) {
            state
                .pending_signature_by_index
                .insert(index, signature.clone());
        }
    }
    let signature = signature
        .or_else(|| index.and_then(|index| state.pending_signature_by_index.remove(&index)));
    let Some((namespace, signature)) = signature else {
        return;
    };
    if let Some(target) = tool_call_id(index, id, state) {
        out.push(StreamingEvent::PartSignature {
            provider: state.provider.clone(),
            model: state.model.clone(),
            format: PartSignatureFormat::OpenAiExtraContent { namespace },
            target: Some(target),
            signature,
        });
    }
}

fn tool_call_id(index: Option<u64>, id: Option<&str>, state: &ChatMapperState) -> Option<String> {
    index
        .and_then(|index| state.index_to_id.get(&index))
        .cloned()
        .or_else(|| id.map(str::to_string))
}

fn map_reasoning_details(delta: &Value, state: &ChatMapperState, out: &mut Vec<StreamingEvent>) {
    let Some(details) = delta.get("reasoning_details").and_then(Value::as_array) else {
        return;
    };
    for detail in details {
        let Some(id) = detail.get("id").and_then(Value::as_str) else {
            continue;
        };
        if detail.get("type").and_then(Value::as_str) != Some("reasoning.encrypted") {
            continue;
        }
        out.push(StreamingEvent::PartSignature {
            provider: state.provider.clone(),
            model: state.model.clone(),
            format: PartSignatureFormat::OpenAiReasoningDetail,
            target: Some(id.to_string()),
            signature: detail.to_string(),
        });
    }
}

fn extra_content_signature(tool_call: &Value) -> Option<(&str, &str)> {
    let extra = tool_call.get("extra_content")?.as_object()?;
    for (namespace, value) in extra {
        if let Some(signature) = value.get("thought_signature").and_then(Value::as_str) {
            if !signature.is_empty() {
                return Some((namespace.as_str(), signature));
            }
        }
    }
    None
}

fn usage_from_openai_chat(v: &Value) -> Usage {
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
    use lofi_types::PromptKind;
    use serde_json::json;

    fn model() -> Model {
        Model {
            id: "gpt-4o".to_string(),
            name: "gpt".to_string(),
            provider: "p".to_string(),
            api: lofi_types::Api::OpenAiCompletions,
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
    fn request_includes_stream_and_usage() {
        let req = build_openai_chat_request(&model(), &[], &[]);
        assert_eq!(req["model"], "gpt-4o");
        assert_eq!(req["stream"], true);
        assert_eq!(req["stream_options"]["include_usage"], true);
    }

    #[test]
    fn request_forwards_configured_reasoning_effort_verbatim() {
        for (level, expected) in [
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Custom("minimal".to_string()), "minimal"),
        ] {
            let mut model = model();
            model.thinking = level;
            let req = build_openai_chat_request(&model, &[], &[]);
            assert_eq!(req["reasoning_effort"], expected);
        }
        assert!(build_openai_chat_request(&model(), &[], &[])
            .get("reasoning_effort")
            .is_none());
    }

    #[test]
    fn request_forwards_service_tier_but_omits_auto() {
        for tier in [
            ServiceTier::Flex,
            ServiceTier::Priority,
            ServiceTier::Custom("vip-2025".to_string()),
        ] {
            let mut model = model();
            model.service_tier = tier.clone();
            let req = build_openai_chat_request(&model, &[], &[]);
            assert_eq!(req["service_tier"], tier.as_str());
        }
        assert!(build_openai_chat_request(&model(), &[], &[])
            .get("service_tier")
            .is_none());
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
    fn per_chunk_usage_does_not_suppress_deltas() {
        // DeepSeek-style frame shape: usage attached to a content-bearing chunk.
        // The delta must still be emitted; usage is not turn end here.
        let chunk = json!({
            "choices": [{"delta": {"content": "Hi! Ready"}}],
            "usage": {"completion_tokens": 41, "prompt_tokens": 3602, "total_tokens": 3643}
        });
        assert_eq!(
            map_openai_chat_event(&chunk, &mut ChatMapperState::default()).unwrap(),
            vec![StreamingEvent::TextDelta("Hi! Ready".to_string())]
        );
    }

    #[test]
    fn empty_choices_with_usage_emits_done() {
        // Final usage-only frame (OpenAI include_usage shape).
        let chunk = json!({
            "choices": [],
            "usage": {"completion_tokens": 58, "prompt_tokens": 3602, "total_tokens": 3660}
        });
        assert_eq!(
            map_openai_chat_event(&chunk, &mut ChatMapperState::default()).unwrap(),
            vec![StreamingEvent::Done(lofi_types::Usage {
                input_tokens: 3602,
                output_tokens: 58,
                ..Default::default()
            })]
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
    fn gemini_extra_content_signature_round_trips_on_tool_call() {
        for namespace in ["google", "vertex"] {
            let mut model = model();
            model.provider = "plexus".to_string();
            model.id = "google/gemini-3.7-flash".to_string();
            let mut state = ChatMapperState {
                provider: model.provider.clone(),
                model: model.id.clone(),
                ..ChatMapperState::default()
            };
            let chunk = json!({"choices":[{"delta":{"tool_calls":[{
                "index": 0,
                "id": "call_1",
                "type": "function",
                "function": {"name": "exec", "arguments": "{}"},
                "extra_content": {
                    namespace: {"thought_signature": "opaque-signature"}
                }
            }]}}]});

            let events = map_openai_chat_event(&chunk, &mut state).unwrap();
            let message = crate::assemble_message(&events);
            assert_eq!(
                message.blocks[1],
                ContentBlock::PartSignature {
                    provider: "plexus".to_string(),
                    model: "google/gemini-3.7-flash".to_string(),
                    format: PartSignatureFormat::OpenAiExtraContent {
                        namespace: namespace.to_string(),
                    },
                    signature: "opaque-signature".to_string(),
                }
            );

            let request = build_openai_chat_request(&model, &[message], &[]);
            assert_eq!(
                request["messages"][0]["tool_calls"][0]["extra_content"][namespace]
                    ["thought_signature"],
                "opaque-signature"
            );
        }
    }

    #[test]
    fn delayed_extra_content_signature_targets_its_tool_call() {
        let mut state = ChatMapperState {
            provider: "plexus".to_string(),
            model: "google/gemini-3.7-flash".to_string(),
            ..ChatMapperState::default()
        };
        let signature = json!({"choices":[{"delta":{"tool_calls":[{
            "index": 1,
            "extra_content": {"google": {"thought_signature": "opaque"}}
        }]}}]});
        assert!(map_openai_chat_event(&signature, &mut state)
            .unwrap()
            .is_empty());

        let start = json!({"choices":[{"delta":{"tool_calls":[{
            "index": 1,
            "id": "call_2",
            "function": {"name": "exec", "arguments": "{}"}
        }]}}]});
        let events = map_openai_chat_event(&start, &mut state).unwrap();
        assert!(events.iter().any(|event| matches!(
            event,
            StreamingEvent::PartSignature {
                target: Some(target),
                signature,
                ..
            } if target == "call_2" && signature == "opaque"
        )));
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

    #[test]
    fn user_image_block_serializes_as_multipart_content() {
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
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        let content = &req["messages"][0]["content"];
        assert_eq!(content[0], json!({"type": "text", "text": "describe"}));
        assert_eq!(
            content[1],
            json!({
                "type": "image_url",
                "image_url": {"url": "data:image/jpeg;base64,AQID"},
            })
        );
    }

    #[test]
    fn tool_result_with_images_serializes_image_as_user_message() {
        // Chat-completions `tool` messages only carry string content, so a
        // tool-result image must be re-emitted on a following user message —
        // otherwise the model never sees it.
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
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert_eq!(
            req["messages"][0],
            json!({"role": "tool", "tool_call_id": "c1", "content": "read image.png"})
        );
        assert_eq!(
            req["messages"][1],
            json!({
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,AQID"},
                }],
            })
        );
    }

    #[test]
    fn user_text_without_image_stays_a_plain_string() {
        let msgs = [Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert_eq!(req["messages"][0]["content"], "hello");
    }

    #[test]
    fn reasoning_delta_records_its_field_exactly_once() {
        let mut state = ChatMapperState::default();
        let first = json!({"choices":[{"delta":{"reasoning_content":"hmm,"}}]});
        let second = json!({"choices":[{"delta":{"reasoning_content":" yes"}}]});

        let out1 = map_openai_chat_event(&first, &mut state).unwrap();
        let out2 = map_openai_chat_event(&second, &mut state).unwrap();

        // First chunk: field marker, then the delta. The marker is not
        // repeated on later chunks — it lives on the message once.
        assert_eq!(
            out1[0],
            StreamingEvent::ThinkingSignature("reasoning_content".to_string())
        );
        assert_eq!(out1[1], StreamingEvent::ThinkingDelta("hmm,".to_string()));
        assert_eq!(out1.len(), 2);
        assert_eq!(
            out2,
            vec![StreamingEvent::ThinkingDelta(" yes".to_string())]
        );
    }

    #[test]
    fn alternate_reasoning_fields_also_recorded() {
        for field in ["thinking", "reasoning", "reasoning_text"] {
            let mut state = ChatMapperState::default();
            let chunk = json!({"choices":[{"delta":{field:"hmm"}}]});
            let out = map_openai_chat_event(&chunk, &mut state).unwrap();
            assert_eq!(
                out,
                vec![
                    StreamingEvent::ThinkingSignature(field.to_string()),
                    StreamingEvent::ThinkingDelta("hmm".to_string())
                ]
            );
        }
    }

    #[test]
    fn empty_reasoning_string_emits_nothing() {
        let mut state = ChatMapperState::default();
        let chunk = json!({"choices":[{"delta":{"reasoning_content":""}}]});
        let out = map_openai_chat_event(&chunk, &mut state).unwrap();
        assert!(out.is_empty());
        assert!(state.reasoning_field.is_none());
    }

    #[test]
    fn thinking_field_takes_precedence_over_later_fields() {
        // Servers sometimes return both reasoning_content and reasoning; the
        // first non-empty wins, mirroring Pi.
        let mut state = ChatMapperState::default();
        let chunk = json!({"choices":[{"delta":{"thinking":"a","reasoning":"b"}}]});
        let out = map_openai_chat_event(&chunk, &mut state).unwrap();
        assert_eq!(
            out,
            vec![
                StreamingEvent::ThinkingSignature("thinking".to_string()),
                StreamingEvent::ThinkingDelta("a".to_string())
            ]
        );
    }

    #[test]
    fn thinking_replayed_under_recorded_field() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: "let me think".to_string(),
                signature: Some("reasoning_content".to_string()),
            }],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        let msg = &req["messages"][0];
        assert_eq!(msg["reasoning_content"], "let me think");
        assert!(msg.get("reasoning").is_none());
        assert!(msg.get("reasoning_details").is_none());
    }

    #[test]
    fn thinking_replayed_under_alternate_field() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: "yep".to_string(),
                signature: Some("reasoning".to_string()),
            }],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert_eq!(req["messages"][0]["reasoning"], "yep");
    }

    #[test]
    fn thinking_with_unrecognised_signature_is_not_replayed() {
        // Anthropic-style signatures (any string not in the whitelist) and
        // signature-less blocks stay local.
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    text: "secret plan".to_string(),
                    signature: Some("EogBCkYICxgCKkA...".to_string()),
                },
                ContentBlock::Thinking {
                    text: "unsigned".to_string(),
                    signature: None,
                },
            ],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        let msg = &req["messages"][0];
        assert!(msg.get("reasoning_content").is_none());
        assert!(msg.get("reasoning").is_none());
        assert!(msg.get("thinking").is_none());
    }

    #[test]
    fn multiple_thinking_blocks_joined_under_first_field() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    text: "first".to_string(),
                    signature: Some("reasoning_content".to_string()),
                },
                ContentBlock::Text {
                    text: "answer".to_string(),
                },
                ContentBlock::Thinking {
                    text: "second".to_string(),
                    signature: Some("reasoning_content".to_string()),
                },
            ],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert_eq!(req["messages"][0]["reasoning_content"], "first\nsecond");
    }

    #[test]
    fn whitespace_only_thinking_is_not_replayed() {
        // Pi trims the thinking text before deciding whether to emit. A
        // whitespace-only trace would just be noise on the wire.
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: "  \n ".to_string(),
                signature: Some("reasoning_content".to_string()),
            }],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert!(req["messages"][0].get("reasoning_content").is_none());
    }

    #[test]
    fn reasoning_text_field_replayed_verbatim() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: "thinking".to_string(),
                signature: Some("reasoning_text".to_string()),
            }],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert_eq!(req["messages"][0]["reasoning_text"], "thinking");
    }

    #[test]
    fn empty_thinking_text_is_not_replayed() {
        let msgs = [Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: String::new(),
                signature: Some("reasoning_content".to_string()),
            }],
            kind: PromptKind::default(),
        }];
        let req = build_openai_chat_request(&model(), &msgs, &[]);
        assert!(req["messages"][0].get("reasoning_content").is_none());
    }
}
