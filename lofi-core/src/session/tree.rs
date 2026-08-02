//! Session tree projection over the event index. Owns all index-walking
//! (active path, hidden checkpoints, branch detection) and event-derived row
//! labels so the TUI only maps rows onto widgets.

// The graph maps here are built internally with the std hasher; generalizing
// the hasher on every walker buys nothing for an internal projection.
#![allow(clippy::implicit_hasher)]

use std::collections::HashMap;

use lofi_types::{ContentBlock, Role, SessionEventKind};

use super::store::{EventIndex, IndexId, IndexKind, SessionCursor};

/// A display-agnostic tree row. The TUI maps these onto its widgets; all
/// session reasoning (which events, what labels, which branches) lives here.
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub branch_point: String,
    pub label: String,
    pub prefix: String,
    pub prefill: String,
    pub is_active: bool,
    pub source_index: usize,
    pub source_offset: u64,
    pub source_kind: IndexKind,
    pub hydrated: bool,
}

struct TreeCtx<'a> {
    indices: &'a [EventIndex],
    children_by_parent: &'a HashMap<&'a IndexId, Vec<usize>>,
    by_id: &'a HashMap<&'a IndexId, usize>,
    cursor: &'a SessionCursor,
    native_tools: std::cell::RefCell<HashMap<String, Vec<(String, String)>>>,
    lazy_native_tools: bool,
    hydrate: bool,
    retain_prefill: bool,
    hydrate_only: Option<&'a std::collections::HashSet<usize>>,
}

#[must_use]
pub fn build_tree_rows(
    indices: &[EventIndex],
    leaf_id: Option<&str>,
    cursor: &SessionCursor,
) -> Vec<TreeRow> {
    build_tree_rows_inner(indices, leaf_id, cursor, true, true, None)
}

#[must_use]
pub fn build_tree_row_skeletons(
    indices: &[EventIndex],
    leaf_id: Option<&str>,
    cursor: &SessionCursor,
) -> Vec<TreeRow> {
    build_tree_rows_inner(indices, leaf_id, cursor, false, false, None)
}

pub fn hydrate_tree_rows(
    indices: &[EventIndex],
    cursor: &SessionCursor,
    requested: &[(usize, TreeRow)],
    mut emit: impl FnMut(Vec<(usize, TreeRow)>) -> bool,
    cancelled: impl Fn() -> bool,
) {
    if requested.is_empty() || cancelled() {
        return;
    }
    let mut children_by_parent: HashMap<&IndexId, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<&IndexId, usize> = HashMap::new();
    for (index, entry) in indices.iter().enumerate() {
        // Cursor records reuse `id` for the selected leaf; including them
        // shadows the real leaf event (appended last) and truncates the path.
        if entry.kind != IndexKind::Cursor && !entry.id.is_empty() {
            by_id.insert(&entry.id, index);
        }
        if let Some(parent) = entry.parent_id.as_ref().filter(|parent| !parent.is_empty()) {
            children_by_parent.entry(parent).or_default().push(index);
        }
    }
    let ctx = TreeCtx {
        indices,
        children_by_parent: &children_by_parent,
        by_id: &by_id,
        cursor,
        native_tools: std::cell::RefCell::new(HashMap::new()),
        lazy_native_tools: true,
        hydrate: true,
        retain_prefill: false,
        hydrate_only: None,
    };
    let mut rows = Vec::with_capacity(requested.len());
    for (display_index, skeleton) in requested {
        if cancelled() {
            return;
        }
        let mut entry = Vec::with_capacity(1);
        push_tree_row(
            &ctx,
            skeleton.source_index,
            &skeleton.prefix,
            skeleton.is_active,
            &mut entry,
        );
        if let Some(entry) = entry.pop() {
            rows.push((*display_index, entry));
        }
    }
    if !rows.is_empty() {
        let _ = emit(rows);
    }
}

pub fn hydrate_tree_window(
    indices: &[EventIndex],
    cursor: &SessionCursor,
    skeletons: &[TreeRow],
    display_range: std::ops::Range<usize>,
    emit: impl FnMut(Vec<(usize, TreeRow)>) -> bool,
    cancelled: impl Fn() -> bool,
) {
    let requested: Vec<_> = display_range
        .filter_map(|index| {
            skeletons
                .get(index)
                .filter(|entry| !entry.hydrated)
                .cloned()
                .map(|entry| (index, entry))
        })
        .collect();
    hydrate_tree_rows(indices, cursor, &requested, emit, cancelled);
}

