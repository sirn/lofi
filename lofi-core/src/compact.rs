#![allow(clippy::doc_markdown)]

use std::collections::HashMap;
use std::sync::Arc;

use lofi_types::{
    CompactBlock, CompactionHook, ContentBlock, Message, NativeToolRecord, PromptKind, Role,
    SessionEvent, SessionEventKind,
};

use crate::session::store;

pub const HANDOFF_PREAMBLE: &str = "This summary captures work done before the most recent messages in this session. Read it to pick up context — this is work already in progress. Continue directly where you left off.";

/// Stubs in the kept tail carry only the event id, so this line is the
/// single place that tells the model how to recover them.
const RESULT_RECALL_INSTRUCTION: &str = "Cleared tool results and exec source show only an event id. Recover the original with lofi.result(eventId).";

#[derive(Debug, Clone)]
struct LiveMessage {
    event_id: String,
    message: Message,
}

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
            represented,
            ..
        } = &events[i].kind
        {
            previous_summary = Some(summary.clone());
            previously_summarized = *represented;
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
            SessionEventKind::Message(_) | SessionEventKind::UserBash { .. } if !skipping => {
                if let Some(message) =
                    crate::session::replay::agent_message_for_event(&events[i].kind)
                {
                    live.push(LiveMessage {
                        event_id: events[i].id.clone(),
                        message,
                    });
                }
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
            kind: PromptKind::default(),
        });
    }
    out.extend(compaction.kept_messages.iter().cloned());
    out
}

fn plan_cut(live: &[LiveMessage], opts: &CompactOptions) -> CutPlan {
    let user_indices: Vec<usize> = live
        .iter()
        .enumerate()
        .filter(|(_, lm)| is_real_user(&lm.message))
        .map(|(i, _)| i)
        .collect();

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
                        ..
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
                        ContentBlock::Thinking { .. }
                        | ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. } => {}
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
                        ..
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
    format_handoff(&body)
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
        // Fill from the middle, preferring earlier (closer to the original
        // goal) so the thread of intent is preserved.
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
    for rec in blocks.iter().flat_map(CompactBlock::native_records) {
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
    for rec in blocks.iter().flat_map(CompactBlock::native_records) {
        if rec.name != "bash" || !rec.args.contains("git commit") {
            continue;
        }
        let msg = extract_commit_message(&rec.args).unwrap_or_else(|| "(git commit)".to_string());
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

fn first_hash(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| (7..=12).contains(&word.len()))
        .map(str::to_string)
}

fn blocker_regex() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(BLOCKER_RE).ok())
        .as_ref()
}

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

const BRIEF_MAX_LINES: usize = 120;
const TRUNC_USER: usize = 256;
const TRUNC_ASSISTANT: usize = 200;

const MAX_SUMMARY_TOKENS: usize = 4_000;

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
    "APIs Used",
    "Skills",
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
    format_handoff(&parts.join(SEPARATOR))
}

fn format_handoff(body: &str) -> String {
    format!("{HANDOFF_PREAMBLE}\n\n{RESULT_RECALL_INSTRUCTION}\n\n{body}")
}

