//! Native Google Generative AI request and streaming response mapping.

use std::collections::HashMap;

use base64::Engine as _;
use lofi_error::{Error, Result};
use lofi_types::{
    ContentBlock, Message, Model, PartSignatureFormat, Role, StreamingEvent, ThinkingLevel, Usage,
};
use serde_json::{json, Map, Value};

use super::ProtocolIr;
use crate::ToolSchema;

pub(crate) struct GoogleGenerativeAiIr;

#[derive(Default)]
pub(crate) struct GoogleMapperState {
    provider: String,
    model: String,
    next_tool_id: u64,
    usage: Usage,
    done: bool,
}

impl ProtocolIr for GoogleGenerativeAiIr {
    type State = GoogleMapperState;

    fn build_request_inner<'a>(
        model: &Model,
        messages: impl Iterator<Item = &'a Message>,
        tools: &[ToolSchema],
    ) -> Value {
        build_request(model, messages, tools)
    }

    fn new_state(model: &Model) -> Self::State {
        GoogleMapperState {
            provider: model.provider.clone(),
            model: model.id.clone(),
            ..GoogleMapperState::default()
        }
    }

    fn map_event(
        _event: Option<&str>,
        data: &Value,
        state: &mut Self::State,
    ) -> Result<Vec<StreamingEvent>> {
        map_event(data, state)
    }

    fn on_eof(state: &Self::State) -> Result<()> {
        if state.done {
            Ok(())
        } else {
            Err(Error::Provider(
                "Google stream ended without a finish reason".to_string(),
            ))
        }
    }

    fn handles_done_marker() -> bool {
        false
    }

    fn defer_done_until_transport_end() -> bool {
        true
    }
}

fn build_request<'a>(
    model: &Model,
    messages: impl IntoIterator<Item = &'a Message>,
    tools: &[ToolSchema],
) -> Value {
    let messages: Vec<&Message> = messages.into_iter().collect();
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let tool_names = tool_names(&messages);

    for message in messages {
        match message.role {
            Role::System => collect_system_parts(&message.blocks, &mut system),
            Role::User => {
                let parts = user_parts(&message.blocks);
                if !parts.is_empty() {
                    push_content(&mut contents, "user", parts);
                }
            }
            Role::Assistant => {
                let parts = assistant_parts(&message.blocks, model);
                if !parts.is_empty() {
                    push_content(&mut contents, "model", parts);
                }
            }
            Role::Tool => {
                let parts = tool_result_parts(&message.blocks, &tool_names, model);
                if !parts.is_empty() {
                    push_content(&mut contents, "user", parts);
                }
            }
        }
    }

    let mut body = json!({"contents": contents});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": system});
    }
    if !tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": tools.iter().map(|tool| json!({
                "name": tool.name,
                "description": tool.description,
                "parametersJsonSchema": tool.input_schema,
            })).collect::<Vec<_>>()
        }]);
    }

    let generation = generation_config(model);
    if !generation.is_empty() {
        body["generationConfig"] = Value::Object(generation);
    }
    body
}

fn push_content(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if let Some(previous_parts) = contents.last_mut().and_then(|content| {
        (content.get("role").and_then(Value::as_str) == Some(role))
            .then(|| content.get_mut("parts"))
            .flatten()
            .and_then(Value::as_array_mut)
    }) {
        previous_parts.extend(parts);
    } else {
        contents.push(json!({"role": role, "parts": parts}));
    }
}

fn collect_system_parts(blocks: &[ContentBlock], out: &mut Vec<Value>) {
    for block in blocks {
        if let ContentBlock::Text { text } = block {
            out.push(json!({"text": text}));
        }
    }
}

fn user_parts(blocks: &[ContentBlock]) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(json!({"text": text})),
            ContentBlock::Image { bytes, media_type } => Some(json!({
                "inlineData": {"mimeType": media_type, "data": super::b64(bytes)}
            })),
            _ => None,
        })
        .collect()
}

