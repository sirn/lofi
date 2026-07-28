#![allow(clippy::doc_markdown)]

use std::collections::HashMap;
use std::sync::Arc;

use lofi_types::{
    CompactBlock, CompactionHook, ContentBlock, Message, NativeToolRecord, Role, SessionEvent,
    SessionEventKind,
};

use crate::session::store;

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

#[derive(Debug, Clone)]
pub struct Compaction {
    pub summary: String,
    pub kept_messages: Vec<Message>,
    pub summarized_count: usize,
    /// Total original messages represented by the merged summary. This is
    /// persisted so resume preserves the history count across re-compactions.
    pub represented_count: usize,
    pub kept_count: usize,
    pub first_kept_event_id: Option<String>,
    pub summarized_range: Option<[String; 2]>,
}

#[derive(Clone, Default)]
pub struct CompactOptions {
    /// Soft token budget (chars/4) for the kept tail. When the most recent
    /// turn alone exceeds it, the cut is pushed back to a completed
    /// tool-cycle boundary so the oversized turn is partly summarized too.
    /// 0 disables the budget guard.
    pub max_kept_tokens: usize,
    pub edit: lofi_types::EditConfig,
    pub hooks: Vec<Arc<dyn CompactionHook>>,
}

const MIN_SUMMARIZED: usize = 5;

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn compact(events: &[SessionEvent], opts: &CompactOptions) -> Option<Compaction> {
    let path = store::active_path_from_leaf(events);
    if path.is_empty() {
        return None;
    }

    let mut previous_summary: Option<String> = None;
    // The summary stands in for this many messages. Keep that semantic count
    // across resume instead of treating the restored summary as one message
    // when applying the minimum-history economy guard below.
    let mut previously_summarized = 0usize;
    let mut live_start_id: Option<String> = None;
    let mut previous_marker_pos: Option<usize> = None;
    for (path_pos, &i) in path.iter().enumerate().rev() {
        if let SessionEventKind::Compaction {
            summary,
            first_kept_entry_id,
            summarized,
            represented,
            ..
        } = &events[i].kind
        {
            previous_summary = Some(summary.clone());
            previously_summarized = if *represented == 0 {
                *summarized
            } else {
                *represented
            };
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
    let live_start = match &live_start_id {
        Some(start) if start.is_empty() => previous_marker_pos.map_or(path.len(), |pos| pos + 1),
        Some(start) => path
            .iter()
            .position(|&i| events[i].id == *start)
            .unwrap_or(path.len()),
        None => 0,
    };

    let mut live: Vec<LiveMessage> = Vec::new();
    let mut native_by_parent: HashMap<String, Vec<NativeToolRecord>> = HashMap::new();
    let mut skipping = false;
    for &i in &path[live_start..] {
        match &events[i].kind {
            SessionEventKind::TurnFailed { .. } => skipping = true,
            SessionEventKind::TurnEnd { .. } => skipping = false,
            SessionEventKind::Message(m) if !skipping => {
                live.push(LiveMessage {
                    event_id: events[i].id.clone(),
                    message: m.clone(),
                });
            }
            SessionEventKind::UserBash {
                command,
                output,
                exit_code,
                signal,
                duration_ms,
                truncated,
                cancelled,
                exclude_from_context,
            } if !skipping && !exclude_from_context => {
                let result = crate::UserBashResult::from_session(
                    command.clone(),
                    output.clone(),
                    *exit_code,
                    *signal,
                    *duration_ms,
                    *truncated,
                    *cancelled,
                );
                live.push(LiveMessage {
                    event_id: events[i].id.clone(),
                    message: Message {
                        role: Role::User,
                        blocks: vec![ContentBlock::Text {
                            text: result.context_text(),
                        }],
                    },
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
        let pairs: Vec<(&str, &Message)> = live
            .iter()
            .map(|message| (message.event_id.as_str(), &message.message))
            .collect();
        let edited = crate::context_edit::edit_tail_refs(&pairs, &opts.edit);
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

    if live.is_empty() {
        return None;
    }

    let plan = plan_cut(&live, opts);
    if plan.summarized == 0
        || previously_summarized.saturating_add(plan.summarized) < MIN_SUMMARIZED
    {
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
    let kept_pairs: Vec<(&str, &Message)> = kept_live
        .iter()
        .map(|message| (message.event_id.as_str(), &message.message))
        .collect();
    let kept_messages: Vec<Message> = crate::context_edit::edit_tail_refs(&kept_pairs, &opts.edit);

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
        represented_count: previously_summarized.saturating_add(plan.summarized),
        kept_count,
        first_kept_event_id: plan.first_kept_event_id,
        summarized_range,
    })
}

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

    // Strategy 1: Cut at the last completed user-prompt boundary.
    // Summarize everything before it; keep the final turn whole (unless the
    // oversized-turn guard splits it). Requires at least two user prompts so
    // the summarized prefix is non-empty.
    if let Some(&last_user) = user_indices.last() {
        let mut cut = last_user;
        if has_unmatched_tool_call(live, last_user) {
            if let Some(&prev) = user_indices.iter().rev().nth(1) {
                cut = prev;
            }
        }
        if opts.max_kept_tokens > 0 {
            let suffix_tokens = estimate_tokens(&live[cut..]);
            if suffix_tokens > opts.max_kept_tokens {
                if let Some(split) = find_suffix_split(live, cut, opts.max_kept_tokens) {
                    cut = split;
                } else {
                    // No cycle boundary fits the budget — compact everything.
                    return CutPlan {
                        summarized: live.len(),
                        first_kept_event_id: None,
                    };
                }
            }
        }
        if cut > 0 {
            return CutPlan {
                summarized: cut,
                first_kept_event_id: Some(live[cut].event_id.clone()),
            };
        }
    }

    // Strategy 2: Split a single turn (or a promptless conversation) at a
    // completed tool-cycle boundary near the midpoint. This lets a long
    // agentic turn with many tool calls be partially summarized even when
    // there is only one user prompt.
    if let Some(mid) = find_mid_cycle_boundary(live) {
        if mid > 0 && mid < live.len() - 1 {
            return CutPlan {
                summarized: mid + 1,
                first_kept_event_id: Some(live[mid + 1].event_id.clone()),
            };
        }
    }

    // Strategy 3: Compact-all. No suitable boundary was found; fold the
    // entire live list into the summary.
    CutPlan {
        summarized: live.len(),
        first_kept_event_id: None,
    }
}

fn is_real_user(m: &Message) -> bool {
    if m.role != Role::User {
        return false;
    }
    !m.blocks
        .iter()
        .all(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

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


const SEPARATOR: &str = "\n\n---\n\n";

fn build_summary(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> String {
    let goal = extract_session_goal(blocks);
    let prefs = extract_preferences(blocks);
    let outstanding = extract_outstanding(blocks, hooks);

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

    let headers_text = [stable, volatile].concat().join("\n\n");
    let capped_brief = if brief.is_empty() {
        String::new()
    } else {
        cap_brief(&brief)
    };
    // Apply the summary token budget: trim the brief (most volatile) to
    // keep the structured headers + brief within MAX_SUMMARY_TOKENS.
    let trimmed_brief = trim_brief_to_budget(&headers_text, &capped_brief);
    let mut parts: Vec<String> = Vec::new();
    if !headers_text.is_empty() {
        parts.push(headers_text);
    }
    if !trimmed_brief.is_empty() {
        parts.push(trimmed_brief);
    }
    if parts.is_empty() {
        return String::new();
    }
    let body = parts.join(SEPARATOR);
    format!("{HANDOFF_PREAMBLE}\n\n{body}")
}

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
/// Prioritises the original intent (first 2 prompts) and the current
/// direction (most recent 3), dropping the middle to avoid evicting the
/// original goal on long sessions.
fn extract_session_goal(blocks: &[CompactBlock]) -> Vec<String> {
    const HEAD: usize = 2;
    const TAIL: usize = 3;
    const MAX: usize = 8;

    let prompts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| {
            let CompactBlock::User { text } = b else {
                return None;
            };
            let t = text.trim();
            (!t.is_empty()).then_some(t)
        })
        .collect();
    if prompts.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let n = prompts.len();
    let mut keep: Vec<usize> = Vec::new();
    if n <= MAX {
        (0..n).for_each(|i| keep.push(i));
    } else {
        (0..HEAD.min(n)).for_each(|i| keep.push(i));
        let tail_start = n.saturating_sub(TAIL);
        for i in tail_start..n {
            if !keep.contains(&i) {
                keep.push(i);
            }
        }
        for i in HEAD..tail_start {
            if keep.len() >= MAX {
                break;
            }
            keep.push(i);
        }
    }
    keep.sort_unstable();
    for &i in &keep {
        let clipped = clip(prompts[i], 200);
        let key = clipped.to_lowercase();
        if seen.insert(key) {
            out.push(clipped);
        }
    }
    out
}

/// User Preferences: user lines that read as a directive.
/// Signal keywords must appear near the start of a clause (first 60 chars
/// of a line/sentence) to avoid matching incidental questions like "can
/// you use the openai provider?". Questions (lines ending in '?') are
/// excluded entirely.
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
    fn has_signal_near_start(lower: &str) -> bool {
        const MAX_PREFIX: usize = 60;
        let prefix = if lower.len() > MAX_PREFIX {
            &lower[..MAX_PREFIX]
        } else {
            lower
        };
        SIGNALS.iter().any(|s| prefix.contains(s))
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in blocks {
        let CompactBlock::User { text } = b else {
            continue;
        };
        let trimmed = text.trim();
        if trimmed.ends_with('?') {
            continue;
        }
        let lower = trimmed.to_lowercase();
        if !has_signal_near_start(&lower) {
            continue;
        }
        let line = clip(trimmed, 160);
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
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| (7..=12).contains(&word.len()))
        .map(str::to_string)
}

/// The blocker regex. Compiled once and cached for the process lifetime.
fn blocker_regex() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(BLOCKER_RE).ok())
        .as_ref()
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
                    if line.len() < 15 || !blocker.is_some_and(|regex| regex.is_match(&line)) {
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

/// Soft token budget (chars/4) for the entire summary. The brief transcript
/// is trimmed first (it is the most volatile and least structured part);
/// section headers are left intact. 0 disables the budget guard.
const MAX_SUMMARY_TOKENS: usize = 4_000;

/// Build the compressed per-turn transcript: [user]/[assistant] sections
/// with clipped text and one-liner tool actions.
#[allow(clippy::too_many_lines)]
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
                    let mut i = 0;
                    while i < native.len() {
                        let rec = &native[i];
                        let args = clip(&rec.args, 80);
                        let marker = if rec.is_error { " (error)" } else { "" };
                        let mut count = 1;
                        while i + count < native.len()
                            && native[i + count].name == rec.name
                            && native[i + count].args == rec.args
                            && native[i + count].is_error == rec.is_error
                        {
                            count += 1;
                        }
                        if count > 1 {
                            lines.push(format!("* {} \"{}\"{} (x{count})", rec.name, args, marker));
                        } else {
                            lines.push(format!("* {} \"{}\"{}", rec.name, args, marker));
                        }
                        i += count;
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
/// many earlier lines were omitted. If the result still exceeds the summary
/// token budget, further trim from the front (keeping the most recent lines).
fn cap_brief(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let (kept, omitted) = if lines.len() <= BRIEF_MAX_LINES {
        (lines.as_slice(), 0)
    } else {
        let omitted = lines.len() - BRIEF_MAX_LINES;
        (&lines[lines.len() - BRIEF_MAX_LINES..], omitted)
    };
    let mut out = if omitted > 0 {
        format!("...({omitted} earlier lines omitted)\n\n")
    } else {
        String::new()
    };
    out.push_str(&kept.join("\n"));
    out
}

/// Trim the brief transcript (already line-capped by `cap_brief`) so the
/// full summary stays within the soft token budget. Removes lines from the
/// front of the brief (oldest first) until the estimated token count of the
/// *entire* summary body fits. The brief is the most volatile part and the
/// structured sections are always preserved.
fn trim_brief_to_budget(headers: &str, brief: &str) -> String {
    if MAX_SUMMARY_TOKENS == 0 {
        return brief.to_string();
    }
    let header_tokens = headers.len() / 4;
    let budget = MAX_SUMMARY_TOKENS.saturating_sub(header_tokens);
    let brief_tokens = brief.len() / 4;
    if brief_tokens <= budget {
        return brief.to_string();
    }
    // Keep the last N lines that fit within the remaining budget.
    let lines: Vec<&str> = brief.split('\n').collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut kept_chars = 0;
    let budget_chars = budget * 4;
    for line in lines.iter().rev() {
        if kept_chars + line.len() + 1 > budget_chars && !kept.is_empty() {
            break;
        }
        kept_chars += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    if omitted > 0 {
        format!(
            "...({omitted} earlier lines trimmed)\n\n{}",
            kept.join("\n")
        )
    } else {
        kept.join("\n")
    }
}

const BLOCKER_RE: &str = r"(?i)(fail(ed|s|ure|ing)?|broken|cannot|can't|won't work|does not work|doesn't work|still (broken|failing|wrong)|blocked|blocker|not (fixed|resolved|working)|crash(es|ed|ing)?)";


const SECTION_HEADERS: &[&str] = &[
    "Session Goal",
    "User Preferences",
    "Files And Changes",
    "Commits",
    "Outstanding Context",
];

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

    let headers_text = merged_headers.join("\n\n");
    let capped_brief = if brief.is_empty() {
        String::new()
    } else {
        cap_brief(&brief)
    };
    let trimmed_brief = trim_brief_to_budget(&headers_text, &capped_brief);
    let mut parts: Vec<String> = Vec::new();
    if !headers_text.is_empty() {
        parts.push(headers_text);
    }
    if !trimmed_brief.is_empty() {
        parts.push(trimmed_brief);
    }
    if parts.is_empty() {
        return String::new();
    }
    format!("{HANDOFF_PREAMBLE}\n\n{}", parts.join(SEPARATOR))
}

fn strip_preamble(text: &str) -> String {
    text.strip_prefix(HANDOFF_PREAMBLE).map_or_else(
        || text.to_string(),
        |rest| rest.trim_start_matches('\n').to_string(),
    )
}

fn split_headers_brief(body: &str) -> (String, String) {
    match body.split_once(SEPARATOR) {
        Some((h, b)) => (h.trim().to_string(), b.trim().to_string()),
        None => (body.trim().to_string(), String::new()),
    }
}

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
