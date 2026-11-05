use std::collections::HashMap;

use lofi_types::{ContentBlock, Message, PartSignatureFormat, Role, StreamingEvent};
use serde_json::Value;

#[derive(Default)]
pub struct MessageAssembler {
    order: Vec<Slot>,
    tools: Vec<ToolBuilder>,
    pending_tool_sigs: HashMap<String, PartSig>,
    /// The last part the stream sent was reasoning, and no later part has
    /// closed it yet.
    thinking_open: bool,
    /// The protocol marks the end of each call, so a call without a marker
    /// is unfinished. One that never marks ends proves nothing per call.
    marks_tool_end: bool,
    /// Counts the argument events. A protocol without end markers ends inside
    /// the call that received bytes after the newest start, which parallel
    /// calls can make an older one.
    seq: u64,
    last_start_seq: u64,
}

#[derive(Debug)]
struct ToolBuilder {
    id: String,
    name: String,
    input: String,
    ended: bool,
    sig: Option<PartSig>,
    /// The sequence number of the newest argument bytes this call received.
    input_seq: u64,
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
        redacted: bool,
        /// A signature arrived after this trace's text, so the part is complete
        /// and a later delta belongs to the next one.
        closed: bool,
    },
    Tool(usize),
}

impl MessageAssembler {
    #[must_use]
    pub fn new(marks_tool_end: bool) -> Self {
        Self {
            marks_tool_end,
            ..Self::default()
        }
    }

    #[allow(clippy::too_many_lines)]
    pub fn push(&mut self, event: StreamingEvent) {
        self.note_open_trace(&event);
        match event {
            StreamingEvent::TextDelta(d) => match self.order.last_mut() {
                Some(Slot::Text { text, .. }) => text.push_str(&d),
                _ => self.order.push(Slot::Text { text: d, sig: None }),
            },
            StreamingEvent::ThinkingDelta(d) => match self.order.last_mut() {
                // A completed trace is closed, so a delta after it opens the
                // next part instead of extending a part the provider signed.
                // A signature that arrives before the text marks the field the
                // text came on, so that slot is still open.
                Some(Slot::Thinking {
                    text,
                    closed: false,
                    part_sig: None,
                    ..
                }) => text.push_str(&d),
                _ => self.order.push(Slot::Thinking {
                    text: d,
                    sig: None,
                    part_sig: None,
                    redacted: false,
                    closed: false,
                }),
            },
            StreamingEvent::ThinkingSignature(s) => {
                if let Some(Slot::Thinking {
                    sig, text, closed, ..
                }) = self
                    .order
                    .iter_mut()
                    .rev()
                    .find(|slot| matches!(slot, Slot::Thinking { .. }))
                {
                    // A signature that follows its text completes the part.
                    *closed = !text.is_empty();
                    *sig = Some(s);
                } else {
                    self.order.push(Slot::Thinking {
                        text: String::new(),
                        sig: Some(s),
                        part_sig: None,
                        redacted: false,
                        closed: false,
                    });
                }
            }
            StreamingEvent::ThinkingRedacted { data } => {
                self.order.push(Slot::Thinking {
                    text: "[Reasoning redacted]".to_string(),
                    sig: Some(data),
                    part_sig: None,
                    redacted: true,
                    closed: true,
                });
            }
            StreamingEvent::PartSignature {
                provider,
                model,
                format,
                target,
                signature,
            } => self.attach_signature(
                PartSig {
                    provider,
                    model,
                    format,
                    signature,
                },
                target,
            ),
            StreamingEvent::ToolUseStart { id, name } => {
                let sig = self.pending_tool_sigs.remove(&id);
                self.tools.push(ToolBuilder {
                    id,
                    name,
                    input: String::new(),
                    ended: false,
                    sig,
                    input_seq: 0,
                });
                self.seq += 1;
                self.last_start_seq = self.seq;
                self.order.push(Slot::Tool(self.tools.len() - 1));
            }
            StreamingEvent::ToolUseInputDelta { id, delta } => {
                let pos = self.tools.iter().rposition(|t| !t.ended && t.id == id);
                if let Some(i) = pos {
                    self.seq += 1;
                    self.tools[i].input.push_str(&delta);
                    self.tools[i].input_seq = self.seq;
                }
            }
            StreamingEvent::ToolUseInputComplete { id, input } => {
                let pos = self.tools.iter().rposition(|t| !t.ended && t.id == id);
                if let Some(i) = pos {
                    self.seq += 1;
                    self.tools[i].input = input;
                    self.tools[i].input_seq = self.seq;
                }
            }
            StreamingEvent::ToolUseEnd { id } => {
                let pos = self.tools.iter().rposition(|t| !t.ended && t.id == id);
                if let Some(i) = pos {
                    self.tools[i].ended = true;
                }
            }
            StreamingEvent::Done { .. } | StreamingEvent::Error(_) => {}
        }
    }