fn assistant_parts(blocks: &[ContentBlock], model: &Model) -> Vec<Value> {
    let mut parts = Vec::new();
    for (index, block) in blocks.iter().enumerate() {
        match block {
            ContentBlock::Text { text } => parts.push(json!({"text": text})),
            ContentBlock::Thinking { text, .. } => {
                let same_model_signature = blocks.get(index + 1).is_some_and(|next| {
                    matches!(
                        next,
                        ContentBlock::PartSignature {
                            provider,
                            model: signed_model,
                            format: PartSignatureFormat::Google,
                            ..
                        } if provider == &model.provider && signed_model == &model.id
                    )
                });
                if !text.is_empty() || same_model_signature {
                    let mut part = json!({"text": text});
                    if same_model_signature {
                        part["thought"] = json!(true);
                    }
                    parts.push(part);
                }
            }
            ContentBlock::ToolUse { id, name, input } => {
                // `functionCall.args` must be a Struct; sessions recorded
                // before the assembler normalized empty inputs can carry
                // `null` from the durable log.
                let args = if input.is_object() {
                    input.clone()
                } else {
                    json!({})
                };
                let mut part = json!({"functionCall": {"name": name, "args": args}});
                if requires_tool_call_id(&model.id) {
                    part["functionCall"]["id"] = json!(normalize_tool_id(id));
                }
                if is_gemini_3(&model.id) {
                    part["thoughtSignature"] = json!("skip_thought_signature_validator");
                }
                parts.push(part);
            }
            ContentBlock::PartSignature {
                provider,
                model: signed_model,
                format,
                signature,
            } => {
                if provider == &model.provider
                    && signed_model == &model.id
                    && format == &PartSignatureFormat::Google
                {
                    if let (Some(part), Some(signature)) =
                        (parts.last_mut(), valid_signature(Some(signature)))
                    {
                        part["thoughtSignature"] = json!(signature);
                    }
                }
            }
            ContentBlock::Image { bytes, media_type } => parts.push(json!({
                "inlineData": {"mimeType": media_type, "data": super::b64(bytes)}
            })),
            ContentBlock::ToolResult { .. } => {}
        }
    }
    parts
}

fn tool_names<'a>(messages: &[&'a Message]) -> HashMap<&'a str, &'a str> {
    let mut names = HashMap::new();
    for message in messages {
        for block in &message.blocks {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.as_str(), name.as_str());
            }
        }
    }
    names
}

fn tool_result_parts(
    blocks: &[ContentBlock],
    names: &HashMap<&str, &str>,
    model: &Model,
) -> Vec<Value> {
    blocks
        .iter()
        .filter_map(|block| {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                images,
            } = block
            else {
                return None;
            };
            let name = names.get(tool_use_id.as_str()).copied().unwrap_or("tool");
            let key = if *is_error { "error" } else { "output" };
            let mut response = Map::new();
            response.insert(key.to_string(), Value::String(content.clone()));
            let mut function_response = json!({"name": name, "response": response});
            if requires_tool_call_id(&model.id) {
                function_response["id"] = json!(normalize_tool_id(tool_use_id));
            }
            if !images.is_empty() {
                function_response["parts"] = Value::Array(
                    images
                        .iter()
                        .map(|image| {
                            json!({"inlineData": {
                                "mimeType": image.media_type,
                                "data": super::b64(&image.bytes)
                            }})
                        })
                        .collect(),
                );
            }
            Some(json!({"functionResponse": function_response}))
        })
        .collect()
}

fn generation_config(model: &Model) -> Map<String, Value> {
    let mut config = Map::new();
    if let Some(max_tokens) = model.max_tokens {
        config.insert("maxOutputTokens".to_string(), json!(max_tokens));
    }
    if model.reasoning {
        let thinking = match model.thinking {
            ThinkingLevel::Off if is_gemini_3_pro(&model.id) => json!({"thinkingLevel": "LOW"}),
            ThinkingLevel::Off if is_gemini_3(&model.id) => json!({"thinkingLevel": "MINIMAL"}),
            ThinkingLevel::Off => json!({"thinkingBudget": 0}),
            ref level if is_gemini_3(&model.id) => json!({
                "includeThoughts": true,
                "thinkingLevel": thinking_level(level, &model.id),
            }),
            ref level => json!({
                "includeThoughts": true,
                "thinkingBudget": thinking_budget(level, &model.id),
            }),
        };
        config.insert("thinkingConfig".to_string(), thinking);
    }
    config
}

