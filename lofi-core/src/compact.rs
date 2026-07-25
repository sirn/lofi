#![allow(clippy::doc_markdown)]
//! Offline, no-LLM conversation compaction.
//!
//! A port of the pi-vcc algorithm: a purely algorithmic (no model call)
//! pipeline that turns a slice of the conversation into a structured
//! summary, so a long session can be folded back under the context window
//! without losing the thread. The summary is a fixed set of sections —
//! Session Goal, User Preferences, Files And Changes, Commits, Outstanding
//! Context — followed by a compressed per-turn brief transcript. Repeated
//! compactions merge into the prior summary so stable facts accumulate and
//! only the volatile tail is recomputed each time.
//!
//! The compaction is near-lossless in the sense that every turn is
//! represented in the brief transcript (clipped, not dropped), and the
//! structured sections capture the durable facts (goals, preferences, file
//! activity, commits, open problems). The full transcript stays on disk —
//! this only shrinks the slice the model is asked to re-read.
//!
//! lofi's shape differs from pi-vcc in one way: there is a single LLM-facing
//! tool (exec), and the real file/shell actions are native tool calls
//! (lofi.read / lofi.edit / lofi.bash / ...) recorded inside each exec.
//! The extractor therefore reads file activity and commits from the
//! NativeToolRecords on the active path rather than from per-tool messages.

use std::collections::HashMap;
use std::sync::Arc;

use lofi_types::{
    CompactBlock, CompactionHook, ContentBlock, Message, NativeToolRecord, Role, SessionEvent,
    SessionEventKind,
};

use crate::session::store;

/// The marker that begins every compaction summary. Used to detect a
/// previously-injected summary message when re-compacting, so it is stripped
/// from the live message list before planning the next cut (its content is
/// passed as previous_summary and merged into the fresh one).
pub const HANDOFF_PREAMBLE: &str = "This summary captures work done before the most recent messages in this session. Read it to pick up context — this is work already in progress. Continue directly where you left off.";

/// A live message on the active path with the event id of the
/// SessionEventKind::Message it came from. The event id is needed to
/// record the cut boundary (first_kept_entry_id) in the compaction marker.
#[derive(Debug, Clone)]
struct LiveMessage {
    event_id: String,
    message: Message,
}

/// The result of planning where to cut the live message list: how many
/// leading messages to summarize, and the event id of the first kept
/// message (the new compaction boundary).
#[derive(Debug, Clone)]
struct CutPlan {
    summarized: usize,
    first_kept_event_id: Option<String>,
}

/// A completed compaction: the merged summary text, the kept tail messages
/// (to follow the injected summary message), and bookkeeping for the
/// transcript marker and the user-facing notification.
#[derive(Debug, Clone)]
pub struct Compaction {
    /// The full summary string (preamble + sections + brief transcript),
    /// ready to inject as a single user message at the head of the kept tail.
    pub summary: String,
    /// The kept tail messages, in order. The new history is
    /// [summary_message] + kept_messages.
    pub kept_messages: Vec<Message>,
    /// Number of live messages folded into the summary (excluding the prior
    /// summary message itself).
    pub summarized_count: usize,
    /// Number of messages kept in the tail.
    pub kept_count: usize,
    /// Event id of the first kept message — recorded in the
    /// SessionEventKind::Compaction marker so a resumed session rebuilds
    /// the same compacted history. None when nothing was kept (compact-all).
    pub first_kept_event_id: Option<String>,
    /// Event ids `[first, last]` of the summarized range — every live
    /// message folded into this summary. Recorded in the marker so
    /// `/recall scope:compaction:N` can resolve the range to global message
    /// indices and search within it. `None` only when the live list was
    /// empty (compact refused earlier); in practice always `Some`.
    pub summarized_range: Option<[String; 2]>,
}

/// Options for compact.
#[derive(Clone, Default)]
pub struct CompactOptions {
    /// Soft token budget (chars/4) for the kept tail. When the most recent
    /// turn alone exceeds it, the cut is pushed back to a completed
    /// tool-cycle boundary so the oversized turn is partly summarized too.
    /// 0 disables the budget guard.
    pub max_kept_tokens: usize,
    /// Tiered-retention context editing applied to the kept tail (see
    /// [`crate::context_edit`]). When `enabled` is false the tail is carried
    /// verbatim.
    pub edit: lofi_types::EditConfig,
    /// Compaction hooks (e.g. lofi-code's tool-API section). Each hook
    /// receives the normalized transcript blocks and returns additional
    /// summary sections.
    pub hooks: Vec<Arc<dyn CompactionHook>>,
}

/// Minimum number of live messages the summarized prefix must contain for a
/// compaction to be worth running. Below this there is too little to fold —
/// the summary would be barely smaller than the original — so `compact`
/// returns `None` and the caller refuses (manual `/compact` notifies;
/// auto-compact simply no-ops).
const MIN_SUMMARIZED: usize = 5;