    /// Note whether the stream is still inside a reasoning trace: one that no
    /// later part follows. A signature belongs to the trace it signs, so it
    /// opens a trace whose text has not started yet and closes the one whose
    /// text it follows.
    fn note_open_trace(&mut self, event: &StreamingEvent) {
        self.thinking_open = match event {
            StreamingEvent::ThinkingDelta(_) => true,
            StreamingEvent::ThinkingSignature(_) => !matches!(
                self.order.last(),
                Some(Slot::Thinking { text, .. }) if !text.is_empty()
            ),
            _ => false,
        };
    }

    fn attach_signature(&mut self, sig: PartSig, target: Option<String>) {
        let Some(target) = target else {
            match self.order.last_mut() {
                Some(Slot::Text { sig: slot_sig, .. }) => *slot_sig = Some(sig),
                Some(Slot::Thinking { part_sig, .. }) => *part_sig = Some(sig),
                Some(Slot::Tool(i)) => self.tools[*i].sig = Some(sig),
                None => {}
            }
            return;
        };
        if let Some(tool) = self.tools.iter_mut().rfind(|tool| tool.id == target) {
            tool.sig = Some(sig);
        } else {
            self.pending_tool_sigs.insert(target, sig);
        }
    }

    /// Whether the stream ended inside a reasoning trace: its text started and
    /// no later part arrived to close it. A signature in front of the text does
    /// not close it, so a chat-completions trace counts as open from its first
    /// delta.
    #[must_use]
    pub fn open_thinking(&self) -> bool {
        self.thinking_open
    }

    /// The calls the stream ended inside. Running one would send arguments
    /// the model never wrote.
    ///
    /// A protocol that marks the end of each call proves which calls finished,
    /// so every call without a marker is cut. One that does not mark ends
    /// proves nothing per call, so the cut is the call the stream was inside
    /// when it ended: the one that received argument bytes after the newest
    /// start, which parallel calls can make an older call. A call the stream
    /// never gave an argument is inside it too, and an older call whose
    /// arguments finished before the newest start is the model's whole input.
    #[must_use]
    pub fn open_tool_ids(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|tool| {
                !tool.ended
                    && (self.marks_tool_end
                        || tool.input_seq > self.last_start_seq
                        || tool.input.is_empty())
            })
            .map(|tool| tool.id.clone())
            .collect()
    }

    /// Tool calls whose accumulated arguments do not parse as JSON: the
    /// provider cut the call off mid-stream. Parse failure is the only
    /// cut signal that works across every protocol — Completions streams
    /// carry no per-call end marker, so `ToolUseEnd` absence proves nothing.
    #[must_use]
    pub fn unparseable_tool_ids(&self) -> Vec<String> {
        self.tools
            .iter()
            .filter(|t| !t.input.is_empty() && serde_json::from_str::<Value>(&t.input).is_err())
            .map(|t| t.id.clone())
            .collect()
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
                    redacted,
                    ..
                } => {
                    blocks.push(ContentBlock::Thinking {
                        text,
                        signature: sig,
                        redacted,
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
                    // Tool inputs are JSON objects by protocol; Anthropic
                    // rejects `null`. An empty buffer is a no-argument call
                    // and an unparseable buffer is a call cut mid-stream -
                    // both replay as `{}` so the retained assistant message
                    // stays sendable.
                    let input = serde_json::from_str(&t.input)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| serde_json::json!({}));
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
            origin: None,
            role: Role::Assistant,
            blocks,
            kind: lofi_types::PromptKind::User,
        }
    }
}

