use lofi_error::Result;
use lofi_types::{ContentBlock, Message, Model, Role, StreamingEvent};
use serde_json::Value;
use std::borrow::Cow;

use crate::ToolSchema;

pub(crate) mod anthropic_messages;
pub(crate) mod google_generative_ai;
pub(crate) mod openai_completions;
pub(crate) mod openai_responses;

pub(crate) use anthropic_messages::AnthropicMessagesIr;
pub(crate) use google_generative_ai::GoogleGenerativeAiIr;
pub(crate) use openai_completions::OpenAiCompletionsIr;
pub(crate) use openai_responses::OpenAiResponsesIr;

/// Base64-encodes an image payload for the wire. All protocols take
/// image bytes base64-encoded; the in-memory block stores raw bytes, so the
/// encoding happens here at the IR boundary rather than on the block itself.
pub(crate) fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Converts provider-neutral messages to one protocol's wire request and
/// maps that protocol's streaming payloads back to provider-neutral events.
pub(crate) trait ProtocolIr: Send + 'static {
    type State: Default + Send + 'static;

    fn build_request(model: &Model, messages: &[Message], tools: &[ToolSchema]) -> Value {
        match messages_for_model(model, messages) {
            Some(messages) => {
                Self::build_request_inner(model, messages.iter().map(Cow::as_ref), tools)
            }
            None => Self::build_request_inner(model, messages.iter(), tools),
        }
    }

    fn build_request_inner<'a>(
        model: &Model,
        messages: impl Iterator<Item = &'a Message>,
        tools: &[ToolSchema],
    ) -> Value;

    fn new_state(_model: &Model) -> Self::State {
        Self::State::default()
    }

    fn map_event(
        event: Option<&str>,
        data: &Value,
        state: &mut Self::State,
    ) -> Result<Vec<StreamingEvent>>;

    fn on_eof(_state: &Self::State) -> Result<()> {
        Ok(())
    }

    fn handles_done_marker() -> bool {
        true
    }

    fn defer_done_until_transport_end() -> bool {
        false
    }
}

fn part_signature_matches(
    model: &Model,
    provider: &str,
    signed_model: &str,
    format: &lofi_types::PartSignatureFormat,
    signature: &str,
) -> bool {
    use lofi_types::PartSignatureFormat;

    !signature.is_empty()
        && provider == model.provider
        && signed_model == model.id
        && matches!(
            (format, model.api),
            (
                PartSignatureFormat::Google,
                lofi_types::Api::GoogleGenerativeAi
            ) | (
                PartSignatureFormat::OpenAiExtraContent { .. }
                    | PartSignatureFormat::OpenAiReasoningDetail,
                lofi_types::Api::OpenAiCompletions
            )
        )
}

fn message_has_reasoning(message: &Message) -> bool {
    message.blocks.iter().any(|block| {
        matches!(
            block,
            ContentBlock::Thinking { .. } | ContentBlock::PartSignature { .. }
        )
    })
}

fn message_has_opaque_thinking(message: &Message) -> bool {
    message.blocks.iter().any(|block| match block {
        ContentBlock::Thinking {
            signature,
            redacted,
            ..
        } => *redacted || signature.is_some(),
        _ => false,
    })
}

fn message_has_foreign_reasoning(message: &Message, model: &Model) -> bool {
    if message.role != Role::Assistant {
        return false;
    }
    match &message.origin {
        Some(origin) => message_has_reasoning(message) && !origin.matches(model),
        // Opaque thinking without provenance cannot be validated safely.
        None => message_has_opaque_thinking(message),
    }
}

fn block_has_invalid_part_signature(block: &ContentBlock, model: &Model) -> bool {
    match block {
        ContentBlock::PartSignature {
            provider,
            model: signed_model,
            format,
            signature,
        } => !part_signature_matches(model, provider, signed_model, format, signature),
        _ => false,
    }
}