fn strip_preamble(text: &str) -> String {
    let rest = text
        .strip_prefix(HANDOFF_PREAMBLE)
        .map_or(text, |rest| rest.trim_start_matches('\n'));
    rest.strip_prefix(RESULT_RECALL_INSTRUCTION)
        .unwrap_or(rest)
        .trim_start_matches('\n')
        .to_string()
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
    let parse_lines = |text: &str| -> Vec<String> {
        text.split('\n')
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('['))
            .map(|l| l.strip_prefix("- ").unwrap_or(l).to_string())
            .collect()
    };
    let prev_lines = parse_lines(prev);
    let fresh_lines = parse_lines(fresh);
    let mut lines: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    if name == "Session Goal" {
        const PROTECTED: usize = 2;
        for l in prev_lines.iter().take(PROTECTED) {
            if seen.insert(l.clone()) {
                lines.push(l.clone());
            }
        }
        for l in fresh_lines.iter().chain(prev_lines.iter().skip(PROTECTED)) {
            if seen.insert(l.clone()) {
                lines.push(l.clone());
            }
            if lines.len() >= cap {
                break;
            }
        }
    } else {
        for l in prev_lines.iter().chain(fresh_lines.iter()) {
            if seen.insert(l.clone()) {
                lines.push(l.clone());
            }
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

fn split_paths(rest: &str) -> Vec<String> {
    let no_recall = rest.split("+recall:").next().unwrap_or(rest);
    no_recall
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

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

fn first_line(text: &str, max: usize) -> String {
    let line = text.split('\n').next().unwrap_or("").trim();
    clip(line, max)
}

fn compress_tool_result(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if !trimmed.contains('\n') {
        return clip(trimmed, max);
    }

    if trimmed.starts_with('{') {
        if let Some(summary) = compress_json_result(trimmed, max) {
            return summary;
        }
    }

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

fn compress_json_result(text: &str, max: usize) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let obj = v.as_object()?;
    let priority = [
        "output", "error", "stderr", "stdout", "path", "value", "result", "content", "message",
        "text",
    ];
    let mut parts: Vec<String> = Vec::new();

    for key in &priority {
        if let Some(val) = obj.get(*key) {
            let s = json_value_brief(val, 60);
            if !s.is_empty() {
                parts.push(format!("{key}={s}"));
            }
        }
    }

    for key in &["ok", "code", "status", "signal", "is_error", "duration_ms"] {
        if let Some(val) = obj.get(*key) {
            let s = json_value_brief(val, 30);
            if !s.is_empty() {
                parts.push(format!("{key}={s}"));
            }
        }
    }

    if parts.is_empty() {
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

fn extract_full_output_path(text: &str) -> Option<&str> {
    let rest = text.split("Full output: ").nth(1)?;
    let path = rest.split(". Use lofi.read").next()?.trim();
    if path.starts_with('/') {
        Some(path)
    } else {
        None
    }
}

fn non_empty_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

fn estimate_message_tokens(m: &Message) -> usize {
    let chars: usize = m
        .blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
            ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
            // Image tokens scale with pixel dimensions, not byte length, and
            // we don't retain dimensions on the block. Use a fixed per-image
            // estimate (in chars so the outer `/4` yields ~1000 tokens, the
            // ballpark of a full-frame image at Anthropic's ~750px/token).
            ContentBlock::Image { .. } => 4000,
        })
        .sum();
    chars / 4
}

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
            kind: PromptKind::default(),
        }
    }
    fn assistant(t: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: t.into() }],
            kind: PromptKind::default(),
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
            kind: PromptKind::default(),
        }
    }
    fn exec_result(id: &str, value: &str) -> Message {
        Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: serde_json::json!({ "value": value, "logs": [] }).to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
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
    fn previous_compaction_restores_summarized_message_count() {
        let mut events = events_of(&[
            user("old prompt"),
            assistant("old response"),
            assistant("old follow-up"),
            assistant("old result"),
            assistant("old conclusion"),
        ]);
        events.push(SessionEvent {
            id: "marker".to_string(),
            parent_id: Some("e4".to_string()),
            kind: SessionEventKind::Compaction {
                summary: "previous summary".to_string(),
                first_kept_entry_id: String::new(),
                summarized_range: ["e0".to_string(), "e4".to_string()],
                checkpointed_tail: true,
                summarized: 5,
                represented: 5,
                kept: 0,
            },
        });
        events.push(SessionEvent {
            id: "e6".to_string(),
            parent_id: Some("marker".to_string()),
            kind: SessionEventKind::Message(assistant("continued work")),
        });
        events.push(SessionEvent {
            id: "e7".to_string(),
            parent_id: Some("e6".to_string()),
            kind: SessionEventKind::Message(assistant("continued result")),
        });

        let compacted = compact(&events, &CompactOptions::default())
            .expect("restored summarized count must satisfy the history guard");
        assert_eq!(compacted.summarized_count, 2);
        assert_eq!(compacted.represented_count, 7);
        assert_eq!(compacted.kept_count, 0);
    }

    #[test]
    fn compact_refuses_when_too_little_to_fold() {
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
        assert!(c.summary.contains(RESULT_RECALL_INSTRUCTION));
        assert!(c.summary.contains("[Session Goal]"));
        assert!(c.summary.contains("login form"));
        assert_eq!(
            c.kept_messages.first().map(user_text).as_deref(),
            Some("Now add tests please")
        );
    }

    #[test]
    fn compact_drops_image_blocks() {
        // The byte-pressure recovery depends on this: an oversized image
        // payload stops before send, the UI force-compacts, and the continued
        // request must be small. That only holds because compaction drops
        // `Image` blocks from the summarized prefix. If compaction ever
        // starts keeping images, the recovery loops forever — this test pins
        // the contract.
        let with_image = Message {
            role: Role::User,
            kind: PromptKind::default(),
            blocks: vec![
                ContentBlock::Text {
                    text: "look at this".into(),
                },
                ContentBlock::Image {
                    bytes: vec![0u8; 16],
                    media_type: "image/jpeg".into(),
                },
            ],
        };
        let msgs = [
            with_image,
            assistant("I see it."),
            exec_call("t1", "return 1"),
            exec_result("t1", "1"),
            assistant("Done."),
            user("next"),
            assistant("ok"),
        ];
        let events = events_of(&msgs);
        let c = compact(&events, &CompactOptions::default()).expect("some compaction");
        let history = compacted_history(&c);
        assert!(
            !history
                .iter()
                .flat_map(|m| m.blocks.iter())
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "compacted history must not carry Image blocks"
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
        assert_eq!(merged.matches(RESULT_RECALL_INSTRUCTION).count(), 1);
    }

    #[test]
    fn merge_previous_keeps_apis_and_skills_before_outstanding() {
        let prev = format!(
            "{HANDOFF_PREAMBLE}\n\n[APIs Used]\n- exec\n\n[Skills]\n- code-commit — Write a commit message\n\n[Outstanding Context]\n- old blocker"
        );
        let fresh = format!(
            "{HANDOFF_PREAMBLE}\n\n[APIs Used]\n- exec\n- lofi.read\n\n[Skills]\n- code-iterate — Iterate until clean\n\n[Outstanding Context]\n- new blocker"
        );
        let merged = merge_previous(&prev, &fresh);
        let apis = merged.find("[APIs Used]").unwrap();
        let skills = merged.find("[Skills]").unwrap();
        let outstanding = merged.find("[Outstanding Context]").unwrap();
        assert!(apis < skills && skills < outstanding);
        assert!(merged.contains("lofi.read"));
        assert!(merged.contains("code-commit"));
        assert!(merged.contains("code-iterate"));
        assert!(merged.contains("new blocker"));
        assert!(!merged.contains("old blocker"));
    }

    #[test]
    fn merge_previous_preserves_original_goals() {
        let prev_goals: Vec<String> = (0..8).map(|i| format!("- original goal {i}")).collect();
        let fresh_goals: Vec<String> = (0..5).map(|i| format!("- fresh goal {i}")).collect();
        let prev = format!(
            "{HANDOFF_PREAMBLE}\n\n[Session Goal]\n{}\n\n---\n\n[user]\nold",
            prev_goals.join("\n")
        );
        let fresh = format!(
            "{HANDOFF_PREAMBLE}\n\n[Session Goal]\n{}\n\n---\n\n[user]\nnew",
            fresh_goals.join("\n")
        );
        let merged = merge_previous(&prev, &fresh);
        assert!(merged.contains("original goal 0"));
        assert!(merged.contains("original goal 1"));
        assert!(merged.contains("fresh goal 4"));
        let goal_count = merged
            .split("[Session Goal]")
            .nth(1)
            .unwrap_or("")
            .lines()
            .filter(|l| l.starts_with("- "))
            .count();
        assert!(goal_count <= 8, "goal_count={goal_count} should be <= 8");
    }

    #[test]
    fn extract_preferences_skips_questions() {
        let blocks = vec![
            CompactBlock::User {
                text: "Can you use the openai provider?".to_string(),
            },
            CompactBlock::User {
                text: "Please always use 2-space indentation".to_string(),
            },
        ];
        let prefs = extract_preferences(&blocks);
        assert!(prefs.iter().all(|p| !p.contains("openai provider")));
        assert!(prefs.iter().any(|p| p.contains("2-space indentation")));
    }

    #[test]
    fn brief_collapses_consecutive_identical_tool_calls() {
        let blocks = vec![CompactBlock::ToolCall {
            id: "t1".to_string(),
            code: String::new(),
            label: None,
            native: vec![
                native("t1", "read", "foo.rs"),
                native("t1", "read", "foo.rs"),
                native("t1", "read", "foo.rs"),
            ],
        }];
        let brief = build_brief(&blocks, &[]);
        assert!(
            brief.contains("(x3)"),
            "brief should contain repeat count: {brief}"
        );
    }

    #[test]
    fn summary_stays_within_token_budget() {
        // Build a conversation with many turns so the brief transcript
        // would be large without the token budget.
        let big = "x".repeat(2000);
        let mut msgs = Vec::new();
        msgs.push(user("do a big task"));
        for i in 0..40 {
            msgs.push(exec_call(&format!("t{i}"), "lofi.read"));
            msgs.push(exec_result(&format!("t{i}"), &big));
            msgs.push(assistant(&"step done with lots of text ".repeat(5)));
        }
        msgs.push(user("now summarize"));
        msgs.push(assistant("done"));
        let events = events_of(&msgs);
        let c = compact(&events, &CompactOptions::default()).expect("should compact");
        let summary_chars = c.summary.len();
        assert!(
            summary_chars < 20_000,
            "summary is {summary_chars} chars, should be under ~16k"
        );
    }

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
                represented: 2,
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
        let big_result = "x".repeat(10_000);
        let mut events = events_of(&[
            user("do task"),
            exec_call("t1", "lofi.read"),
            exec_result("t1", &big_result),
            assistant("done"),
            user("now continue"),
            exec_call("t2", "lofi.read"),
            exec_result("t2", &big_result),
            assistant("ok"),
        ]);
        events.push(SessionEvent {
            id: "c1".to_string(),
            parent_id: Some("e7".to_string()),
            kind: SessionEventKind::Compaction {
                summary: format!("{HANDOFF_PREAMBLE}\n\n[prior summary]"),
                first_kept_entry_id: "e4".to_string(),
                summarized_range: ["e0".to_string(), "e3".to_string()],
                checkpointed_tail: false,
                summarized: 4,
                represented: 4,
                kept: 4,
            },
        });
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

        assert!(c.summary.contains("[prior summary]"));
        assert!(
            !c.summary.contains(&"x".repeat(100)),
            "summary should not contain the big result from the old kept tail"
        );

        assert!(
            c.summary.contains("cleared") || !c.summary.contains(&big_result),
            "old kept tail results should be stubbed, not carried verbatim into summary"
        );
    }

    #[test]
    fn compact_single_prompt_oversized_turn() {
        // One user prompt followed by 6 exec tool cycles (12 messages).
        // Each exec result is 20k chars → ~30k tokens total, well over the
        // 2k-token budget. The oversized-turn guard must split at a
        // completed tool-cycle so the leading messages are summarized.
        let big = "x".repeat(20_000);
        let msgs = [
            user("do a big task"),
            exec_call("t1", "lofi.read big1"),
            exec_result("t1", &big),
            exec_call("t2", "lofi.read big2"),
            exec_result("t2", &big),
            exec_call("t3", "lofi.read big3"),
            exec_result("t3", &big),
            exec_call("t4", "lofi.read big4"),
            exec_result("t4", &big),
            exec_call("t5", "lofi.read big5"),
            exec_result("t5", &big),
            exec_call("t6", "lofi.read big6"),
            exec_result("t6", "ok"),
        ];
        let events = events_of(&msgs);
        let opts = CompactOptions {
            max_kept_tokens: 2_000,
            ..Default::default()
        };
        let c = compact(&events, &opts).expect("should compact single oversized turn");
        assert!(
            c.summarized_count >= MIN_SUMMARIZED,
            "summarized {} should be >= {MIN_SUMMARIZED}",
            c.summarized_count
        );
        assert!(c.summary.contains("do a big task"));
        assert!(!c.kept_messages.is_empty(), "should keep a tail");
    }
}