#[must_use]
pub fn assemble_message(events: &[StreamingEvent]) -> Message {
    let mut assembler = MessageAssembler::new(true);
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
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: None,
            },
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
    fn unparseable_tool_ids_reports_cut_calls_only() {
        let mut a = MessageAssembler::new(true);
        a.push(StreamingEvent::ToolUseStart {
            id: "partial".to_string(),
            name: "exec".to_string(),
        });
        a.push(StreamingEvent::ToolUseInputDelta {
            id: "partial".to_string(),
            delta: r#"{"code":"#.to_string(),
        });
        a.push(StreamingEvent::ToolUseStart {
            id: "whole".to_string(),
            name: "exec".to_string(),
        });
        a.push(StreamingEvent::ToolUseInputDelta {
            id: "whole".to_string(),
            delta: r#"{"code":"1+1"}"#.to_string(),
        });
        a.push(StreamingEvent::ToolUseEnd {
            id: "whole".to_string(),
        });
        // A tool with empty arguments is not a cut call.
        a.push(StreamingEvent::ToolUseStart {
            id: "empty".to_string(),
            name: "exec".to_string(),
        });
        a.push(StreamingEvent::ToolUseEnd {
            id: "empty".to_string(),
        });
        assert_eq!(a.unparseable_tool_ids(), vec!["partial".to_string()]);
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
        assert_eq!(input, &serde_json::json!({}));
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
            ContentBlock::Thinking {
                text, signature, ..
            } if text == "plan" && signature.as_deref() == Some("enc_blob")
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
                redacted: false,
            }
        );
    }

    #[test]
    fn empty_tool_input_becomes_an_empty_object() {
        let events = [StreamingEvent::ToolUseStart {
            id: "t".to_string(),
            name: "exec".to_string(),
        }];
        let m = assemble_message(&events);
        let ContentBlock::ToolUse { input, .. } = &m.blocks[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(input, &serde_json::json!({}));
    }

    #[test]
    fn cut_tool_input_becomes_an_empty_object() {
        let events = [
            StreamingEvent::ToolUseStart {
                id: "t".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "t".to_string(),
                delta: r#"{"code":"#.to_string(),
            },
        ];
        let m = assemble_message(&events);
        let ContentBlock::ToolUse { input, .. } = &m.blocks[0] else {
            panic!("expected tool_use");
        };
        assert_eq!(input, &serde_json::json!({}));
    }

    #[test]
    fn a_signature_after_its_text_closes_the_trace() {
        let mut assembler = MessageAssembler::new(true);
        assembler.push(StreamingEvent::ThinkingDelta("plan".to_string()));
        assembler.push(StreamingEvent::ThinkingSignature("enc_blob".to_string()));
        assert!(!assembler.open_thinking(), "the signed trace is complete");
    }

    #[test]
    fn a_signature_before_any_text_opens_the_trace() {
        let mut assembler = MessageAssembler::new(true);
        assembler.push(StreamingEvent::ThinkingSignature(
            "reasoning_content".into(),
        ));
        assembler.push(StreamingEvent::ThinkingDelta("plan".to_string()));
        assert!(assembler.open_thinking());
    }

    #[test]
    fn a_reasoning_delta_after_a_signed_trace_opens_a_new_part() {
        let events = [
            StreamingEvent::ThinkingDelta("first".to_string()),
            StreamingEvent::PartSignature {
                provider: "google".into(),
                model: "m".into(),
                format: PartSignatureFormat::Google,
                target: None,
                signature: "c2ln".into(),
            },
            StreamingEvent::ThinkingDelta("second".to_string()),
        ];
        let m = assemble_message(&events);
        assert_eq!(m.blocks.len(), 3, "{:?}", m.blocks);
        assert!(matches!(&m.blocks[0], ContentBlock::Thinking { text, .. } if text == "first"));
        assert!(matches!(&m.blocks[1], ContentBlock::PartSignature { .. }));
        assert!(matches!(&m.blocks[2], ContentBlock::Thinking { text, .. } if text == "second"));
    }

    #[test]
    fn open_tool_ids_reports_the_last_slot_and_empty_starts() {
        let mut assembler = MessageAssembler::new(false);
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t0".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: r#"{"code":"#.to_string(),
        });
        assert_eq!(assembler.open_tool_ids(), ["t0", "t1"]);
    }

    #[test]
    fn open_tool_ids_reports_an_unfinished_call_after_an_end_marker() {
        let mut assembler = MessageAssembler::new(true);
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t0".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseInputDelta {
            id: "t0".to_string(),
            delta: r#"{"code":"1"}"#.to_string(),
        });
        // A whole-call protocol never replaced this call's arguments, so the
        // stream ended inside it even though they parse.
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        });
        assert_eq!(assembler.open_tool_ids(), ["t0"]);
    }

    #[test]
    fn open_tool_ids_trusts_an_earlier_parseable_call_without_end_markers() {
        let mut assembler = MessageAssembler::new(false);
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t0".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseInputDelta {
            id: "t0".to_string(),
            delta: r#"{"code":"1"}"#.to_string(),
        });
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        assert_eq!(assembler.open_tool_ids(), ["t1"]);
    }

    #[test]
    fn open_tool_ids_cuts_the_call_that_received_bytes_after_a_later_start() {
        let mut assembler = MessageAssembler::new(false);
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t0".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        // Parallel calls: the stream went back to the first one, so the cut
        // landed inside it even though its arguments parse.
        assembler.push(StreamingEvent::ToolUseInputDelta {
            id: "t0".to_string(),
            delta: r#"{"code":"1"}"#.to_string(),
        });
        assert_eq!(assembler.open_tool_ids(), ["t0", "t1"]);
    }

    #[test]
    fn open_tool_ids_ignores_a_finished_call() {
        let mut assembler = MessageAssembler::new(true);
        assembler.push(StreamingEvent::ToolUseStart {
            id: "t0".to_string(),
            name: "exec".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseInputComplete {
            id: "t0".to_string(),
            input: "{}".to_string(),
        });
        assembler.push(StreamingEvent::ToolUseEnd {
            id: "t0".to_string(),
        });
        assembler.push(StreamingEvent::TextDelta("tail".to_string()));
        assert!(assembler.open_tool_ids().is_empty());
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
            ContentBlock::Thinking {
                text, signature, ..
            } => {
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
