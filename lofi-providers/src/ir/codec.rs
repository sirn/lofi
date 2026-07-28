//! Shared SSE line reader and `StreamingEvent` accumulator.
//!
//! The SSE wire format groups events into blocks terminated by a blank line.
//! Within a block, `event:` sets the type and `data:` lines carry the payload
//! (multiple `data:` lines are joined with `\n`). This module parses that
//! format into [`SseEvent`]s without touching HTTP — the providers layer feeds
//! raw body text or line streams here.
//!
//! [`assemble_message`] folds a sequence of [`StreamingEvent`]s back into a
//! single assistant [`Message`], which is how a streamed turn is collapsed into
//! the conversation log.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use lofi_types::{ContentBlock, Message, Role, StreamingEvent};
use serde_json::Value;

/// A single parsed SSE event block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

/// Parse an SSE body into [`SseEvent`]s.
///
/// Splits on blank lines, accumulating `event:`/`data:` lines within each
/// block. Comment lines and unrecognized fields are ignored. A trailing block
/// without a final blank line is still emitted.
#[must_use]
pub fn parse_sse(body: &str) -> Vec<SseEvent> {
    parse_sse_lines(body.lines())
}

/// Parse an SSE stream from a line iterator. See [`parse_sse`].
///
/// A block with no `data:` line is skipped: the mappers decode the `data:`
/// payload as JSON, so an event-only block (e.g. a keep-alive) would otherwise
/// abort the stream with a parse error.
#[must_use]
pub fn parse_sse_lines<'a>(lines: impl Iterator<Item = &'a str>) -> Vec<SseEvent> {
    let mut out = Vec::new();
    let mut event: Option<String> = None;
    let mut data_lines: Vec<String> = Vec::new();
    let mut have_data = false;
    for line in lines {
        if line.is_empty() {
            if have_data {
                out.push(SseEvent {
                    event: event.take(),
                    data: data_lines.join("\n"),
                });
                data_lines.clear();
                have_data = false;
            } else {
                event = None;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(rest.to_string());
            have_data = true;
        }
    }
    if have_data {
        out.push(SseEvent {
            event: event.take(),
            data: data_lines.join("\n"),
        });
    }
    out
}

/// Recognize the `OpenAI` stream-termination sentinel `data: [DONE]`.
#[must_use]
pub fn is_done_marker(data: &str) -> bool {
    data.trim() == "[DONE]"
}

/// Incrementally folds streaming events into an assistant message.
///
/// The builder retains only assembled content and in-flight tool JSON, rather
/// than every delta that produced it.
#[derive(Default)]
pub struct MessageAssembler {
    order: Vec<Slot>,
    tools: Vec<ToolBuilder>,
}

#[derive(Debug)]
struct ToolBuilder {
    id: String,
    name: String,
    input: String,
    ended: bool,
}

enum Slot {
    Text(String),
    Thinking { text: String, sig: Option<String> },
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
                Some(Slot::Text(s)) => s.push_str(&d),
                _ => self.order.push(Slot::Text(d)),
            },
            StreamingEvent::ThinkingDelta(d) => match self.order.last_mut() {
                Some(Slot::Thinking { text, .. }) => text.push_str(&d),
                _ => self.order.push(Slot::Thinking { text: d, sig: None }),
            },
            StreamingEvent::ThinkingSignature(s) => {
                if let Some(Slot::Thinking { sig, .. }) = self.order.last_mut() {
                    *sig = Some(s);
                }
            }
            StreamingEvent::ToolUseStart { id, name } => {
                self.tools.push(ToolBuilder {
                    id,
                    name,
                    input: String::new(),
                    ended: false,
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
                Slot::Text(text) => blocks.push(ContentBlock::Text { text }),
                Slot::Thinking { text, sig } => blocks.push(ContentBlock::Thinking {
                    text,
                    signature: sig,
                }),
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
                }
            }
        }
        Message {
            role: Role::Assistant,
            blocks,
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
    use super::*;
    use lofi_types::Usage;

    #[test]
    fn parses_event_and_data_lines() {
        let body = "event: delta\ndata: {\"a\":1}\n\nevent: done\ndata: [DONE]\n\n";
        let ev = parse_sse(body);
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].event.as_deref(), Some("delta"));
        assert_eq!(ev[0].data, "{\"a\":1}");
        assert!(is_done_marker(&ev[1].data));
    }

    #[test]
    fn joins_multiline_data() {
        let body = "data: line1\ndata: line2\n\n";
        let ev = parse_sse(body);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event, None);
        assert_eq!(ev[0].data, "line1\nline2");
    }

    #[test]
    fn emits_trailing_block_without_blank_line() {
        let body = "data: tail";
        let ev = parse_sse(body);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "tail");
    }

    #[test]
    fn ignores_comments_and_unknown_fields() {
        let body = ": ping\nretry: 5000\ndata: keep\n\n";
        let ev = parse_sse(body);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].data, "keep");
    }

    #[test]
    fn event_only_block_does_not_leak_event_name() {
        // An event-only block (e.g. a keep-alive) must not attach its event
        // name to the next data-bearing block.
        let body = "event: ping\n\ndata: {\"a\":1}\n\n";
        let ev = parse_sse(body);
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].event, None);
        assert_eq!(ev[0].data, "{\"a\":1}");
    }

    #[test]
    fn done_marker_is_trim_aware() {
        assert!(is_done_marker("[DONE]"));
        assert!(is_done_marker("  [DONE]  "));
        assert!(!is_done_marker("not done"));
    }

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
        // Anthropic streams a thinking block before the tool_use it grounds;
        // the assembled message must keep that order and attach the
        // signature to the thinking block, not group thinking last.
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
}