fn hidden_checkpoint_indices(
    indices: &[EventIndex],
    active_path: &[usize],
    cursor: &SessionCursor,
) -> std::collections::HashSet<usize> {
    let mut hidden = std::collections::HashSet::new();
    for (marker_pos, &idx) in active_path.iter().enumerate() {
        if indices[idx].kind != IndexKind::Compaction {
            continue;
        }
        let (_, _, checkpointed, first_kept) = load_compaction_details(cursor, indices[idx].offset);
        if checkpointed && !first_kept.is_empty() {
            if let Some(start) = active_path[..marker_pos]
                .iter()
                .position(|&i| indices[i].id.matches(&first_kept))
            {
                hidden.extend(active_path[start..marker_pos].iter().copied());
            }
        }
    }
    hidden
}

fn top_level_tree_nodes(indices: &[EventIndex], by_id: &HashMap<&IndexId, usize>) -> Vec<usize> {
    indices
        .iter()
        .enumerate()
        .filter(|&(_, entry)| {
            if !is_tree_node(entry.kind) {
                return false;
            }
            let mut parent = entry.parent_id.as_ref();
            while let Some(id) = parent {
                let Some(&index) = by_id.get(id) else {
                    break;
                };
                if is_tree_node(indices[index].kind) {
                    return false;
                }
                parent = indices[index].parent_id.as_ref();
            }
            true
        })
        .map(|(index, _)| index)
        .collect()
}

#[allow(clippy::too_many_lines)]
fn build_tree_rows_inner(
    indices: &[EventIndex],
    leaf_id: Option<&str>,
    cursor: &SessionCursor,
    hydrate: bool,
    retain_prefill: bool,
    hydrate_only: Option<&std::collections::HashSet<usize>>,
) -> Vec<TreeRow> {
    let native_tools = if hydrate && hydrate_only.is_none() {
        build_native_tool_map(indices, cursor)
    } else {
        HashMap::new()
    };
    let mut children_by_parent: HashMap<&IndexId, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<&IndexId, usize> = HashMap::new();
    for (i, ix) in indices.iter().enumerate() {
        // Cursor records reuse `id` to carry the selected leaf, so including
        // them would shadow the real leaf event (they are appended last) and
        // truncate the active path to the record, which has no parent.
        if ix.kind != IndexKind::Cursor && !ix.id.is_empty() {
            by_id.insert(&ix.id, i);
        }
        if let Some(p) = ix.parent_id.as_ref() {
            if !p.is_empty() {
                children_by_parent.entry(p).or_default().push(i);
            }
        }
    }
    let active_path: Vec<usize> = leaf_id
        .filter(|id| !id.is_empty())
        .map_or_else(Vec::new, |id| active_path_from_index(indices, &by_id, id));
    let active_set: std::collections::HashSet<usize> = active_path.iter().copied().collect();
    let hidden_checkpoint = hidden_checkpoint_indices(indices, &active_path, cursor);
    let trunk: Vec<usize> = active_path
        .iter()
        .copied()
        .filter(|i| !hidden_checkpoint.contains(i))
        .filter(|&i| is_tree_node(indices[i].kind))
        .collect();

    let n = trunk.len();
    let mut out = Vec::new();
    let ctx = TreeCtx {
        indices,
        children_by_parent: &children_by_parent,
        by_id: &by_id,
        cursor,
        native_tools: std::cell::RefCell::new(native_tools),
        lazy_native_tools: false,
        hydrate,
        retain_prefill,
        hydrate_only,
    };
    for (pos, &idx) in trunk.iter().enumerate() {
        let is_last = pos == n - 1;
        let connector = if is_last {
            "\u{2514}\u{2500} "
        } else {
            "\u{251c}\u{2500} "
        };
        let child_indent = if is_last { "   " } else { "\u{2502}  " };
        push_tree_row(&ctx, idx, connector, active_set.contains(&idx), &mut out);
        let mut branches: Vec<usize> = Vec::new();
        if indices[idx].kind == IndexKind::UserPrompt {
            if let Some(te_idx) = find_turn_outcome(idx, indices, &children_by_parent) {
                if !active_set.contains(&te_idx) {
                    branches.push(te_idx);
                }
            }
        }
        let user_branches: Vec<usize> = children_by_parent
            .get(&indices[idx].id)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| indices[i].kind == IndexKind::UserPrompt && !active_set.contains(&i))
            .collect();
        branches.extend(user_branches);
        if !branches.is_empty() {
            render_branch_subtree(&ctx, &branches, child_indent, &mut out);
        }
    }
    if trunk.is_empty() && out.is_empty() {
        let roots = top_level_tree_nodes(indices, &by_id);
        render_branch_subtree(&ctx, &roots, "", &mut out);
    }
    out
}