/// Run an offline compaction over the active path of events.
///
/// events is the full transcript event log; the active path (root -> leaf)
/// is walked the same way store::active_path_from_leaf does, so a branched
/// session compacts only the visible conversation. A prior
/// SessionEventKind::Compaction marker on the path supplies
/// previous_summary (its already-summarized range is excluded from the new
/// cut); None when this is the first compaction.
///
/// Returns None when there is nothing worth compacting (no live messages,
/// or too few to fold).
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn compact(events: &[SessionEvent], opts: &CompactOptions) -> Option<Compaction> {
    let path = store::active_path_from_leaf(events);
    if path.is_empty() {
        return None;
    }

    // Walk leaf -> root once to find the most recent compaction marker on the
    // active path (its summary is previous_summary; its first_kept_entry_id
    // is where the already-summarized range ends).
    let mut previous_summary: Option<String> = None;
    let mut live_start_id: Option<String> = None;
    let mut previous_marker_pos: Option<usize> = None;
    for (path_pos, &i) in path.iter().enumerate().rev() {
        if let SessionEventKind::Compaction {
            summary,
            first_kept_entry_id,
            ..
        } = &events[i].kind
        {
            previous_summary = Some(summary.clone());
            live_start_id = Some(first_kept_entry_id.clone());
            previous_marker_pos = Some(path_pos);
            break;
        }
    }

    // Build the live message list (root -> leaf) for the range
    // [live_start_id .. leaf], skipping failed-turn content just like
    // messages_from_events. An empty boundary means the prior compaction kept
    // nothing, so only events appended after its marker are live.
    // Native tools on the path are keyed by their parent exec id so the brief
    // transcript can attach them.
    let live_range: Vec<usize> = match &live_start_id {
        Some(start) if start.is_empty() => path
            .iter()
            .copied()
            .skip(previous_marker_pos.map_or(path.len(), |pos| pos + 1))
            .collect(),
        Some(start) => path
            .iter()
            .copied()
            .skip_while(|&i| events[i].id != *start)
            .collect(),
        None => path.clone(),
    };

    let mut live: Vec<LiveMessage> = Vec::new();
    let mut native_by_parent: HashMap<String, Vec<NativeToolRecord>> = HashMap::new();
    let mut skipping = false;
    for &i in &live_range {
        match &events[i].kind {
            SessionEventKind::TurnFailed { .. } => skipping = true,
            SessionEventKind::TurnEnd { .. } => skipping = false,
            SessionEventKind::Message(m) if !skipping => {
                live.push(LiveMessage {
                    event_id: events[i].id.clone(),
                    message: m.clone(),
                });
            }
            SessionEventKind::NativeTool(rec) => {
                native_by_parent
                    .entry(rec.parent.clone())
                    .or_default()
                    .push(rec.clone());
            }
            _ => {}
        }
    }

    // When there is a prior compaction on the active path, the on-disk
    // transcript stores the *original* unedited messages from the old kept
    // tail (the span between the prior Compaction marker's
    // first_kept_entry_id and the marker itself). The in-memory history the
    // agent runs on was context-edited at the previous compact time, but
    // that edit was never written back to disk. Loading the originals
    // verbatim inflates the live list with huge tool results the agent no
    // longer sees, so a re-compact barely shrinks the context and plan_cut's
    // token estimates are wrong.
    //
    // Fix: apply edit_tail to the entire live list before plan_cut when a
    // prior compaction is in effect. The keep_* counters count from the end
    // (newest messages), so recent results/thinking/calls in the new turns
    // are kept verbatim while the old kept tail's results are stubbed —
    // exactly matching what the agent sees. This is cache-safe: compact()
    // already rebuilds the prefix. The final edit_tail on the kept tail after
    // plan_cut is then idempotent (stubs stay stubs).
    if previous_summary.is_some() && opts.edit.enabled && !live.is_empty() {
        let pairs: Vec<(String, Message)> = live
            .iter()
            .map(|lm| (lm.event_id.clone(), lm.message.clone()))
            .collect();
        let edited = crate::context_edit::edit_tail(&pairs, &opts.edit);
        for (lm, msg) in live.iter_mut().zip(edited) {
            lm.message = msg;
        }
    }

    // Strip a leading prior-summary user message from the live list: it is
    // not a real turn and must not be re-summarized — its content is merged
    // in via previous_summary. (On the first compaction there is none.)
    if previous_summary.is_some() {
        if let Some(first) = live.first() {
            let text = user_text(&first.message);
            if text.starts_with(HANDOFF_PREAMBLE) {
                live.remove(0);
            }
        }
    }

    if live.len() < 3 {
        return None;
    }

    let plan = plan_cut(&live, opts);
    if plan.summarized < MIN_SUMMARIZED {
        return None;
    }

    let prefix_messages: Vec<&Message> = live[..plan.summarized]
        .iter()
        .map(|lm| &lm.message)
        .collect();
    let blocks = normalize(&prefix_messages, &native_by_parent);
    let fresh = build_summary(&blocks, &opts.hooks);
    let summary = match &previous_summary {
        Some(prev) => merge_previous(prev, &fresh),
        None => fresh,
    };

    let kept_count = live.len().saturating_sub(plan.summarized);
    let kept_live = &live[plan.summarized..];
    let kept_pairs: Vec<(String, Message)> = kept_live
        .iter()
        .map(|lm| (lm.event_id.clone(), lm.message.clone()))
        .collect();
    // Apply tiered-retention context editing to the kept tail so the new
    // prefix is much lighter (old tool results/thinking/tool-call code
    // elided, recall-recoverable). Cache-safe: this rides the prefix
    // rebuild compaction already pays.
    let kept_messages: Vec<Message> = crate::context_edit::edit_tail(&kept_pairs, &opts.edit);

    // The summarized range is `live[0 .. plan.summarized]`. Recorded as
    // event ids so `/recall scope:compaction:N` can resolve it to global
    // message indices without re-deriving the cut. Compact-all (summarized
    // == live.len()) collapses the whole live list; the range still spans
    // first..last.
    let summarized_range: Option<[String; 2]> = if plan.summarized > 0 {
        let first = live[0].event_id.clone();
        let last = live[plan.summarized - 1].event_id.clone();
        Some([first, last])
    } else {
        None
    };

    Some(Compaction {
        summary,
        kept_messages,
        summarized_count: plan.summarized,
        kept_count,
        first_kept_event_id: plan.first_kept_event_id,
        summarized_range,
    })
}

/// Build the new history for an agent run from a Compaction: the summary
/// as a single user message, followed by the kept tail.
#[must_use]
pub fn compacted_history(compaction: &Compaction) -> Vec<Message> {
    let mut out = Vec::with_capacity(compaction.kept_messages.len() + 1);
    if !compaction.summary.is_empty() {
        out.push(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: compaction.summary.clone(),
            }],
        });
    }
    out.extend(compaction.kept_messages.iter().cloned());
    out
}

// ── cut planning ──────────────────────────────────────────────────────────

/// Decide where to cut the live message list. The default cut is at the last
/// user prompt whose response cycle is complete — i.e. keep the most recent
/// turn whole and summarize everything before it. With only one user prompt,
/// fall back to a completed tool-cycle boundary in the first half, then to
/// compact-all. The kept-tail token budget, when set, splits an oversized
/// final turn at a completed tool-cycle so it does not re-overflow.
fn plan_cut(live: &[LiveMessage], opts: &CompactOptions) -> CutPlan {
    let user_indices: Vec<usize> = live
        .iter()
        .enumerate()
        .filter(|(_, lm)| is_real_user(&lm.message))
        .map(|(i, _)| i)
        .collect();

    if let Some(&last_user) = user_indices.last() {
        let mut cut = last_user;
        // In-progress-turn guard: if the turn after the last user prompt has
        // an unmatched tool call, push the cut back to the previous prompt.
        if has_unmatched_tool_call(live, last_user) {
            if let Some(&prev) = user_indices.iter().rev().nth(1) {
                cut = prev;
            }
        }
        // Oversized-turn guard: split the kept suffix at a completed
        // tool-cycle so an oversized final turn is partly summarized.
        // `cut == 0` (a single turn with no prior prompts) must also enter
        // this path — otherwise the entire history is kept verbatim and
        // `summarized == 0 < MIN_SUMMARIZED` makes compact refuse.
        if opts.max_kept_tokens > 0 {
            let suffix_tokens = estimate_tokens(&live[cut..]);
            if suffix_tokens > opts.max_kept_tokens {
                if let Some(split) = find_suffix_split(live, cut, opts.max_kept_tokens) {
                    cut = split;
                } else {
                    return CutPlan {
                        summarized: live.len(),
                        first_kept_event_id: None,
                    };
                }
            }
        }
        return CutPlan {
            summarized: cut,
            first_kept_event_id: Some(live[cut].event_id.clone()),
        };
    }

    // No user prompt: single agentic chain — find a completed tool-cycle
    // boundary in the first half and cut there; else compact-all.
    if let Some(mid) = find_mid_cycle_boundary(live) {
        if mid > 0 && mid < live.len() - 1 {
            return CutPlan {
                summarized: mid + 1,
                first_kept_event_id: Some(live[mid + 1].event_id.clone()),
            };
        }
    }
    CutPlan {
        summarized: live.len(),
        first_kept_event_id: None,
    }
}