fn message_needs_sanitization(message: &Message, model: &Model) -> bool {
    message_has_foreign_reasoning(message, model)
        || (message.role == Role::Assistant
            && message
                .blocks
                .iter()
                .any(|block| block_has_invalid_part_signature(block, model)))
}

fn messages_for_model<'a>(model: &Model, messages: &'a [Message]) -> Option<Vec<Cow<'a, Message>>> {
    if !messages
        .iter()
        .any(|message| message_needs_sanitization(message, model))
    {
        return None;
    }

    Some(
        messages
            .iter()
            .map(|message| {
                if !message_needs_sanitization(message, model) {
                    return Cow::Borrowed(message);
                }
                let foreign_reasoning = message_has_foreign_reasoning(message, model);
                let blocks = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Thinking { text, redacted, .. } if foreign_reasoning => {
                            (!redacted && !text.trim().is_empty())
                                .then(|| ContentBlock::Text { text: text.clone() })
                        }
                        ContentBlock::PartSignature { .. }
                            if foreign_reasoning
                                || block_has_invalid_part_signature(block, model) =>
                        {
                            None
                        }
                        _ => Some(block.clone()),
                    })
                    .collect();
                Cow::Owned(Message {
                    role: message.role,
                    blocks,
                    origin: message.origin.clone(),
                    kind: message.kind,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{
        Api, ModelOrigin, PartSignatureFormat, PromptKind, ServiceTier, ThinkingLevel,
    };

    fn model(provider: &str, id: &str, api: Api) -> Model {
        Model {
            id: id.to_string(),
            name: id.to_string(),
            provider: provider.to_string(),
            api,
            reasoning: true,
            thinking: ThinkingLevel::Medium,
            service_tier: ServiceTier::Auto,
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

    fn assistant(model: &Model, blocks: Vec<ContentBlock>) -> Message {
        Message {
            origin: Some(ModelOrigin::from(model)),
            role: Role::Assistant,
            blocks,
            kind: PromptKind::User,
        }
    }

    #[test]
    fn same_model_keeps_encrypted_reasoning() {
        let target = model("openai", "gpt-5", Api::OpenAiResponses);
        let messages = [assistant(
            &target,
            vec![ContentBlock::Thinking {
                text: "summary".to_string(),
                signature: Some("encrypted".to_string()),
                redacted: false,
            }],
        )];

        assert!(messages_for_model(&target, &messages).is_none());
    }

    #[test]
    fn empty_part_signature_is_dropped_for_same_model() {
        let target = model("openai", "gpt-5", Api::OpenAiCompletions);
        let messages = [assistant(
            &target,
            vec![ContentBlock::PartSignature {
                provider: target.provider.clone(),
                model: target.id.clone(),
                format: PartSignatureFormat::OpenAiExtraContent {
                    namespace: "google".to_string(),
                },
                signature: String::new(),
            }],
        )];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert!(prepared[0].blocks.is_empty());
    }

    #[test]
    fn legacy_matching_part_signature_is_kept_without_message_origin() {
        let target = model("google", "gemini", Api::GoogleGenerativeAi);
        let messages = [Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![ContentBlock::PartSignature {
                provider: target.provider.clone(),
                model: target.id.clone(),
                format: PartSignatureFormat::Google,
                signature: "b3BhcXVl".to_string(),
            }],
            kind: PromptKind::User,
        }];

        assert!(messages_for_model(&target, &messages).is_none());
    }

    #[test]
    fn explicit_foreign_origin_overrides_matching_part_signature() {
        let source = model("google", "gemini-pro", Api::GoogleGenerativeAi);
        let target = model("google", "gemini-flash", Api::GoogleGenerativeAi);
        let messages = [assistant(
            &source,
            vec![ContentBlock::PartSignature {
                provider: target.provider.clone(),
                model: target.id.clone(),
                format: PartSignatureFormat::Google,
                signature: "b3BhcXVl".to_string(),
            }],
        )];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert!(prepared[0].blocks.is_empty());
    }

    #[test]
    fn foreign_reasoning_rebuilds_only_affected_messages() {
        let source = model("openai", "gpt-5", Api::OpenAiResponses);
        let target = model("openai", "gpt-5-mini", Api::OpenAiResponses);
        let messages = [
            Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Image {
                    bytes: vec![0; 1024],
                    media_type: "image/png".to_string(),
                }],
                kind: PromptKind::User,
            },
            assistant(
                &source,
                vec![ContentBlock::Thinking {
                    text: "summary".to_string(),
                    signature: Some("encrypted".to_string()),
                    redacted: false,
                }],
            ),
        ];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert!(matches!(prepared[0], Cow::Borrowed(_)));
        assert!(matches!(prepared[1], Cow::Owned(_)));
    }

    #[test]
    fn different_model_converts_visible_reasoning_to_text() {
        let source = model("openai", "gpt-5", Api::OpenAiResponses);
        let target = model("openai", "gpt-5-mini", Api::OpenAiResponses);
        let messages = [assistant(
            &source,
            vec![
                ContentBlock::Thinking {
                    text: "summary".to_string(),
                    signature: Some("encrypted".to_string()),
                    redacted: false,
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"code": "return 1"}),
                },
            ],
        )];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert_eq!(
            prepared[0].blocks,
            vec![
                ContentBlock::Text {
                    text: "summary".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call-1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"code": "return 1"}),
                },
            ]
        );
        assert!(matches!(
            messages[0].blocks[0],
            ContentBlock::Thinking { .. }
        ));
    }

    #[test]
    fn different_api_with_same_names_is_foreign() {
        let source = model("gateway", "model", Api::OpenAiResponses);
        let target = model("gateway", "model", Api::OpenAiCompletions);
        let messages = [assistant(
            &source,
            vec![ContentBlock::Thinking {
                text: "summary".to_string(),
                signature: Some("encrypted".to_string()),
                redacted: false,
            }],
        )];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert_eq!(
            prepared[0].blocks,
            vec![ContentBlock::Text {
                text: "summary".to_string(),
            }]
        );
    }

    #[test]
    fn signature_after_foreign_thinking_is_dropped() {
        let source = model("openai", "gpt-5", Api::OpenAiResponses);
        let target = model("google", "gemini-3.7-pro", Api::GoogleGenerativeAi);
        let messages = [assistant(
            &source,
            vec![
                ContentBlock::Thinking {
                    text: "summary".to_string(),
                    signature: Some("encrypted".to_string()),
                    redacted: false,
                },
                ContentBlock::PartSignature {
                    provider: target.provider.clone(),
                    model: target.id.clone(),
                    format: PartSignatureFormat::Google,
                    signature: "b3BhcXVl".to_string(),
                },
            ],
        )];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert_eq!(
            prepared[0].blocks,
            vec![ContentBlock::Text {
                text: "summary".to_string(),
            }]
        );
    }

    #[test]
    fn foreign_opaque_reasoning_and_part_signatures_are_dropped() {
        let source = model("anthropic", "claude", Api::AnthropicMessages);
        let target = model("google", "gemini", Api::GoogleGenerativeAi);
        let messages = [assistant(
            &source,
            vec![
                ContentBlock::Thinking {
                    text: "[Reasoning redacted]".to_string(),
                    signature: Some("opaque".to_string()),
                    redacted: true,
                },
                ContentBlock::PartSignature {
                    provider: source.provider.clone(),
                    model: source.id.clone(),
                    format: PartSignatureFormat::OpenAiReasoningDetail,
                    signature: "opaque detail".to_string(),
                },
                ContentBlock::Text {
                    text: "answer".to_string(),
                },
            ],
        )];

        let prepared = messages_for_model(&target, &messages).unwrap();

        assert_eq!(
            prepared[0].blocks,
            vec![ContentBlock::Text {
                text: "answer".to_string(),
            }]
        );
    }
}