fn build_native_tool_map(
    indices: &[EventIndex],
    cursor: &SessionCursor,
) -> HashMap<String, Vec<(String, String)>> {
    let offsets: Vec<u64> = indices
        .iter()
        .filter(|entry| entry.kind == IndexKind::NativeTool)
        .map(|entry| entry.offset)
        .collect();
    let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    let Ok(summaries) = cursor.native_tool_summaries(&offsets) else {
        return map;
    };
    for (parent, name, args) in summaries {
        map.entry(parent).or_default().push((name, one_line(&args)));
    }
    map
}

fn load_native_tools_for_turn(
    ctx: &TreeCtx<'_>,
    tool_result_index: usize,
    tool_use_id: &str,
) -> Vec<(String, String)> {
    let mut offsets = Vec::new();
    let mut current = tool_result_index;
    let mut visited = std::collections::HashSet::new();
    while visited.insert(current) {
        let Some(children) = ctx.children_by_parent.get(&ctx.indices[current].id) else {
            break;
        };
        let Some(&next) = children
            .iter()
            .find(|&&child| ctx.indices[child].kind != IndexKind::UserPrompt)
        else {
            break;
        };
        match ctx.indices[next].kind {
            IndexKind::NativeTool => offsets.push(ctx.indices[next].offset),
            IndexKind::TurnEnd | IndexKind::TurnFailed | IndexKind::TurnCancelled => break,
            _ => {}
        }
        current = next;
    }
    let Ok(summaries) = ctx.cursor.native_tool_summaries(&offsets) else {
        return Vec::new();
    };
    summaries
        .into_iter()
        .filter(|(parent, _, _)| parent == tool_use_id)
        .map(|(_, name, args)| (name, one_line(&args)))
        .collect()
}

/// Active path (root-first indices) from a leaf id, using the lightweight
/// index instead of fully-loaded events.
#[must_use]
pub fn active_path_from_index(
    indices: &[EventIndex],
    by_id: &HashMap<&IndexId, usize>,
    leaf_id: &str,
) -> Vec<usize> {
    super::store::lineage_path(indices, by_id, leaf_id)
}

fn render_branch_subtree(ctx: &TreeCtx, roots: &[usize], prefix: &str, out: &mut Vec<TreeRow>) {
    let indices = ctx.indices;
    let children_by_parent = ctx.children_by_parent;
    let n = roots.len();
    for (pos, &root) in roots.iter().enumerate() {
        let is_last = pos == n - 1;
        let connector = if is_last {
            "\u{2514}\u{2500} "
        } else {
            "\u{251c}\u{2500} "
        };
        let child_indent = format!("{prefix}{}", if is_last { "   " } else { "\u{2502}  " });
        let chain = walk_chain(root, indices, children_by_parent);
        let chain_set: std::collections::HashSet<usize> = chain.iter().copied().collect();
        let cn = chain.len();
        for (cpos, &idx) in chain.iter().enumerate() {
            let cprefix = if cpos == 0 {
                format!("{prefix}{connector}")
            } else {
                let cis_last = cpos == cn - 1;
                format!(
                    "{child_indent}{}",
                    if cis_last {
                        "\u{2514}\u{2500} "
                    } else {
                        "\u{251c}\u{2500} "
                    }
                )
            };
            let sub_indent = format!(
                "{child_indent}{}",
                if cpos == cn - 1 { "   " } else { "\u{2502}  " }
            );
            push_tree_row(ctx, idx, &cprefix, false, out);
            let mut sub_branches: Vec<usize> = Vec::new();
            if indices[idx].kind == IndexKind::UserPrompt {
                if let Some(te_idx) = find_turn_outcome(idx, indices, children_by_parent) {
                    if !chain_set.contains(&te_idx) {
                        sub_branches.push(te_idx);
                    }
                }
            }
            let user_children: Vec<usize> = children_by_parent
                .get(&indices[idx].id)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| indices[i].kind == IndexKind::UserPrompt && !chain_set.contains(&i))
                .collect();
            sub_branches.extend(user_children);
            if !sub_branches.is_empty() {
                render_branch_subtree(ctx, &sub_branches, &sub_indent, out);
            }
        }
    }
}