/// Whether m is a real user prompt (not a tool-result-only message and not
/// empty).
fn is_real_user(m: &Message) -> bool {
    if m.role != Role::User {
        return false;
    }
    !m.blocks
        .iter()
        .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// Whether the turn starting at the user prompt from has an assistant tool
/// call with no matching tool result (i.e. the cycle is incomplete).
fn has_unmatched_tool_call(live: &[LiveMessage], from: usize) -> bool {
    let mut calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut results: std::collections::HashSet<String> = std::collections::HashSet::new();
    for lm in live.iter().skip(from + 1) {
        if is_real_user(&lm.message) {
            break;
        }
        for b in &lm.message.blocks {
            match b {
                ContentBlock::ToolUse { id, .. } => calls.insert(id.clone()),
                ContentBlock::ToolResult { tool_use_id, .. } => results.insert(tool_use_id.clone()),
                _ => false,
            };
        }
    }
    calls.iter().any(|c| !results.contains(c))
}

/// Find a completed tool-cycle boundary (a tool-result closing the last open
/// tool call) nearest the midpoint of the first half. Used when there is no
/// user prompt to cut at.
fn find_mid_cycle_boundary(live: &[LiveMessage]) -> Option<usize> {
    let mut cycle_ends: Vec<usize> = Vec::new();
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, lm) in live.iter().enumerate() {
        if is_real_user(&lm.message) {
            pending.clear();
            continue;
        }
        for b in &lm.message.blocks {
            match b {
                ContentBlock::ToolUse { id, .. } => {
                    pending.insert(id.clone());
                }
                ContentBlock::ToolResult { tool_use_id, .. } => {
                    pending.remove(tool_use_id);
                }
                _ => {}
            }
        }
        if pending.is_empty()
            && lm
                .message
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            cycle_ends.push(i);
        }
    }
    if cycle_ends.is_empty() {
        return None;
    }
    let target = live.len() / 2;
    cycle_ends.into_iter().min_by_key(|&i| i.abs_diff(target))
}

/// Find a completed tool-cycle boundary inside the kept suffix such that the
/// tail after it fits the token budget, keeping as much recent context as
/// possible. Returns the index of the first message to keep.
fn find_suffix_split(live: &[LiveMessage], cut: usize, budget_tokens: usize) -> Option<usize> {
    let suffix = &live[cut..];
    let mut cycle_ends: Vec<usize> = Vec::new();
    let mut pending: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (i, lm) in suffix.iter().enumerate() {
        if is_real_user(&lm.message) {
            pending.clear();
            continue;
        }
        for b in &lm.message.blocks {
            match b {
                ContentBlock::ToolUse { id, .. } => pending.insert(id.clone()),
                ContentBlock::ToolResult { tool_use_id, .. } => pending.remove(tool_use_id),
                _ => false,
            };
        }
        if pending.is_empty()
            && lm
                .message
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            cycle_ends.push(i);
        }
    }
    let mut tail_tokens = vec![0usize; suffix.len() + 1];
    for i in (0..suffix.len()).rev() {
        tail_tokens[i] = tail_tokens[i + 1] + estimate_message_tokens(&suffix[i].message);
    }
    for &boundary in &cycle_ends {
        let keep_from = boundary + 1;
        if keep_from < suffix.len() && tail_tokens[keep_from] <= budget_tokens {
            return Some(cut + keep_from);
        }
    }
    None
}

// ── normalization ─────────────────────────────────────────────────────────

/// Flatten a slice of messages (the summarized prefix) into Blocks,
/// attaching the native tool calls that ran inside each exec.
fn normalize(
    messages: &[&Message],
    native_by_parent: &HashMap<String, Vec<NativeToolRecord>>,
) -> Vec<CompactBlock> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::User => {
                let text = user_text(m);
                if !text.trim().is_empty() {
                    out.push(CompactBlock::User { text });
                }
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = b
                    {
                        out.push(CompactBlock::ToolResult {
                            id: tool_use_id.clone(),
                            text: exec_result_display(content, *is_error),
                            is_error: *is_error,
                        });
                    }
                }
            }
            Role::Assistant => {
                let mut text_buf = String::new();
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                text_buf.push_str(text);
                            }
                        }
                        ContentBlock::ToolUse { id, name, input } if name == "exec" => {
                            if !text_buf.is_empty() {
                                out.push(CompactBlock::Assistant {
                                    text: std::mem::take(&mut text_buf),
                                });
                            }
                            let (code, label) = crate::agent::exec_input_code_and_label(input);
                            let native = native_by_parent.get(id).cloned().unwrap_or_default();
                            out.push(CompactBlock::ToolCall {
                                id: id.clone(),
                                code,
                                label,
                                native,
                            });
                        }
                        ContentBlock::ToolUse { id, name, .. } => {
                            if !text_buf.is_empty() {
                                out.push(CompactBlock::Assistant {
                                    text: std::mem::take(&mut text_buf),
                                });
                            }
                            out.push(CompactBlock::ToolCall {
                                id: id.clone(),
                                code: String::new(),
                                label: Some(name.clone()),
                                native: Vec::new(),
                            });
                        }
                        ContentBlock::Thinking { .. } | ContentBlock::ToolResult { .. } => {}
                    }
                }
                if !text_buf.is_empty() {
                    out.push(CompactBlock::Assistant { text: text_buf });
                }
            }
            Role::Tool => {
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = b
                    {
                        out.push(CompactBlock::ToolResult {
                            id: tool_use_id.clone(),
                            text: exec_result_display(content, *is_error),
                            is_error: *is_error,
                        });
                    }
                }
            }
            Role::System => {}
        }
    }
    out
}

