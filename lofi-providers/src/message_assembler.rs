use std::collections::HashMap;

use lofi_types::{ContentBlock, Message, PartSignatureFormat, Role, StreamingEvent};
use serde_json::Value;

#[derive(Default)]
pub struct MessageAssembler {
    order: Vec<Slot>,
    tools: Vec<ToolBuilder>,
    pending_tool_sigs: HashMap<String, PartSig>,
}

#[derive(Debug)]
struct ToolBuilder {
    id: String,
    name: String,
    input: String,
    ended: bool,
    sig: Option<PartSig>,
}

#[derive(Debug)]
struct PartSig {
    provider: String,
    model: String,
    format: PartSignatureFormat,
    signature: String,
}

enum Slot {
    Text {
        text: String,
        sig: Option<PartSig>,
    },
    Thinking {
        text: String,
        sig: Option<String>,
        part_sig: Option<PartSig>,
    },
    Tool(usize),
}

impl MessageAssembler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: StreamingEvent) {
        match event {
            StreamingEvent::TextDelta(d) => match self.order.last_mut() {
                Some(Slot::Text { text, .. }) => text.push_str(&d),
                _ => self.order.push(Slot::Text { text: d, sig: None }),
            },
            StreamingEvent::ThinkingDelta(d) => match self.order.last_mut() {
                Some(Slot::Thinking { text, .. }) => text.push_str(&d),
                _ => self.order.push(Slot::Thinking {
                    text: d,
                    sig: None,
                    part_sig: None,
                }),
            },
            StreamingEvent::ThinkingSignature(s) => {
                if let Some(Slot::Thinking { sig, .. }) = self
                    .order
                    .iter_mut()
                    .rev()
                    .find(|slot| matches!(slot, Slot::Thinking { .. }))
                {
                    *sig = Some(s);
                } else {
                    self.order.push(Slot::Thinking {
                        text: String::new(),
                        sig: Some(s),
                        part_sig: None,
                    });
                }
            }
            StreamingEvent::PartSignature {
                provider,
                model,
                format,
                target,
                signature,
            } => {
                let sig = PartSig {
                    provider,
                    model,
                    format,
                    signature,
                };
                if let Some(target) = target {
                    if let Some(tool) = self.tools.iter_mut().rfind(|tool| tool.id == target) {
                        tool.sig = Some(sig);
                    } else {
                        self.pending_tool_sigs.insert(target, sig);
                    }
                } else {
                    match self.order.last_mut() {
                        Some(Slot::Text { sig: slot_sig, .. }) => *slot_sig = Some(sig),
                        Some(Slot::Thinking { part_sig, .. }) => *part_sig = Some(sig),
                        Some(Slot::Tool(i)) => self.tools[*i].sig = Some(sig),
                        None => {}
                    }
                }
            }
            StreamingEvent::ToolUseStart { id, name } => {
                let sig = self.pending_tool_sigs.remove(&id);
                self.tools.push(ToolBuilder {
                    id,
                    name,
                    input: String::new(),
                    ended: false,
                    sig,
                });
                self.order.push(Slot::Tool(self.tools.len() - 1));
            }
            StreamingEvent::ToolUseInputDelta { id, delta } => {
                let pos = self.tools.iter().rposition(|t| !t.ended && t.id == id);
                if let Some(i) = pos {
                    self.tools[i].input.push_str(&delta);
                }
            }
            StreamingEvent::ToolUseEnd { id } => {
                let pos = self.tools.iter().rposition(|t| !t.ended && t.id == id);
                if let Some(i) = pos {
                    self.tools[i].ended = true;
                }
            }
            StreamingEvent::Done(_) | StreamingEvent::Error(_) => {}
        }
    }

    #[must_use]
    pub fn finish(self) -> Message {
        let mut blocks = Vec::with_capacity(self.order.len());
        for slot in self.order {
            match slot {
                Slot::Text { text, sig } => {
                    blocks.push(ContentBlock::Text { text });
                    if let Some(sig) = sig {
                        blocks.push(ContentBlock::PartSignature {
                            provider: sig.provider,
                            model: sig.model,
                            format: sig.format,
                            signature: sig.signature,
                        });
                    }
                }
                Slot::Thinking {
                    text,
                    sig,
                    part_sig,
                } => {
                    blocks.push(ContentBlock::Thinking {
                        text,
                        signature: sig,
                    });
                    if let Some(sig) = part_sig {
                        blocks.push(ContentBlock::PartSignature {
                            provider: sig.provider,
                            model: sig.model,
                            format: sig.format,
                            signature: sig.signature,
                        });
                    }
                }
                Slot::Tool(i) => {
                    let t = &self.tools[i];
                    let input = if t.input.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str(&t.input).unwrap_or(Value::Null)
                    };
                    blocks.push(ContentBlock::ToolUse {
                        id: t.id.clone(),
                        name: t.name.clone(),
                        input,
                    });
                    if let Some(signature) = &t.sig {
                        blocks.push(ContentBlock::PartSignature {
                            provider: signature.provider.clone(),
                            model: signature.model.clone(),
                            format: signature.format.clone(),
                            signature: signature.signature.clone(),
                        });
                    }
                }
            }
        }
        Message {
            role: Role::Assistant,
            blocks,
            kind: lofi_types::PromptKind::User,
        }
    }
}