#[must_use]
pub fn find_next_user_prompt(
    start: usize,
    indices: &[EventIndex],
    children_by_parent: &HashMap<&IndexId, Vec<usize>>,
) -> Option<usize> {
    let children = children_by_parent.get(&indices[start].id)?;
    if let Some(&u) = children
        .iter()
        .find(|&&i| indices[i].kind == IndexKind::UserPrompt)
    {
        return Some(u);
    }
    let comp = children
        .iter()
        .copied()
        .find(|&i| indices[i].kind == IndexKind::Compaction)?;
    let comp_children = children_by_parent.get(&indices[comp].id)?;
    comp_children
        .iter()
        .copied()
        .find(|&i| indices[i].kind == IndexKind::UserPrompt)
}

#[must_use]
pub fn walk_chain(
    start: usize,
    indices: &[EventIndex],
    children_by_parent: &HashMap<&IndexId, Vec<usize>>,
) -> Vec<usize> {
    let mut chain = vec![start];
    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut cur = start;
    loop {
        let next = if indices[cur].kind == IndexKind::UserPrompt {
            find_turn_outcome(cur, indices, children_by_parent)
        } else {
            find_next_user_prompt(cur, indices, children_by_parent)
        };
        match next {
            Some(n) if visited.insert(n) => {
                chain.push(n);
                cur = n;
            }
            _ => break,
        }
    }
    chain
}

type TreeRowFields = (String, String, String);

/// Branch point for reverting to a turn. A completed turn (`TurnEnd`) keeps
/// itself as the head — reverting to it preserves the whole turn. An aborted
/// outcome (`TurnFailed`/`TurnCancelled`) has no completed assistant message,
/// so reverting to that row must land on its parent instead; otherwise the
/// broken/cancelled tail is reselected as the leaf and the revert is a no-op.
fn turn_outcome_branch_point(ix: &EventIndex) -> String {
    match ix.kind {
        IndexKind::TurnFailed | IndexKind::TurnCancelled => ix
            .parent_id
            .as_ref()
            .map(IndexId::to_event_id)
            .unwrap_or_default(),
        _ => ix.id.to_event_id(),
    }
}

fn skeleton_tree_row(ix: &EventIndex) -> Option<TreeRowFields> {
    let label = match ix.kind {
        IndexKind::UserPrompt => "user: loading\u{2026}",
        IndexKind::ToolResult => "tool: loading\u{2026}",
        IndexKind::TurnEnd => "agent: loading\u{2026}",
        IndexKind::TurnFailed => "agent: loading\u{2026} (failed)",
        IndexKind::TurnCancelled => "agent: loading\u{2026} (cancelled)",
        IndexKind::Compaction => "compact: loading\u{2026}",
        _ => return None,
    }
    .to_string();
    let branch_point = match ix.kind {
        IndexKind::UserPrompt | IndexKind::Compaction => ix
            .parent_id
            .as_ref()
            .map(IndexId::to_event_id)
            .unwrap_or_default(),
        IndexKind::TurnFailed | IndexKind::TurnCancelled => turn_outcome_branch_point(ix),
        _ => ix.id.to_event_id(),
    };
    Some((label, String::new(), branch_point))
}

