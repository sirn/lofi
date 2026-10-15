#![allow(clippy::wildcard_imports)]

use super::*;

pub(super) fn stamp_message_origin(message: &mut Message, model: &Model) {
    message.origin = Some(lofi_types::ModelOrigin::from(model));
}

pub(in crate::agent) fn assistant_is_thinking_only(message: &Message) -> bool {
    let mut has_thinking = false;
    let mut last_was_thinking = false;
    for block in &message.blocks {
        match block {
            ContentBlock::Thinking {
                text,
                signature,
                redacted,
                ..
            } => {
                has_thinking |= *redacted || signature.is_some() || !text.trim().is_empty();
                last_was_thinking = true;
            }
            ContentBlock::Text { text } if text.trim().is_empty() => last_was_thinking = false,
            ContentBlock::PartSignature { .. } if last_was_thinking => has_thinking = true,
            ContentBlock::PartSignature { .. } => {}
            _ => return false,
        }
    }
    has_thinking
}

pub(in crate::agent) fn assistant_announces_tool_intent(message: &Message) -> bool {
    let Some(text) = message.blocks.iter().rev().find_map(|block| match block {
        ContentBlock::Text { text } => Some(text.as_str()),
        _ => None,
    }) else {
        return false;
    };
    let paragraph = text.trim().rsplit("\n\n").next().unwrap_or("").trim();
    if paragraph.is_empty() || paragraph.chars().count() > 240 {
        return false;
    }
    let normalized = paragraph.replace(['’', '‘'], "'").to_ascii_lowercase();
    let sentence = normalized
        .rsplit(['.', '!', '?'])
        .find(|part| !part.trim().is_empty())
        .unwrap_or("")
        .trim();
    let action = [
        "let me ",
        "i'll ",
        "i will ",
        "i am going to ",
        "i'm going to ",
        "next, i'll ",
        "next i'll ",
    ]
    .iter()
    .find_map(|prefix| sentence.strip_prefix(prefix));
    let Some(action) = action else {
        return false;
    };
    if action.starts_with("not ") {
        return false;
    }
    let verb = action
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    matches!(
        verb,
        "inspect"
            | "check"
            | "read"
            | "load"
            | "search"
            | "run"
            | "test"
            | "verify"
            | "investigate"
            | "implement"
            | "fix"
            | "update"
            | "edit"
            | "try"
    )
}

/// Enforces the pairing invariant: every trailing assistant `ToolUse` block
/// must be answered by a `ToolResult` before the message list is handed back
/// to the caller. Providers like the Anthropic Messages API reject an orphaned
/// `tool_use` (400 `"did not find any tool_result blocks"`), and the transcript
/// keeps cancelled partials for replay — so pairing is structural, not a
/// per-outcome fallback. Idempotent: appends an error `ToolResult` only for
/// `ToolUse` blocks in the last assistant message that have no matching result
/// after it; a fully paired tail is left untouched.
///
/// Called on every abandoned completion of `run_once_inner` (provider error,
/// cancel, closed channel). The fully-failed turn is truncated by the caller
/// afterwards, so the synthesized results are dropped there; on a cancelled
/// turn they are what keep the retained partial provider-valid.
pub(super) fn close_orphaned_tool_uses(messages: &mut Vec<Message>) {
    let Some(assistant_index) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
        return;
    };
    // Only the tail matters: scan the last assistant message's tool uses and
    // the results that came after it.
    let pending: Vec<String> = messages[assistant_index]
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    if pending.is_empty() {
        return;
    }
    let answered: std::collections::HashSet<&str> = messages[assistant_index + 1..]
        .iter()
        .flat_map(|m| m.blocks.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    let synthesized: Vec<ContentBlock> = pending
        .iter()
        .filter(|id| !answered.contains(id.as_str()))
        .map(|id| ContentBlock::ToolResult {
            tool_use_id: id.clone(),
            content: "cancelled: tool run interrupted before producing a result".to_string(),
            is_error: true,
            images: Vec::new(),
        })
        .collect();
    if synthesized.is_empty() {
        return;
    }
    messages.push(Message {
        origin: None,
        role: Role::Tool,
        blocks: synthesized,
        kind: lofi_types::PromptKind::User,
    });
}

/// Count attached images across the request history, for the omit notice.
pub(super) fn count_image_blocks(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| match b {
            ContentBlock::Image { .. } => 1,
            ContentBlock::ToolResult { images, .. } => images.len(),
            _ => 0,
        })
        .sum()
}

/// Return a copy of `messages` with every image dropped for a model without
/// image support: standalone `Image` blocks become text markers, and the
/// images carried on a `ToolResult` are dropped with a marker appended to its
/// text. The model still sees that an attachment was present. Used only on the
/// send path; the durable transcript is untouched.
pub(super) fn strip_image_blocks(messages: &[Message]) -> Vec<Message> {
    use std::fmt::Write as _;
    messages
        .iter()
        .map(|m| Message {
            origin: m.origin.clone(),
            role: m.role,
            kind: m.kind,
            blocks: m
                .blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Image { media_type, .. } => ContentBlock::Text {
                        text: format!("[image omitted: model does not support images; media_type={media_type}]"),
                    },
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        images,
                    } if !images.is_empty() => {
                        let mut content = content.clone();
                        for img in images {
                            if !content.is_empty() {
                                content.push('\n');
                            }
                            let _ = write!(content, "[image omitted: model does not support images; media_type={}]", img.media_type);
                        }
                        ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content,
                            is_error: *is_error,
                            images: Vec::new(),
                        }
                    }
                    other => other.clone(),
                })
                .collect(),
        })
        .collect()
}

/// Request image payload budget (base64 bytes). Providers cap the request
/// *body*, not just the token count: Anthropic rejects a body over 32 MiB
/// with HTTP 413 `request_too_large` even when the token estimate is well
/// under the context window. Images are the dominant unbounded term (text,
/// tool results, and schemas are bounded elsewhere), so the byte pressure
/// signal thresholds just the image payload. 8 MiB sits far under every
/// provider body cap after base64 inflation (x4/3) plus envelope overhead.
pub(super) const MAX_REQUEST_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Sentinel error message marking a stop-before-send due to an oversized
/// image payload. The run loop intercepts it and routes to the existing
/// `ContextPressure` recovery (force-compact + continue) instead of failing
/// the turn. A sentinel string, not a new `Error` variant, because only this
/// send/loop pair needs to distinguish it and a variant would leak an
/// internal control-flow detail into the shared error type.
pub(super) const IMAGE_PRESSURE_SENTINEL: &str = "__lofi_image_pressure__";

/// Sum the base64 payload bytes of every attached image across the request
/// history. Base64 length is `4 * ceil(n/3)` for `n` raw bytes.
pub(super) fn image_payload_bytes(messages: &[Message]) -> u64 {
    messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .map(|b| match b {
            ContentBlock::Image { bytes, .. } => 4 * bytes.len().div_ceil(3) as u64,
            ContentBlock::ToolResult { images, .. } => images
                .iter()
                .map(|img| 4 * img.bytes.len().div_ceil(3) as u64)
                .sum(),
            _ => 0,
        })
        .sum()
}