fn map_event(data: &Value, state: &mut GoogleMapperState) -> Result<Vec<StreamingEvent>> {
    if let Some(error) = data.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Google API error");
        return Err(Error::Provider(message.to_string()));
    }

    // A prompt the safety filter rejects carries no candidate and no finish
    // reason; surface the block reason instead of the generic EOF error.
    if let Some(reason) = data
        .pointer("/promptFeedback/blockReason")
        .and_then(Value::as_str)
    {
        return Err(Error::Provider(format!(
            "Google blocked the prompt: {reason}"
        )));
    }

    update_usage(data.get("usageMetadata"), &mut state.usage);
    let mut out = Vec::new();
    let candidate = data
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|candidates| candidates.first());

    if let Some(parts) = candidate
        .and_then(|candidate| candidate.pointer("/content/parts"))
        .and_then(Value::as_array)
    {
        for part in parts {
            map_part(part, state, &mut out)?;
        }
    }

    if let Some(reason) = candidate
        .and_then(|candidate| candidate.get("finishReason"))
        .and_then(Value::as_str)
    {
        match reason {
            "STOP" | "MAX_TOKENS" => {
                if !state.done {
                    state.done = true;
                    out.push(StreamingEvent::Done {
                        usage: state.usage,
                        stop_reason: Some(if reason == "MAX_TOKENS" {
                            lofi_types::StopReason::MaxTokens
                        } else {
                            lofi_types::StopReason::EndTurn
                        }),
                    });
                }
            }
            other => {
                return Err(Error::Provider(format!(
                    "Google stopped generation with {other}"
                )));
            }
        }
    }
    Ok(out)
}

fn map_part(
    part: &Value,
    state: &mut GoogleMapperState,
    out: &mut Vec<StreamingEvent>,
) -> Result<()> {
    if let Some(text) = part.get("text").and_then(Value::as_str) {
        if part.get("thought").and_then(Value::as_bool) == Some(true) {
            out.push(StreamingEvent::ThinkingDelta(text.to_string()));
            if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                out.push(StreamingEvent::PartSignature {
                    provider: state.provider.clone(),
                    model: state.model.clone(),
                    format: PartSignatureFormat::Google,
                    target: None,
                    signature: signature.to_string(),
                });
            }
        } else {
            out.push(StreamingEvent::TextDelta(text.to_string()));
            if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
                out.push(StreamingEvent::PartSignature {
                    provider: state.provider.clone(),
                    model: state.model.clone(),
                    format: PartSignatureFormat::Google,
                    target: None,
                    signature: signature.to_string(),
                });
            }
        }
    } else if let Some(call) = part.get("functionCall") {
        let name = call.get("name").and_then(Value::as_str).unwrap_or("");
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map_or_else(
                || {
                    state.next_tool_id += 1;
                    format!("{name}_{}", state.next_tool_id)
                },
                ToString::to_string,
            );
        let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
        let delta = serde_json::to_string(&args)
            .map_err(|error| Error::Provider(format!("invalid Google function call: {error}")))?;
        out.push(StreamingEvent::ToolUseStart {
            id: id.clone(),
            name: name.to_string(),
        });
        out.push(StreamingEvent::ToolUseInputDelta {
            id: id.clone(),
            delta,
        });
        out.push(StreamingEvent::ToolUseEnd { id: id.clone() });
        if let Some(signature) = part.get("thoughtSignature").and_then(Value::as_str) {
            out.push(StreamingEvent::PartSignature {
                provider: state.provider.clone(),
                model: state.model.clone(),
                format: PartSignatureFormat::Google,
                target: Some(id),
                signature: signature.to_string(),
            });
        }
    }
    Ok(())
}

