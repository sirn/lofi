use std::collections::HashMap;

use lofi_types::{ContentBlock, Message, NativeToolRecord, Role, SessionEvent, SessionEventKind};

use super::{user_text, CompactOptions, HANDOFF_PREAMBLE};
use crate::session::store;

#[derive(Debug, Clone)]
pub(super) struct LiveMessage {
    pub(super) event_id: String,
    pub(super) message: Message,
}

#[derive(Debug, Clone)]
pub(super) struct CutPlan {
    pub(super) summarized: usize,
    pub(super) first_kept_event_id: Option<String>,
}

pub(super) struct CompactionPlan {
    pub(super) live: Vec<LiveMessage>,
    pub(super) cut: CutPlan,
    pub(super) previous_summary: Option<String>,
    pub(super) previously_summarized: usize,
    pub(super) native_by_parent: HashMap<String, Vec<NativeToolRecord>>,
}

pub(super) const MIN_SUMMARIZED: usize = 5;

#[allow(clippy::too_many_lines)]
pub(super) fn prepare(events: &[SessionEvent], opts: &CompactOptions) -> Option<CompactionPlan> {
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

    let mut native_by_parent: HashMap<String, Vec<NativeToolRecord>> = HashMap::new();
    for &index in &path[live_start..] {
        if let SessionEventKind::NativeTool(record) = &events[index].kind {
            native_by_parent
                .entry(record.parent.clone())
                .or_default()
                .push(record.clone());
        }
    }

    let mut filter = crate::session::replay::LeafFirstContextFilter::default();
    let mut live: Vec<LiveMessage> = path[live_start..]
        .iter()
        .rev()
        .filter_map(|&index| {
            filter.push(&events[index].kind).map(|message| LiveMessage {
                event_id: events[index].id.clone(),
                message,
            })
        })
        .collect();
    live.reverse();

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

    Some(CompactionPlan {
        live,
        cut: plan,
        previous_summary,
        previously_summarized,
        native_by_parent,
    })
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

fn estimate_message_tokens(m: &Message) -> usize {
    let chars: usize = m
        .blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
            ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
            ContentBlock::PartSignature { .. } => 0,
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
