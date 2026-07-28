//! Tiered-retention context editing for the compaction kept tail.
//!
//! A no-LLM, in-place pass that shrinks the kept tail before it becomes the
//! new prefix, so the next compaction fires far later. Applied at compaction
//! boundaries only (see [`crate::compact`]) — never mid-run — which keeps it
//! prefix-cache-safe: between compactions the tail is append-only and caches
//! normally; the edit rides the cache break that compaction already pays.
//!
//! Three levers, each with a protected recent window counted from the end of
//! the tail (so the model's working set stays intact):
//!
//! - **tool results** (the bulk): older than the last `keep_results` are
//!   replaced with a stub naming an event id the model re-expands via
//!   `lofi.result`. Non-destructive — unlike Anthropic's `clear_tool_uses` /
//!   opencode's prune / Claude Code's microcompact, which discard outright.
//! - **thinking**: older than the last `keep_thinking` are dropped. Thinking
//!   is per-turn scratchpad; the conclusion lives in the assistant text,
//!   which is always kept. (Older Anthropic models strip old thinking from
//!   the cache anyway; `clear_thinking_20251015` makes it explicit.)
//! - **tool-call code**: older than the last `keep_calls` keep their
//!   `display` label (intent) but the verbatim `code` is stubbed, also
//!   `lofi.result`-recoverable.
//!
//! Assistant prose (`Text`) is always kept verbatim — it is ~4% of bytes and
//!   the actual signal.

