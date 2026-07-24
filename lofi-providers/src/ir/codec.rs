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
    /// The value of the `event:` line, if any.
    pub event: Option<String>,
    /// The `data:` payload, with multiple `data:` lines joined by `\n`.
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
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            // A single optional leading space after the colon is part of the
            // delimiter, not the payload.
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            data_lines.push(rest.to_string());
            have_data = true;
        }
        // `id:`, `retry:`, and `:` comment lines are intentionally ignored.
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

/// Fold accumulated [`StreamingEvent`]s into a final assistant [`Message`].
///
/// Text deltas concatenate into a single `Text` block. Tool-use blocks are
/// assembled from `ToolUseStart` + `ToolUseInputDelta`s + `ToolUseEnd`; the
/// input JSON string is parsed at finalization (empty/invalid input becomes
/// `Value::Null`). Providers identify in-flight tool calls by different
/// fields (`OpenAI` sends an opaque id on the first delta, Anthropic sends a
/// block index on later deltas), so the mappers normalize every delta to the
/// real tool id and correlation here is by id only. An input delta whose id
/// matches no in-flight tool is dropped rather than misattributed to an
/// unrelated tool. `Done`/`Error` are ignored here.
#[must_use]
pub fn assemble_message(events: &[StreamingEvent]) -> Message {
    #[derive(Debug)]
    struct ToolBuilder {
        id: String,
        name: String,
        input: String,
        ended: bool,
    }

    /// An in-order content-block builder. Streaming providers emit text,
    /// thinking, and tool-use blocks interleaved (Anthropic sends a thinking
    /// block *before* the `tool_use` it justifies); the assembled message must
    /// replay them in that order, not grouped by type.
    enum Slot {
        Text(String),
        Thinking { text: String, sig: Option<String> },
        Tool(usize),
    }

    let mut order: Vec<Slot> = Vec::new();
    let mut tools: Vec<ToolBuilder> = Vec::new();

    for ev in events {
        match ev {
            StreamingEvent::TextDelta(d) => match order.last_mut() {
                Some(Slot::Text(s)) => s.push_str(d),
                _ => order.push(Slot::Text(d.clone())),
            },
            StreamingEvent::ThinkingDelta(d) => match order.last_mut() {
                Some(Slot::Thinking { text, .. }) => text.push_str(d),
                _ => order.push(Slot::Thinking {
                    text: d.clone(),
                    sig: None,
                }),
            },
            StreamingEvent::ThinkingSignature(s) => {
                // Attach to the most recent thinking slot; Anthropic sends
                // the signature after the thinking text of the same block.
                if let Some(Slot::Thinking { sig, .. }) = order.last_mut() {
                    *sig = Some(s.clone());
                }
            }
            StreamingEvent::ToolUseStart { id, name } => {
                tools.push(ToolBuilder {
                    id: id.clone(),
                    name: name.clone(),
                    input: String::new(),
                    ended: false,
                });
                order.push(Slot::Tool(tools.len() - 1));
            }
            StreamingEvent::ToolUseInputDelta { id, delta } => {
                let pos = tools.iter().rposition(|t| !t.ended && t.id == *id);
                if let Some(i) = pos {
                    tools[i].input.push_str(delta);
                }
            }
            StreamingEvent::ToolUseEnd { id } => {
                let pos = tools.iter().rposition(|t| !t.ended && t.id == *id);
                if let Some(i) = pos {
                    tools[i].ended = true;
                }
            }
            _ => {}
        }
    }

    let mut blocks = Vec::new();
    for slot in order {
        match slot {
            Slot::Text(text) => blocks.push(ContentBlock::Text { text }),
            Slot::Thinking { text, sig } => blocks.push(ContentBlock::Thinking {
                text,
                signature: sig,
            }),
            Slot::Tool(i) => {
                let t = &tools[i];
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
        // Mappers emit the real tool id on every delta; correlation is by id,
        // not by positional fallback.
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