/// Surface the displayable text of an exec result. On success the payload
/// is {"value":..., "logs":[...]}; extract value. On error, the content is
/// the error message verbatim.
fn exec_result_display(content: &str, is_error: bool) -> String {
    if is_error {
        return content.to_string();
    }
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|v| v.get("value").cloned())
        .map_or_else(
            || content.to_string(),
            |v| {
                if let Some(s) = v.as_str() {
                    s.to_string()
                } else {
                    serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
                }
            },
        )
}

// ── summary building ──────────────────────────────────────────────────────

const SEPARATOR: &str = "\n\n---\n\n";

/// Build the full fresh summary (no merging): preamble + ordered sections +
/// brief transcript.
fn build_summary(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> String {
    let goal = extract_session_goal(blocks);
    let prefs = extract_preferences(blocks);
    let outstanding = extract_outstanding(blocks, hooks);

    // Files and Commits are provided by hooks (they know the tool
    // vocabulary). Fall back to built-in extractors when no hook is
    // registered, so tests without hooks still work.
    let files: Vec<String> = hooks
        .iter()
        .find_map(|h| {
            let items = h.file_changes(blocks);
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_else(|| extract_files(blocks));
    let commits: Vec<String> = hooks
        .iter()
        .find_map(|h| {
            let items = h.commits(blocks);
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_else(|| extract_commits(blocks));

    let brief = build_brief(blocks, hooks);

    let mut stable: Vec<String> = [
        section("Session Goal", &goal),
        section("User Preferences", &prefs),
        section("Files And Changes", &files),
        section("Commits", &commits),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();

    // Hook sections (e.g. APIs Used from lofi-code) go after the stable
    // built-in sections, before the volatile Outstanding Context.
    for hook in hooks {
        for sec in hook.sections(blocks) {
            let s = section(&sec.title, &sec.items);
            if !s.is_empty() {
                stable.push(s);
            }
        }
    }

    let volatile: Vec<String> = [section("Outstanding Context", &outstanding)]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    let mut parts: Vec<String> = Vec::new();
    let headers = [stable, volatile].concat();
    if !headers.is_empty() {
        parts.push(headers.join("\n\n"));
    }
    if !brief.is_empty() {
        parts.push(cap_brief(&brief));
    }
    if parts.is_empty() {
        return String::new();
    }
    let body = parts.join(SEPARATOR);
    format!("{HANDOFF_PREAMBLE}\n\n{body}")
}

/// Format a section: [Title] header followed by - item lines.
fn section(title: &str, items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let body = items
        .iter()
        .map(|i| format!("- {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[{title}]\n{body}")
}

/// Session Goal: the first few substantive user prompts, clipped.
fn extract_session_goal(blocks: &[CompactBlock]) -> Vec<String> {
    let mut out = Vec::new();
    for b in blocks {
        if let CompactBlock::User { text } = b {
            let t = text.trim();
            if t.is_empty() || out.len() >= 8 {
                continue;
            }
            out.push(clip(t, 200));
        }
    }
    out
}

/// User Preferences: user lines that read as a directive.
fn extract_preferences(blocks: &[CompactBlock]) -> Vec<String> {
    const SIGNALS: &[&str] = &[
        "prefer",
        "use ",
        "don't",
        "do not",
        "always",
        "never",
        "please",
        "make sure",
        "avoid",
        "keep ",
        "no need",
        "no longer",
        "instead of",
        "rather than",
    ];
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in blocks {
        let CompactBlock::User { text } = b else {
            continue;
        };
        let lower = text.to_lowercase();
        if !SIGNALS.iter().any(|s| lower.contains(s)) {
            continue;
        }
        let line = clip(text.trim(), 160);
        if line.len() < 8 {
            continue;
        }
        let key = line.to_lowercase();
        if seen.insert(key) {
            out.push(line);
        }
        if out.len() >= 15 {
            break;
        }
    }
    out
}

/// Files And Changes from native tool calls: read/bash_read -> Read,
/// edit -> Modified, write -> Created. Dedup; Created drops files already
/// in Modified.
fn extract_files(blocks: &[CompactBlock]) -> Vec<String> {
    let mut modified: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut created: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut read: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in blocks {
        let CompactBlock::ToolCall { native, .. } = b else {
            continue;
        };
        for rec in native {
            if rec.is_error {
                continue;
            }
            match rec.name.as_str() {
                "edit" if !rec.args.is_empty() => {
                    modified.insert(rec.args.clone());
                }
                "write" if !rec.args.is_empty() => {
                    created.insert(rec.args.clone());
                }
                "read" | "bash_read" if !rec.args.is_empty() => {
                    read.insert(rec.args.clone());
                }
                _ => {}
            }
        }
    }
    for p in &modified {
        created.remove(p);
    }
    let cap = |set: &std::collections::HashSet<String>, limit: usize| -> String {
        let mut arr: Vec<String> = set.iter().cloned().collect();
        arr.sort();
        if arr.len() <= limit {
            arr.join(", ")
        } else {
            let kept = arr[..limit].join(", ");
            format!("{kept}, +recall: {}", arr[limit..].join(", "))
        }
    };
    let mut lines = Vec::new();
    if !modified.is_empty() {
        lines.push(format!("Modified: {}", cap(&modified, 10)));
    }
    if !created.is_empty() {
        lines.push(format!("Created: {}", cap(&created, 10)));
    }
    if !read.is_empty() {
        lines.push(format!("Read: {}", cap(&read, 10)));
    }
    lines
}

/// Commits from bash native calls whose command runs git commit.
fn extract_commits(blocks: &[CompactBlock]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in blocks {
        let CompactBlock::ToolCall { native, .. } = b else {
            continue;
        };
        for rec in native {
            if rec.name != "bash" || !rec.args.contains("git commit") {
                continue;
            }
            let msg =
                extract_commit_message(&rec.args).unwrap_or_else(|| "(git commit)".to_string());
            let hash = first_hash(&rec.result);
            let line = match hash {
                Some(h) => format!("{h} {msg}"),
                None => msg,
            };
            if seen.insert(line.clone()) {
                out.push(line);
            }
            if out.len() >= 8 {
                break;
            }
        }
    }
    out
}

/// Extract the -m "message" (or -m 'message') from a git commit command.
fn extract_commit_message(cmd: &str) -> Option<String> {
    let m = cmd.find("-m")?;
    let rest = cmd[m + 2..].trim_start();
    let quote = rest.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let body = &rest[1..];
    let end = body.find(quote)?;
    Some(clip(body[..end].trim(), 120))
}

/// First short git hash in a bash result string.
fn first_hash(text: &str) -> Option<String> {
    let re = regex::Regex::new(r"\b[0-9a-f]{7,12}\b").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

/// The blocker regex. The pattern is a compile-time constant so this
/// always succeeds; the fallback is defensive only.
#[allow(clippy::unwrap_used)]
fn blocker_regex() -> regex::Regex {
    regex::Regex::new(BLOCKER_RE)
        .or_else(|_| regex::Regex::new("$^"))
        .or_else(|_| regex::Regex::new("."))
        .unwrap()
}

/// Outstanding Context: errors and blockers from the recent tail (last ~25
/// blocks). Tagged by severity.
fn extract_outstanding(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> Vec<String> {
    let blocker = blocker_regex();
    let tail = if blocks.len() > 25 {
        &blocks[blocks.len() - 25..]
    } else {
        blocks
    };
    let compress = |text: &str, max: usize| -> String {
        for hook in hooks {
            if let Some(s) = hook.compress_tool_result(text, max) {
                return s;
            }
        }
        compress_tool_result(text, max)
    };
    let mut items: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in tail {
        match b {
            CompactBlock::ToolResult {
                text,
                is_error: true,
                ..
            } => {
                let s = format!("[ERROR] {}", compress(text, 150));
                if seen.insert(s.clone()) {
                    items.push(s);
                }
            }
            CompactBlock::ToolCall { native, .. } => {
                for rec in native {
                    if !rec.is_error {
                        continue;
                    }
                    let s = format!("[ERROR] {}: {}", rec.name, first_line(&rec.result, 120));
                    if seen.insert(s.clone()) {
                        items.push(s);
                    }
                }
            }
            CompactBlock::Assistant { text } | CompactBlock::User { text } => {
                for line in non_empty_lines(text) {
                    if line.len() < 15 || !blocker.is_match(&line) {
                        continue;
                    }
                    let s = clip(&line, 150);
                    if seen.insert(s.clone()) {
                        items.push(s);
                    }
                    break;
                }
            }
            CompactBlock::ToolResult {
                is_error: false, ..
            } => {}
        }
        if items.len() >= 8 {
            break;
        }
    }
    items
}

// ── brief transcript ──────────────────────────────────────────────────────

const BRIEF_MAX_LINES: usize = 120;
const TRUNC_USER: usize = 256;
const TRUNC_ASSISTANT: usize = 200;

/// Build the compressed per-turn transcript: [user]/[assistant] sections
/// with clipped text and one-liner tool actions.
fn build_brief(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> String {
    let compress = |text: &str, max: usize| -> String {
        for hook in hooks {
            if let Some(s) = hook.compress_tool_result(text, max) {
                return s;
            }
        }
        compress_tool_result(text, max)
    };
    let full_path = |text: &str| -> Option<String> {
        for hook in hooks {
            if let Some(p) = hook.full_output_path(text) {
                return Some(p);
            }
        }
        extract_full_output_path(text).map(str::to_string)
    };

    let mut lines: Vec<String> = Vec::new();
    let mut last_header = "";
    for b in blocks {
        match b {
            CompactBlock::User { text } => {
                let t = clip(text.trim(), TRUNC_USER);
                if t.is_empty() {
                    continue;
                }
                if last_header != "[user]" {
                    lines.push("[user]".to_string());
                    last_header = "[user]";
                }
                lines.push(t);
            }
            CompactBlock::Assistant { text } => {
                let t = clip(text.trim(), TRUNC_ASSISTANT);
                if t.is_empty() {
                    continue;
                }
                if last_header != "[assistant]" {
                    lines.push("[assistant]".to_string());
                    last_header = "[assistant]";
                }
                lines.push(t);
            }
            CompactBlock::ToolCall {
                code,
                label,
                native,
                ..
            } => {
                if last_header != "[assistant]" {
                    lines.push("[assistant]".to_string());
                    last_header = "[assistant]";
                }
                if native.is_empty() {
                    let l = label.clone().unwrap_or_else(|| first_line(code, 60));
                    lines.push(format!("* exec \"{l}\""));
                } else {
                    for rec in native {
                        let args = clip(&rec.args, 80);
                        let marker = if rec.is_error { " (error)" } else { "" };
                        lines.push(format!("* {} \"{}\"{}", rec.name, args, marker));
                    }
                }
            }
            CompactBlock::ToolResult {
                text,
                is_error: true,
                ..
            } => {
                let body = compress(text, 150);
                if body.is_empty() {
                    continue;
                }
                if last_header != "[tool_error]" {
                    lines.push("[tool_error]".to_string());
                    last_header = "[tool_error]";
                }
                lines.push(body);
            }
            CompactBlock::ToolResult {
                text,
                is_error: false,
                ..
            } => {
                let body = compress(text, 120);
                if body.is_empty() {
                    continue;
                }
                if last_header != "[tool_result]" {
                    lines.push("[tool_result]".to_string());
                    last_header = "[tool_result]";
                }
                if let Some(path) = full_path(text) {
                    lines.push(format!("{body} ... Full output: {path}"));
                } else {
                    lines.push(body);
                }
            }
        }
    }
    lines.join("\n")
}

/// Cap the brief transcript to the last BRIEF_MAX_LINES lines, noting how
/// many earlier lines were omitted.
fn cap_brief(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= BRIEF_MAX_LINES {
        return text.to_string();
    }
    let omitted = lines.len() - BRIEF_MAX_LINES;
    let kept = &lines[lines.len() - BRIEF_MAX_LINES..];
    format!(
        "...({omitted} earlier lines omitted)\n\n{}",
        kept.join("\n")
    )
}

/// The blocker pattern for the Outstanding Context section: matches lines
/// that read as a failure or unresolved problem.
const BLOCKER_RE: &str = r"(?i)(fail(ed|s|ure|ing)?|broken|cannot|can't|won't work|does not work|doesn't work|still (broken|failing|wrong)|blocked|blocker|not (fixed|resolved|working)|crash(es|ed|ing)?)";

// ── merge with previous summary ───────────────────────────────────────────

const SECTION_HEADERS: &[&str] = &[
    "Session Goal",
    "User Preferences",
    "Files And Changes",
    "Commits",
    "Outstanding Context",
];

/// Merge a fresh summary into a previous one. Stable sections (Goal,
/// Preferences, Files, Commits) dedup and cap; the volatile section
/// (Outstanding Context) is replaced wholesale; the brief transcript
/// concatenates (prev then fresh), capped.
fn merge_previous(prev: &str, fresh: &str) -> String {
    let prev = strip_preamble(prev);
    let fresh = strip_preamble(fresh);

    let (prev_headers, prev_brief) = split_headers_brief(&prev);
    let (fresh_headers, fresh_brief) = split_headers_brief(&fresh);

    let mut merged_headers: Vec<String> = Vec::new();
    let mut handled: std::collections::HashSet<String> = std::collections::HashSet::new();
    for header in SECTION_HEADERS {
        handled.insert((*header).to_string());
        let p = section_of(&prev_headers, header);
        let f = section_of(&fresh_headers, header);
        let merged = merge_section(header, &p, &f);
        if !merged.is_empty() {
            merged_headers.push(merged);
        }
    }

    // Carry forward hook-provided sections (any [Header] not in
    // SECTION_HEADERS). Fresh sections replace prev ones of the same name;
    // prev-only sections are kept so hook data is never lost on re-compact.
    for header in extra_section_names(&prev_headers, &fresh_headers, &handled) {
        let p = section_of(&prev_headers, &header);
        let f = section_of(&fresh_headers, &header);
        let merged = if f.is_empty() {
            format!("[{header}]\n{p}")
        } else {
            merge_section(&header, &p, &f)
        };
        if !merged.is_empty() {
            merged_headers.push(merged);
        }
    }

    let mut brief = String::new();
    if !prev_brief.is_empty() {
        brief.push_str(&prev_brief);
        brief.push_str("\n\n");
    }
    brief.push_str(&fresh_brief);

    let mut parts: Vec<String> = Vec::new();
    if !merged_headers.is_empty() {
        parts.push(merged_headers.join("\n\n"));
    }
    if !brief.is_empty() {
        parts.push(cap_brief(&brief));
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{HANDOFF_PREAMBLE}\n\n{}", parts.join(SEPARATOR))
}

/// Remove the leading preamble so only the section body remains.
fn strip_preamble(text: &str) -> String {
    text.strip_prefix(HANDOFF_PREAMBLE).map_or_else(
        || text.to_string(),
        |rest| rest.trim_start_matches('\n').to_string(),
    )
}

/// Split a summary body into its [Header] block (all sections) and the
/// brief transcript (everything after the --- separator).
fn split_headers_brief(body: &str) -> (String, String) {
    match body.split_once(SEPARATOR) {
        Some((h, b)) => (h.trim().to_string(), b.trim().to_string()),
        None => (body.trim().to_string(), String::new()),
    }
}

/// Extract a single [Header] section block (header line + body lines) from
/// the headers text.
fn section_of(headers: &str, name: &str) -> String {
    let tag = format!("[{name}]");
    let start = headers.find(&tag).map(|i| i + tag.len());
    let Some(start) = start else {
        return String::new();
    };
    let rest = &headers[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

/// Collect `[Header]` names from both prev and fresh headers that are not in
/// the built-in `SECTION_HEADERS` set. These are hook-provided sections that
/// must be carried through merges. Returns names in order of first appearance
/// (prev then fresh), deduplicated.
fn extra_section_names(
    prev: &str,
    fresh: &str,
    builtin: &std::collections::HashSet<String>,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for text in [prev, fresh] {
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some(name) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let name = name.to_string();
                if !builtin.contains(&name) && seen.insert(name.clone()) {
                    out.push(name);
                }
            }
        }
    }
    out
}

/// Merge one section. Outstanding Context is volatile (fresh only). Files And
/// Changes is unioned across categories. The rest dedup body lines and cap.
fn merge_section(name: &str, prev: &str, fresh: &str) -> String {
    if name == "Outstanding Context" {
        if fresh.is_empty() {
            return String::new();
        }
        return format!("[{name}]\n{fresh}");
    }
    if name == "Files And Changes" {
        return merge_files(prev, fresh);
    }
    let cap = if matches!(name, "Session Goal" | "Commits") {
        8
    } else {
        15
    };
    let mut lines: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in prev.split('\n').chain(fresh.split('\n')) {
        let l = line.trim();
        if l.is_empty() || l.starts_with('[') {
            continue;
        }
        let body = l.strip_prefix("- ").unwrap_or(l).to_string();
        if seen.insert(body.clone()) {
            lines.push(body);
        }
    }
    if lines.is_empty() {
        return String::new();
    }
    if lines.len() > cap {
        lines.truncate(cap);
    }
    let body = lines
        .iter()
        .map(|l| format!("- {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[{name}]\n{body}")
}

/// Merge Files And Changes by unioning each category path set.
fn merge_files(prev: &str, fresh: &str) -> String {
    let mut modified: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut created: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut read: std::collections::HashSet<String> = std::collections::HashSet::new();
    for text in [prev, fresh] {
        for line in text.split('\n') {
            let l = line.trim().strip_prefix("- ").unwrap_or(line.trim());
            if let Some(rest) = l.strip_prefix("Modified: ") {
                for p in split_paths(rest) {
                    modified.insert(p);
                }
            } else if let Some(rest) = l.strip_prefix("Created: ") {
                for p in split_paths(rest) {
                    created.insert(p);
                }
            } else if let Some(rest) = l.strip_prefix("Read: ") {
                for p in split_paths(rest) {
                    read.insert(p);
                }
            }
        }
    }
    for p in &modified {
        created.remove(p);
    }
    let cap = |set: &std::collections::HashSet<String>, limit: usize| -> String {
        let mut arr: Vec<String> = set.iter().cloned().collect();
        arr.sort();
        if arr.len() <= limit {
            arr.join(", ")
        } else {
            let kept = arr[..limit].join(", ");
            format!("{kept}, +recall: {}", arr[limit..].join(", "))
        }
    };
    let mut lines = Vec::new();
    if !modified.is_empty() {
        lines.push(format!("- Modified: {}", cap(&modified, 10)));
    }
    if !created.is_empty() {
        lines.push(format!("- Created: {}", cap(&created, 10)));
    }
    if !read.is_empty() {
        lines.push(format!("- Read: {}", cap(&read, 10)));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!("[Files And Changes]\n{}", lines.join("\n"))
}

/// Split a "Modified: a, b, c (+recall: d, e)" line into individual paths,
/// dropping the +recall marker.
fn split_paths(rest: &str) -> Vec<String> {
    let no_recall = rest.split("+recall:").next().unwrap_or(rest);
    no_recall
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

// ── text helpers ──────────────────────────────────────────────────────────

/// Concatenate all text blocks of a message (used for user/assistant text).
fn user_text(m: &Message) -> String {
    m.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Clip text to max chars on a word boundary, avoiding splitting a
/// surrogate pair.
fn clip(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut end_byte = 0;
    for (i, (b, _)) in text.char_indices().enumerate() {
        if i == max {
            end_byte = b;
            break;
        }
    }
    let window = &text[..end_byte];
    let mut cut = window
        .rfind(' ')
        .filter(|&i| i > end_byte * 3 / 5)
        .unwrap_or(end_byte);
    if cut > 0 && text.is_char_boundary(cut) {
        let prev = &text[..cut];
        if let Some(last) = prev.chars().next_back() {
            if ((last as u32) & 0xFFFF) >= 0xD800 && (last as u32) <= 0xDBFF {
                if let Some((p, _)) = prev.char_indices().next_back() {
                    cut = p;
                }
            }
        }
    }
    text[..cut].trim_end().to_string()
}

/// First non-empty line of text, clipped to max.
fn first_line(text: &str, max: usize) -> String {
    let line = text.split('\n').next().unwrap_or("").trim();
    clip(line, max)
}

/// Compress a tool result into a compact, meaningful summary.
///
/// Instead of just taking the first line (which may be `{` for JSON output),
/// this tries to extract the most useful information:
/// - For JSON objects, it picks out key fields like `output`, `ok`, `code`, etc.
/// - For multi-line text, it takes the first few non-empty lines.
/// - Falls back to first_line for single-line results.
fn compress_tool_result(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    // Single-line results: just clip.
    if !trimmed.contains('\n') {
        return clip(trimmed, max);
    }

    // Try JSON parsing for structured tool output.
    if trimmed.starts_with('{') {
        if let Some(summary) = compress_json_result(trimmed, max) {
            return summary;
        }
    }

    // For multi-line text, take up to 3 non-empty lines and join with " | ".
    let lines: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let joined = lines.join(" | ");
    clip(&joined, max)
}

/// Extract a compact summary from a JSON tool result object.
/// Picks out the most informative fields and formats them as `key=value` pairs.
fn compress_json_result(text: &str, max: usize) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    // Priority fields that carry the most useful info.
    let priority = [
        "output", "error", "stderr", "stdout", "path", "value", "result", "content", "message",
        "text",
    ];
    let mut parts: Vec<String> = Vec::new();

    // First, grab priority fields.
    for key in &priority {
        if let Some(val) = obj.get(*key) {
            let s = json_value_brief(val, 60);
            if !s.is_empty() {
                parts.push(format!("{key}={s}"));
            }
        }
    }

    // Then grab status-ish fields.
    for key in &["ok", "code", "status", "signal", "is_error", "duration_ms"] {
        if let Some(val) = obj.get(*key) {
            let s = json_value_brief(val, 30);
            if !s.is_empty() {
                parts.push(format!("{key}={s}"));
            }
        }
    }

    if parts.is_empty() {
        // No recognised fields; show first 3 keys.
        for (k, v) in obj.iter().take(3) {
            let s = json_value_brief(v, 40);
            if !s.is_empty() {
                parts.push(format!("{k}={s}"));
            }
        }
    }

    if parts.is_empty() {
        return None;
    }
    Some(clip(&parts.join(" "), max))
}

/// Render a JSON value as a brief string for inclusion in a compressed summary.
fn json_value_brief(v: &serde_json::Value, max: usize) -> String {
    match v {
        serde_json::Value::String(s) => clip(s.trim(), max),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Array(a) => {
            if a.is_empty() {
                "[]".to_string()
            } else {
                format!("[{} items]", a.len())
            }
        }
        serde_json::Value::Object(o) => {
            if o.is_empty() {
                "{}".to_string()
            } else {
                format!("{{{} fields}}", o.len())
            }
        }
    }
}

/// Extract the absolute path from a bash truncation notice like
/// "Full output: /tmp/lofi-bash-xxxx.log. Use lofi.read(...)".
fn extract_full_output_path(text: &str) -> Option<&str> {
    let rest = text.split("Full output: ").nth(1)?;
    let path = rest.split(". Use lofi.read").next()?.trim();
    if path.starts_with('/') {
        Some(path)
    } else {
        None
    }
}

/// Non-empty, trimmed lines of text.
fn non_empty_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Rough token estimate (chars/4) for a message.
fn estimate_message_tokens(m: &Message) -> usize {
    let chars: usize = m
        .blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
            ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
        })
        .sum();
    chars / 4
}

/// Rough token estimate for a slice of messages.
fn estimate_tokens(msgs: &[LiveMessage]) -> usize {
    msgs.iter()
        .map(|lm| estimate_message_tokens(&lm.message))
        .sum()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn user(t: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: t.into() }],
        }
    }
    fn assistant(t: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: t.into() }],
        }
    }
    fn exec_call(id: &str, code: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "exec".into(),
                input: serde_json::json!({ "code": code }),
            }],
        }
    }
    fn exec_result(id: &str, value: &str) -> Message {
        Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: serde_json::json!({ "value": value, "logs": [] }).to_string(),
                is_error: false,
            }],
        }
    }
    fn native(parent: &str, name: &str, args: &str) -> NativeToolRecord {
        NativeToolRecord {
            parent: parent.into(),
            call_id: 0,
            name: name.into(),
            args: args.into(),
            result: String::new(),
            is_error: false,
        }
    }
    fn events_of(msgs: &[Message]) -> Vec<SessionEvent> {
        let mut out = Vec::with_capacity(msgs.len());
        for (i, m) in msgs.iter().enumerate() {
            out.push(SessionEvent {
                id: format!("e{i}"),
                parent_id: if i == 0 {
                    None
                } else {
                    Some(format!("e{}", i - 1))
                },
                kind: SessionEventKind::Message(m.clone()),
            });
        }
        out
    }

    #[test]
    fn clip_word_boundary() {
        assert_eq!(clip("hello world foo bar", 11), "hello world");
        assert_eq!(clip("short", 10), "short");
    }

    #[test]
    fn compact_returns_none_for_too_few() {
        let events = events_of(&[user("hi"), assistant("hello")]);
        assert!(compact(&events, &CompactOptions::default()).is_none());
    }

    #[test]
    fn compact_refuses_when_too_little_to_fold() {
        // Four messages: the prefix before the last user prompt is only two
        // messages — below MIN_SUMMARIZED, so compact returns None.
        let events = events_of(&[
            user("hi"),
            assistant("hello"),
            user("again"),
            assistant("sure"),
        ]);
        assert!(compact(&events, &CompactOptions::default()).is_none());
    }

    #[test]
    fn compact_keeps_last_turn_and_summarizes_prefix() {
        let msgs = [
            user("Please add a login form to auth.ts"),
            assistant("I'll edit auth.ts to add the form."),
            exec_call("t1", "lofi.edit"),
            exec_result("t1", "ok"),
            assistant("Done."),
            user("Now add tests please"),
            assistant("Adding tests."),
        ];
        let events = events_of(&msgs);
        let c = compact(&events, &CompactOptions::default()).expect("some compaction");
        assert!(c.summarized_count >= 1);
        assert!(c.summary.starts_with(HANDOFF_PREAMBLE));
        assert!(c.summary.contains("[Session Goal]"));
        assert!(c.summary.contains("login form"));
        assert_eq!(
            c.kept_messages.first().map(user_text).as_deref(),
            Some("Now add tests please")
        );
    }

    #[test]
    fn compact_files_and_changes_from_native_tools() {
        let mut events = events_of(&[
            user("edit auth.ts"),
            exec_call("t1", "lofi.edit"),
            exec_result("t1", "ok"),
            assistant("done"),
            user("thanks"),
            assistant("ok"),
            user("also add a test"),
            assistant("sure"),
        ]);
        events.push(SessionEvent {
            id: "n1".to_string(),
            parent_id: Some("e7".to_string()),
            kind: SessionEventKind::NativeTool(native("t1", "edit", "auth.ts")),
        });
        let c = compact(&events, &CompactOptions::default()).expect("some compaction");
        assert!(c.summary.contains("[Files And Changes]"));
        assert!(c.summary.contains("Modified: auth.ts"));
    }

    #[test]
    fn merge_previous_accumulates_goals() {
        let prev =
            format!("{HANDOFF_PREAMBLE}\n\n[Session Goal]\n- goal one\n\n---\n\n[user]\nold");
        let fresh =
            format!("{HANDOFF_PREAMBLE}\n\n[Session Goal]\n- goal two\n\n---\n\n[user]\nnew");
        let merged = merge_previous(&prev, &fresh);
        assert!(merged.contains("goal one"));
        assert!(merged.contains("goal two"));
    }

    /// On a re-compact, the on-disk transcript still has the *original* unedited
    /// messages from the prior compaction's kept tail. The live list must apply
    /// edit_tail to that old span so the second compact sees the same lightweight
    /// prefix the agent does, not the bloated originals.
    #[test]
    fn re_compact_after_compact_all_ignores_summarized_messages() {
        let mut events = events_of(&[user("OLD MESSAGE MUST STAY SUMMARIZED"), assistant("old")]);
        events.push(SessionEvent {
            id: "c1".to_string(),
            parent_id: Some("e1".to_string()),
            kind: SessionEventKind::Compaction {
                summary: format!("{HANDOFF_PREAMBLE}\n\n[prior summary]"),
                first_kept_entry_id: String::new(),
                summarized_range: ["e0".to_string(), "e1".to_string()],
                checkpointed_tail: false,
                summarized: 2,
                kept: 0,
            },
        });
        let new_messages = [
            user("new one"),
            assistant("reply one"),
            user("new two"),
            assistant("reply two"),
            user("new three"),
            assistant("reply three"),
            user("new four"),
            assistant("reply four"),
        ];
        let mut parent = "c1".to_string();
        for (i, message) in new_messages.into_iter().enumerate() {
            let id = format!("n{i}");
            events.push(SessionEvent {
                id: id.clone(),
                parent_id: Some(parent),
                kind: SessionEventKind::Message(message),
            });
            parent = id;
        }

        let c = compact(&events, &CompactOptions::default()).expect("compaction should succeed");
        assert!(c.summary.contains("[prior summary]"));
        assert!(!c.summary.contains("OLD MESSAGE MUST STAY SUMMARIZED"));
    }

    #[test]
    fn re_compact_edits_old_kept_tail_from_disk() {
        // Simulate the on-disk state after a hard-compact + continue:
        // [old turn with large tool results] [Compaction marker] [new turn]
        //
        // The old turn's tool results are huge on disk. Without the fix,
        // compact() loads them verbatim and the live list is inflated.
        // With the fix, edit_tail is applied to the old kept tail span,
        // shrinking the stubbed results before plan_cut runs.
        let big_result = "x".repeat(10_000);
        let mut events = events_of(&[
            user("do task"),
            exec_call("t1", "lofi.read"),
            exec_result("t1", &big_result),
            assistant("done"),
            // --- prior compaction kept tail starts here (e4) ---
            user("now continue"),
            exec_call("t2", "lofi.read"),
            exec_result("t2", &big_result),
            assistant("ok"),
            // --- prior compaction kept tail ends here ---
        ]);
        // Add a Compaction marker after e7. first_kept_entry_id = "e4".
        events.push(SessionEvent {
            id: "c1".to_string(),
            parent_id: Some("e7".to_string()),
            kind: SessionEventKind::Compaction {
                summary: format!("{HANDOFF_PREAMBLE}\n\n[prior summary]"),
                first_kept_entry_id: "e4".to_string(),
                summarized_range: ["e0".to_string(), "e3".to_string()],
                checkpointed_tail: false,
                summarized: 4,
                kept: 4,
            },
        });
        // New messages after the compaction marker (the continuation).
        events.push(SessionEvent {
            id: "e8".to_string(),
            parent_id: Some("c1".to_string()),
            kind: SessionEventKind::Message(user("what did you do?")),
        });
        events.push(SessionEvent {
            id: "e9".to_string(),
            parent_id: Some("e8".to_string()),
            kind: SessionEventKind::Message(assistant("I read a file")),
        });
        events.push(SessionEvent {
            id: "e10".to_string(),
            parent_id: Some("e9".to_string()),
            kind: SessionEventKind::Message(user("ok now add tests")),
        });
        events.push(SessionEvent {
            id: "e11".to_string(),
            parent_id: Some("e10".to_string()),
            kind: SessionEventKind::Message(exec_call("t3", "lofi.write")),
        });
        events.push(SessionEvent {
            id: "e12".to_string(),
            parent_id: Some("e11".to_string()),
            kind: SessionEventKind::Message(exec_result("t3", &big_result)),
        });
        events.push(SessionEvent {
            id: "e13".to_string(),
            parent_id: Some("e12".to_string()),
            kind: SessionEventKind::Message(assistant("Done adding tests")),
        });

        let opts = CompactOptions {
            edit: lofi_types::EditConfig {
                enabled: true,
                keep_results: 1,
                keep_thinking: 0,
                keep_calls: 1,
            },
            ..Default::default()
        };

        let c = compact(&events, &opts).expect("compaction should succeed");

        // The old kept tail's tool result (e6, 10k chars) should have been
        // edited to a short stub before plan_cut, so it does NOT appear in
        // the summary. Without the fix, the 10k-char result would be loaded
        // verbatim and inflate the prefix that gets summarized.
        assert!(c.summary.contains("[prior summary]"));
        assert!(
            !c.summary.contains(&"x".repeat(100)),
            "summary should not contain the big result from the old kept tail"
        );

        // The old kept tail's tool result that ended up in the summarized
        // prefix should have been stubbed — verify by checking the summary
        // contains the stub marker, not the raw content.
        assert!(
            c.summary.contains("cleared") || !c.summary.contains(&big_result),
            "old kept tail results should be stubbed, not carried verbatim into summary"
        );
    }
}