use lofi_types::{ContentBlock, EditConfig, Message, SessionEvent, SessionEventKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cat {
    ToolResult,
    Thinking,
    ToolUse,
    Other,
}

fn category(b: &ContentBlock) -> Cat {
    match b {
        ContentBlock::ToolResult { .. } => Cat::ToolResult,
        ContentBlock::Thinking { .. } => Cat::Thinking,
        ContentBlock::ToolUse { .. } => Cat::ToolUse,
        ContentBlock::Text { .. } => Cat::Other,
    }
}

fn result_stub(event_id: &str, is_error: bool) -> String {
    let kind = if is_error { "error result" } else { "result" };
    format!("[exec {kind} cleared — re-expand with lofi.result(\"{event_id}\")]")
}

fn trim_tool_use_input(input: &serde_json::Value, event_id: &str) -> serde_json::Value {
    use serde_json::json;
    let stub = format!("[code cleared — re-expand with lofi.result(\"{event_id}\")]");
    match input {
        serde_json::Value::Object(obj) => {
            let mut out = serde_json::Map::new();
            if let Some(display) = obj.get("display") {
                out.insert("display".to_string(), display.clone());
            }
            out.insert("code".to_string(), json!(stub));
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Edit the kept tail into a lighter, recall-recoverable form.
///
/// `kept` is the list of `(event_id, message)` pairs that would otherwise be
/// carried verbatim as the post-compaction prefix. Returns a new message
/// list with old tool results / thinking / tool-call code elided per `opts`.
///
/// When `opts.enabled` is false this is a plain clone (the verbatim tail).
#[must_use]
pub fn edit_tail(kept: &[(String, Message)], opts: &EditConfig) -> Vec<Message> {
    let refs: Vec<(&str, &Message)> = kept
        .iter()
        .map(|(id, message)| (id.as_str(), message))
        .collect();
    edit_tail_refs(&refs, opts)
}

#[must_use]
pub fn edit_tail_refs(kept: &[(&str, &Message)], opts: &EditConfig) -> Vec<Message> {
    if !opts.enabled || kept.is_empty() {
        return kept.iter().map(|(_, message)| (*message).clone()).collect();
    }

    let mut kept_flag: Vec<Vec<bool>> = kept
        .iter()
        .map(|(_, message)| vec![true; message.blocks.len()])
        .collect();
    let mut cnt_r = 0usize;
    let mut cnt_t = 0usize;
    let mut cnt_u = 0usize;
    for (mi, (_, msg)) in kept.iter().enumerate().rev() {
        for (bi, block) in msg.blocks.iter().enumerate().rev() {
            let keep = match category(block) {
                Cat::ToolResult => {
                    let keep = cnt_r < opts.keep_results;
                    cnt_r += 1;
                    keep
                }
                Cat::Thinking => {
                    let keep = cnt_t < opts.keep_thinking;
                    cnt_t += 1;
                    keep
                }
                Cat::ToolUse => {
                    let keep = cnt_u < opts.keep_calls;
                    cnt_u += 1;
                    keep
                }
                Cat::Other => true,
            };
            kept_flag[mi][bi] = keep;
        }
    }

    kept.iter()
        .enumerate()
        .map(|(mi, (event_id, msg))| {
            let mut blocks = Vec::with_capacity(msg.blocks.len());
            for (bi, block) in msg.blocks.iter().enumerate() {
                if kept_flag[mi][bi] {
                    blocks.push(block.clone());
                    continue;
                }
                match block {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => blocks.push(ContentBlock::ToolResult {
                        tool_use_id: tool_use_id.clone(),
                        content: result_stub(event_id, *is_error),
                        is_error: *is_error,
                    }),
                    ContentBlock::Thinking { .. } => {}
                    ContentBlock::ToolUse { id, name, input } => {
                        blocks.push(ContentBlock::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            input: trim_tool_use_input(input, event_id),
                        });
                    }
                    text @ ContentBlock::Text { .. } => blocks.push(text.clone()),
                }
            }
            Message {
                role: msg.role,
                blocks,
            }
        })
        .collect()
}

#[must_use]
pub fn recover_message_content(msg: &Message) -> Option<String> {
    let results: Vec<&str> = msg
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .collect();
    if !results.is_empty() {
        return Some(results.join("\n\n"));
    }
    msg.blocks.iter().find_map(|block| match block {
        ContentBlock::ToolUse { input, .. } => {
            Some(serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string()))
        }
        _ => None,
    })
}

#[must_use]
pub fn recover_event_content(events: &[SessionEvent], id: &str) -> Option<String> {
    let event = events.iter().find(|event| event.id == id)?;
    let SessionEventKind::Message(message) = &event.kind else {
        return None;
    };
    recover_message_content(message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use lofi_types::{ContentBlock, Message, Role};

    #[allow(dead_code)]
    fn user(t: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: t.to_string(),
            }],
        }
    }
    fn assistant(t: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: t.to_string(),
            }],
        }
    }
    fn think(t: &str) -> ContentBlock {
        ContentBlock::Thinking {
            text: t.to_string(),
            signature: None,
        }
    }
    fn exec_call(id: &str, code: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({ "code": code, "display": { "name": "do thing" } }),
        }
    }
    fn exec_result(id: &str, content: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: content.to_string(),
            is_error: false,
        }
    }

    fn opts(keep_results: usize, keep_thinking: usize, keep_calls: usize) -> EditConfig {
        EditConfig {
            enabled: true,
            keep_results,
            keep_thinking,
            keep_calls,
        }
    }

    fn tail(kept: &[(String, Message)]) -> Vec<Message> {
        edit_tail(kept, &opts(1, 1, 1))
    }

    #[test]
    fn disabled_returns_verbatim() {
        let kept = vec![("e1".to_string(), assistant("hi"))];
        let o = EditConfig {
            enabled: false,
            keep_results: 1,
            keep_thinking: 1,
            keep_calls: 1,
        };
        let out = edit_tail(&kept, &o);
        assert_eq!(out, vec![assistant("hi")]);
    }

    #[test]
    fn keeps_prose_verbatim() {
        let kept = vec![
            ("e1".to_string(), assistant("old decision")),
            ("e2".to_string(), assistant("recent decision")),
        ];
        let out = tail(&kept);
        assert_eq!(
            out[0].blocks[0],
            ContentBlock::Text {
                text: "old decision".to_string()
            }
        );
        assert_eq!(
            out[1].blocks[0],
            ContentBlock::Text {
                text: "recent decision".to_string()
            }
        );
    }

    #[test]
    fn elides_old_tool_results_keeps_recent() {
        let kept = vec![
            (
                "e1".to_string(),
                Message {
                    role: Role::Tool,
                    blocks: vec![exec_result("a", "out-1")],
                },
            ),
            (
                "e2".to_string(),
                Message {
                    role: Role::Tool,
                    blocks: vec![exec_result("b", "out-2")],
                },
            ),
            (
                "e3".to_string(),
                Message {
                    role: Role::Tool,
                    blocks: vec![exec_result("c", "out-3")],
                },
            ),
        ];
        let out = tail(&kept);
        assert_eq!(out[2].blocks[0], exec_result("c", "out-3"));
        let ContentBlock::ToolResult { content, .. } = &out[0].blocks[0] else {
            panic!()
        };
        assert!(content.contains("lofi.result(\"e1\")"));
        let ContentBlock::ToolResult { content, .. } = &out[1].blocks[0] else {
            panic!()
        };
        assert!(content.contains("lofi.result(\"e2\")"));
    }

    #[test]
    fn drops_old_thinking_keeps_recent() {
        let kept = vec![
            (
                "e1".to_string(),
                Message {
                    role: Role::Assistant,
                    blocks: vec![think("old reasoning"), exec_call("a", "code-1")],
                },
            ),
            (
                "e2".to_string(),
                Message {
                    role: Role::Assistant,
                    blocks: vec![think("recent reasoning"), exec_call("b", "code-2")],
                },
            ),
        ];
        let out = tail(&kept);
        assert!(out[0]
            .blocks
            .iter()
            .all(|b| !matches!(b, ContentBlock::Thinking { .. })));
        assert!(matches!(out[1].blocks[0], ContentBlock::Thinking { .. }));
    }

    #[test]
    fn trims_old_tool_call_code_keeps_display() {
        let kept = vec![
            (
                "e1".to_string(),
                Message {
                    role: Role::Assistant,
                    blocks: vec![exec_call("a", "old-secret-code")],
                },
            ),
            (
                "e2".to_string(),
                Message {
                    role: Role::Assistant,
                    blocks: vec![exec_call("b", "recent-code")],
                },
            ),
        ];
        let out = tail(&kept);
        let ContentBlock::ToolUse { input: recent, .. } = &out[1].blocks[0] else {
            panic!()
        };
        assert_eq!(
            recent.get("code").and_then(|v| v.as_str()),
            Some("recent-code")
        );
        let ContentBlock::ToolUse { input: old, .. } = &out[0].blocks[0] else {
            panic!()
        };
        assert_eq!(
            old.get("display")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str()),
            Some("do thing")
        );
        let code = old.get("code").and_then(|v| v.as_str()).unwrap();
        assert!(code.contains("lofi.result(\"e1\")"));
    }
}