fn tool_result_tree_row(ctx: &TreeCtx, idx: usize) -> TreeRowFields {
    let ix = &ctx.indices[idx];
    let (name, tool_use_id, content, is_error) =
        load_tool_result(idx, ctx.indices, ctx.by_id, ctx.cursor);
    let marker = if is_error { "\u{2717} " } else { "" };
    let label = if name == "exec" {
        let mut tools = ctx.native_tools.borrow().get(&tool_use_id).cloned();
        if tools.is_none() && ctx.lazy_native_tools {
            let loaded = load_native_tools_for_turn(ctx, idx, &tool_use_id);
            ctx.native_tools
                .borrow_mut()
                .insert(tool_use_id.clone(), loaded.clone());
            tools = Some(loaded);
        }
        if let Some(tools) = tools.filter(|tools| !tools.is_empty()) {
            let summary = tools
                .iter()
                .map(|(name, args)| format!("{name} {args}"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("exec: {marker}{}", one_line(&summary))
        } else {
            format!("exec: {marker}{}", one_line(&content))
        }
    } else {
        format!("tool: {marker}{name}: {}", one_line(&content))
    };
    (label, String::new(), ix.id.to_event_id())
}

fn compaction_tree_row(ctx: &TreeCtx, ix: &EventIndex) -> TreeRowFields {
    let (summarized, kept, checkpointed, first_kept) =
        load_compaction_details(ctx.cursor, ix.offset);
    let branch_point = if checkpointed && !first_kept.is_empty() {
        ctx.by_id
            .get(&IndexId::parse(first_kept))
            .and_then(|&i| ctx.indices[i].parent_id.as_ref().map(IndexId::to_event_id))
            .unwrap_or_default()
    } else {
        ix.parent_id
            .as_ref()
            .map(IndexId::to_event_id)
            .unwrap_or_default()
    };
    (
        format!("compact: Compacted {summarized} messages \u{00b7} kept {kept}"),
        String::new(),
        branch_point,
    )
}

fn hydrated_tree_row(ctx: &TreeCtx, idx: usize) -> Option<TreeRowFields> {
    let ix = &ctx.indices[idx];
    match ix.kind {
        IndexKind::UserPrompt => {
            let prompt = load_prompt_text(ctx.cursor, ix.offset);
            let prefill = if ctx.retain_prefill {
                prompt.clone()
            } else {
                String::new()
            };
            Some((
                format!("user: {}", one_line(&prompt)),
                prefill,
                ix.parent_id
                    .as_ref()
                    .map(IndexId::to_event_id)
                    .unwrap_or_default(),
            ))
        }
        IndexKind::ToolResult => Some(tool_result_tree_row(ctx, idx)),
        IndexKind::TurnEnd => {
            let preview = load_assistant_preview(idx, ctx.indices, ctx.by_id, ctx.cursor);
            let preview = if preview.is_empty() {
                "(turn end)".to_string()
            } else {
                preview
            };
            Some((
                format!("agent: {preview}"),
                String::new(),
                ix.id.to_event_id(),
            ))
        }
        IndexKind::TurnFailed => Some((
            format!(
                "agent: {} (failed)",
                one_line(&load_failed_error(ctx.cursor, ix.offset))
            ),
            String::new(),
            turn_outcome_branch_point(ix),
        )),
        IndexKind::TurnCancelled => {
            let preview = load_assistant_preview(idx, ctx.indices, ctx.by_id, ctx.cursor);
            let preview = if preview.is_empty() {
                "(cancelled)".to_string()
            } else {
                format!("{preview} (cancelled)")
            };
            Some((
                format!("agent: {preview}"),
                String::new(),
                turn_outcome_branch_point(ix),
            ))
        }
        IndexKind::Compaction => Some(compaction_tree_row(ctx, ix)),
        _ => None,
    }
}

fn push_tree_row(ctx: &TreeCtx, idx: usize, prefix: &str, is_active: bool, out: &mut Vec<TreeRow>) {
    if ctx
        .hydrate_only
        .is_some_and(|sources| !sources.contains(&idx))
    {
        return;
    }
    let ix = &ctx.indices[idx];
    let fields = if ctx.hydrate {
        hydrated_tree_row(ctx, idx)
    } else {
        skeleton_tree_row(ix)
    };
    let Some((label, prefill, branch_point)) = fields else {
        return;
    };
    out.push(TreeRow {
        prefix: prefix.to_string(),
        label,
        prefill,
        branch_point,
        is_active,
        source_index: idx,
        source_offset: ix.offset,
        source_kind: ix.kind,
        hydrated: ctx.hydrate,
    });
}

#[must_use]
pub fn is_tree_node(kind: IndexKind) -> bool {
    matches!(
        kind,
        IndexKind::UserPrompt
            | IndexKind::ToolResult
            | IndexKind::TurnEnd
            | IndexKind::TurnFailed
            | IndexKind::TurnCancelled
            | IndexKind::Compaction
    )
}

#[must_use]
pub fn find_turn_outcome(
    start: usize,
    indices: &[EventIndex],
    children_by_parent: &HashMap<&IndexId, Vec<usize>>,
) -> Option<usize> {
    let mut cur = start;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(cur) {
            return None;
        }
        match indices[cur].kind {
            IndexKind::TurnEnd | IndexKind::TurnFailed | IndexKind::TurnCancelled => {
                return Some(cur);
            }
            _ => {}
        }
        let children = children_by_parent.get(&indices[cur].id)?;
        cur = *children
            .iter()
            .find(|&&i| indices[i].kind != IndexKind::UserPrompt)?;
    }
}

#[must_use]
pub fn load_assistant_preview(
    turn_end_idx: usize,
    indices: &[EventIndex],
    by_id: &HashMap<&IndexId, usize>,
    cursor: &SessionCursor,
) -> String {
    let mut cur = turn_end_idx;
    let mut visited = std::collections::HashSet::new();
    while let Some(parent_id) = indices[cur].parent_id.as_ref() {
        if !visited.insert(cur) {
            break;
        }
        let Some(&pidx) = by_id.get(parent_id) else {
            break;
        };
        let pentry = &indices[pidx];
        if pentry.kind == IndexKind::UserPrompt {
            break;
        }
        if pentry.kind == IndexKind::AssistantMessage {
            if let Some(text) = load_assistant_text(cursor, pentry.offset) {
                return one_line(&text);
            }
        }
        cur = pidx;
    }
    String::new()
}

#[must_use]
pub fn load_prompt_text(cursor: &SessionCursor, offset: u64) -> String {
    let Ok(ev) = cursor.event_at(offset) else {
        return String::new();
    };
    if let SessionEventKind::Message(m) = ev.kind {
        if m.role == Role::User {
            return m
                .blocks
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
        }
    }
    String::new()
}

#[must_use]
pub fn load_tool_result(
    idx: usize,
    indices: &[EventIndex],
    by_id: &HashMap<&IndexId, usize>,
    cursor: &SessionCursor,
) -> (String, String, String, bool) {
    let ix = &indices[idx];
    let Ok(ev) = cursor.event_at(ix.offset) else {
        return (String::new(), String::new(), String::new(), false);
    };
    let SessionEventKind::Message(m) = ev.kind else {
        return (String::new(), String::new(), String::new(), false);
    };
    let Some(block) = m.blocks.iter().find_map(|b| match b {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => Some((tool_use_id.clone(), content.clone(), *is_error)),
        _ => None,
    }) else {
        return (String::new(), String::new(), String::new(), false);
    };
    let (tool_use_id, content, is_error) = block;
    let name = ix
        .parent_id
        .as_ref()
        .and_then(|pid| by_id.get(pid).copied())
        .and_then(|pidx| {
            let pentry = &indices[pidx];
            let pev = cursor.event_at(pentry.offset).ok()?;
            let SessionEventKind::Message(pm) = pev.kind else {
                return None;
            };
            pm.blocks.iter().find_map(|b| match b {
                ContentBlock::ToolUse { id, name, .. } if id == &tool_use_id => Some(name.clone()),
                _ => None,
            })
        })
        .unwrap_or_else(|| "?".to_string());
    (name, tool_use_id, content, is_error)
}

#[must_use]
pub fn load_assistant_text(cursor: &SessionCursor, offset: u64) -> Option<String> {
    let ev = cursor.event_at(offset).ok()?;
    let SessionEventKind::Message(m) = ev.kind else {
        return None;
    };
    if m.role != Role::Assistant {
        return None;
    }
    m.blocks.iter().find_map(|b| match b {
        ContentBlock::Text { text } => Some(text.clone()),
        _ => None,
    })
}

#[must_use]
pub fn load_failed_error(cursor: &SessionCursor, offset: u64) -> String {
    let Ok(ev) = cursor.event_at(offset) else {
        return String::new();
    };
    if let SessionEventKind::TurnFailed { error, .. } = ev.kind {
        error
    } else {
        String::new()
    }
}

fn load_compaction_details(cursor: &SessionCursor, offset: u64) -> (usize, usize, bool, String) {
    cursor.compaction_details_at(offset)
}

#[must_use]
pub fn one_line(s: &str) -> String {
    const MAX: usize = 60;
    let collapsed = s.replace('\n', " \u{23ce} ");
    if collapsed.chars().count() <= MAX {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(MAX).collect();
        out.push('\u{2026}');
        out
    }
}