#[must_use]
pub fn assemble_message(events: &[StreamingEvent]) -> Message {
    let mut assembler = MessageAssembler::new();
    for event in events {
        assembler.push(event.clone());
    }
    assembler.finish()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::Usage;

    #[test]
    fn assembles_text_only_message() {
        let events = [
            StreamingEvent::TextDelta("Hello".to_string()),
            StreamingEvent::TextDelta(", world".to_string()),
            StreamingEvent::Done(Usage::default()),
        ];
        let m = assemble_message(&events);
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(
            m.blocks[0],
            ContentBlock::Text {
                text: "Hello, world".to_string()
            }
        );
    }

    #[test]
    fn assembles_tool_use_with_parsed_input() {
        let events = [
            StreamingEvent::TextDelta("running".to_string()),
            StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "call_1".to_string(),
                delta: "{\"code\":\"1+".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "call_1".to_string(),
                delta: "1\"}".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "call_1".to_string(),
            },
        ];
        let m = assemble_message(&events);
        assert_eq!(m.blocks.len(), 2);
        let ContentBlock::ToolUse { id, name, input } = &m.blocks[1] else {
            panic!("expected tool_use");
        };
        assert_eq!(id, "call_1");
        assert_eq!(name, "exec");
        assert_eq!(input, &serde_json::json!({"code": "1+1"}));
    }

    #[test]
    fn tool_input_delta_correlates_by_id() {
        let events = [
            StreamingEvent::ToolUseStart {
                id: "tu_0".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "tu_0".to_string(),
                delta: "{\"x\":".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "tu_0".to_string(),
                delta: "1}".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "tu_0".to_string(),
            },
        ];
        let m = assemble_message(&events);
        let ContentBlock::ToolUse { id, input, .. } = &m.blocks[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(id, "tu_0");
        assert_eq!(input, &serde_json::json!({"x": 1}));
    }

    #[test]
    fn unmatched_input_delta_is_dropped_not_misattributed() {
        // A delta whose id matches no in-flight tool must not be appended to
        // an unrelated tool (the old positional fallback did that).
        let events = [
            StreamingEvent::ToolUseStart {
                id: "tu_0".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "other".to_string(),
                delta: "garbage".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "tu_0".to_string(),
            },
        ];
        let m = assemble_message(&events);
        let ContentBlock::ToolUse { id, input, .. } = &m.blocks[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(id, "tu_0");
        assert_eq!(input, &serde_json::json!(null));
    }

    #[test]
    fn preserves_thinking_before_tool_order() {
        let events = [
            StreamingEvent::ThinkingDelta("let me think".to_string()),
            StreamingEvent::ThinkingSignature("sig_abc".to_string()),
            StreamingEvent::ToolUseStart {
                id: "tu_0".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "tu_0".to_string(),
                delta: "1".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "tu_0".to_string(),
            },
        ];
        let m = assemble_message(&events);
        assert_eq!(m.blocks.len(), 2);
        assert!(matches!(
            m.blocks[0],
            ContentBlock::Thinking { ref signature, .. } if signature.as_deref() == Some("sig_abc")
        ));
        assert!(matches!(m.blocks[1], ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn thinking_signature_attaches_after_later_tool() {
        let events = [
            StreamingEvent::ThinkingDelta("plan".to_string()),
            StreamingEvent::ToolUseStart {
                id: "tu_0".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ThinkingSignature("enc_blob".to_string()),
        ];
        let m = assemble_message(&events);
        assert!(matches!(
            &m.blocks[0],
            ContentBlock::Thinking { text, signature }
                if text == "plan" && signature.as_deref() == Some("enc_blob")
        ));
    }

    #[test]
    fn thinking_signature_without_delta_creates_block() {
        let events = [StreamingEvent::ThinkingSignature("enc_blob".to_string())];
        let m = assemble_message(&events);
        assert_eq!(
            m.blocks[0],
            ContentBlock::Thinking {
                text: String::new(),
                signature: Some("enc_blob".to_string()),
            }
        );
    }

    #[test]
    fn empty_tool_input_becomes_null() {
        let events = [StreamingEvent::ToolUseStart {
            id: "t".to_string(),
            name: "exec".to_string(),
        }];
        let m = assemble_message(&events);
        let ContentBlock::ToolUse { input, .. } = &m.blocks[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(input, &Value::Null);
    }

    #[test]
    fn chat_completions_reasoning_field_lands_on_thinking_signature() {
        // What the chat-completions mapper produces for a DeepSeek-style
        // stream: ThinkingSignature carrying the field name, then deltas.
        let events = [
            StreamingEvent::ThinkingSignature("reasoning_content".to_string()),
            StreamingEvent::ThinkingDelta("hmm".to_string()),
            StreamingEvent::ThinkingDelta(" yes".to_string()),
            StreamingEvent::TextDelta("answer".to_string()),
        ];
        let m = assemble_message(&events);
        assert_eq!(m.blocks.len(), 2);
        match &m.blocks[0] {
            ContentBlock::Thinking { text, signature } => {
                assert_eq!(text, "hmm yes");
                assert_eq!(signature.as_deref(), Some("reasoning_content"));
            }
            b => panic!("expected Thinking, got {b:?}"),
        }
        match &m.blocks[1] {
            ContentBlock::Text { text } => assert_eq!(text, "answer"),
            b => panic!("expected Text, got {b:?}"),
        }
    }
}