fn update_usage(metadata: Option<&Value>, usage: &mut Usage) {
    let Some(metadata) = metadata else { return };
    let cached = token_count(metadata, "cachedContentTokenCount");
    usage.input_tokens = token_count(metadata, "promptTokenCount").saturating_sub(cached);
    usage.cache_read_tokens = cached;
    usage.output_tokens = token_count(metadata, "candidatesTokenCount")
        .saturating_add(token_count(metadata, "thoughtsTokenCount"));
}

fn token_count(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn valid_signature(signature: Option<&str>) -> Option<&str> {
    let signature = signature.filter(|value| !value.is_empty())?;
    base64::engine::general_purpose::STANDARD
        .decode(signature)
        .ok()
        .map(|_| signature)
}

fn normalize_tool_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn gemini_major(id: &str) -> Option<u64> {
    id.to_ascii_lowercase()
        .strip_prefix("gemini-")?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn is_gemini_3(id: &str) -> bool {
    gemini_major(id).is_some_and(|major| major >= 3)
}

fn is_gemini_3_pro(id: &str) -> bool {
    is_gemini_3(id) && id.to_ascii_lowercase().contains("-pro")
}

fn requires_tool_call_id(id: &str) -> bool {
    is_gemini_3(id) || id.starts_with("claude-") || id.starts_with("gpt-oss-")
}

fn thinking_level(level: &ThinkingLevel, model_id: &str) -> &'static str {
    match level {
        ThinkingLevel::Low if is_gemini_3_pro(model_id) => "LOW",
        ThinkingLevel::Low => "LOW",
        ThinkingLevel::Medium if is_gemini_3_pro(model_id) => "HIGH",
        ThinkingLevel::Medium => "MEDIUM",
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Custom(_) => "HIGH",
        ThinkingLevel::Off => "MINIMAL",
    }
}

fn thinking_budget(level: &ThinkingLevel, model_id: &str) -> i64 {
    let high = if model_id.contains("2.5-pro") {
        32_768
    } else {
        24_576
    };
    match level {
        ThinkingLevel::Low => 2_048,
        ThinkingLevel::Medium => 8_192,
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Custom(_) => high,
        ThinkingLevel::Off => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{Api, PromptKind, ServiceTier};

    fn model(id: &str) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            provider: "google".to_string(),
            api: Api::GoogleGenerativeAi,
            reasoning: true,
            thinking: ThinkingLevel::High,
            service_tier: ServiceTier::Auto,
            supports_image: true,
            context_window: None,
            max_tokens: Some(8192),
            base_url: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        }
    }

    #[test]
    fn builds_native_request_with_tools_and_thinking() {
        let request = build_request(
            &model("gemini-3.7-flash"),
            &[Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "hi".into() }],
                kind: PromptKind::User,
            }],
            &[ToolSchema {
                name: "exec".into(),
                description: "Run code".into(),
                input_schema: json!({"type": "object"}),
            }],
        );
        assert_eq!(request["contents"][0]["role"], "user");
        assert_eq!(
            request["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            json!({"type": "object"})
        );
        assert_eq!(
            request["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "HIGH"
        );
        assert_eq!(
            request["generationConfig"]["thinkingConfig"]["includeThoughts"],
            true
        );
    }

    #[test]
    fn prompt_feedback_block_reason_fails_with_the_reason() {
        let mut state = GoogleMapperState::default();
        let err = map_event(
            &json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}}),
            &mut state,
        )
        .unwrap_err();
        assert!(matches!(err, Error::Provider(message) if message.contains("PROHIBITED_CONTENT")));
    }

    #[test]
    fn function_call_with_null_args_serializes_an_empty_object() {
        let request = build_request(
            &model("gemini-3.7-flash"),
            &[Message {
                origin: None,
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "exec".into(),
                    input: Value::Null,
                }],
                kind: PromptKind::default(),
            }],
            &[],
        );

        assert_eq!(
            request["contents"][0]["parts"][0]["functionCall"]["args"],
            json!({})
        );
    }

    #[test]
    fn merges_consecutive_contents_with_the_same_role() {
        let request = build_request(
            &model("gemini-3.7-flash"),
            &[
                Message {
                    origin: None,
                    role: Role::User,
                    blocks: vec![ContentBlock::Text { text: "one".into() }],
                    kind: PromptKind::User,
                },
                Message {
                    origin: None,
                    role: Role::User,
                    blocks: vec![ContentBlock::Text { text: "two".into() }],
                    kind: PromptKind::User,
                },
            ],
            &[],
        );

        assert_eq!(request["contents"].as_array().unwrap().len(), 1);
        assert_eq!(
            request["contents"][0]["parts"],
            json!([{"text": "one"}, {"text": "two"}])
        );
    }

    #[test]
    fn maps_function_call_and_replays_signature_for_same_model() {
        let signature = base64::engine::general_purpose::STANDARD.encode("opaque");
        let source_model = model("gemini-3.7-flash");
        let mut state = GoogleGenerativeAiIr::new_state(&source_model);
        let events = map_event(
            &json!({
                "candidates": [{
                    "content": {"parts": [{
                        "functionCall": {"id": "call_1", "name": "exec", "args": {"code": "1+1"}},
                        "thoughtSignature": signature,
                    }]},
                    "finishReason": "STOP"
                }],
                "usageMetadata": {
                    "promptTokenCount": 10,
                    "cachedContentTokenCount": 3,
                    "candidatesTokenCount": 4,
                    "thoughtsTokenCount": 5
                }
            }),
            &mut state,
        )
        .unwrap();
        let message = crate::message_assembler::assemble_message(&events);
        let request = build_request(&source_model, std::slice::from_ref(&message), &[]);
        let part = &request["contents"][0]["parts"][0];
        assert_eq!(part["functionCall"]["id"], "call_1");
        assert_eq!(part["thoughtSignature"], signature);
        assert!(matches!(
            events.last(),
            Some(StreamingEvent::Done {
                usage: Usage {
                    input_tokens: 7,
                    output_tokens: 9,
                    cache_read_tokens: 3,
                    ..
                },
                ..
            })
        ));

        let request = build_request(&model("gemini-3.7-pro"), &[message], &[]);
        assert_eq!(
            request["contents"][0]["parts"][0]["thoughtSignature"],
            "skip_thought_signature_validator"
        );
    }

    #[test]
    fn maps_finish_reason_into_done() {
        for (raw, expected) in [
            ("STOP", lofi_types::StopReason::EndTurn),
            ("MAX_TOKENS", lofi_types::StopReason::MaxTokens),
        ] {
            let mut state = GoogleGenerativeAiIr::new_state(&model("gemini-3.7-flash"));
            let events = map_event(
                &json!({
                    "candidates": [{"content": {"parts": [{"text": "x"}]}, "finishReason": raw}]
                }),
                &mut state,
            )
            .unwrap();
            let done = events.into_iter().last().unwrap();
            let StreamingEvent::Done { stop_reason, .. } = done else {
                panic!("expected Done");
            };
            assert_eq!(stop_reason, Some(expected), "finishReason {raw}");
        }
    }

    #[test]
    fn maps_thought_summary_and_signature() {
        let mut state = GoogleGenerativeAiIr::new_state(&model("gemini-3.7-flash"));
        let events = map_event(
            &json!({
                "candidates": [{"content": {"parts": [{
                    "text": "considering",
                    "thought": true,
                    "thoughtSignature": "c2ln"
                }]}}]
            }),
            &mut state,
        )
        .unwrap();
        assert_eq!(
            events,
            vec![
                StreamingEvent::ThinkingDelta("considering".into()),
                StreamingEvent::PartSignature {
                    provider: "google".into(),
                    model: "gemini-3.7-flash".into(),
                    format: PartSignatureFormat::Google,
                    target: None,
                    signature: "c2ln".into(),
                },
            ]
        );
    }
}
