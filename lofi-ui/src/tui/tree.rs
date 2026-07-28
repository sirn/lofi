#![allow(clippy::wildcard_imports)]

use super::*;

/// The active path (root → `leaf_id`) is the trunk — rendered flat.
/// `None` means the cursor is explicitly before every root event.
/// (non-active sibling turns) create indentation, so the common case is two
/// levels deep regardless of conversation length.
struct TreeCtx<'a> {
    indices: &'a [store::EventIndex],
    children_by_parent: &'a HashMap<&'a store::IndexId, Vec<usize>>,
    by_id: &'a HashMap<&'a store::IndexId, usize>,
    cursor: &'a store::SessionCursor,
    native_tools: std::cell::RefCell<HashMap<String, Vec<(String, String)>>>,
    lazy_native_tools: bool,
    hydrate: bool,
    retain_prefill: bool,
    hydrate_only: Option<&'a std::collections::HashSet<usize>>,
}

#[allow(clippy::too_many_lines)]
pub(super) fn build_tree_entries(
    indices: &[store::EventIndex],
    leaf_id: Option<&str>,
    cursor: &store::SessionCursor,
) -> Vec<TreeEntry> {
    build_tree_entries_inner(indices, leaf_id, cursor, true, true, None)
}

pub(super) fn build_tree_entry_skeletons(
    indices: &[store::EventIndex],
    leaf_id: Option<&str>,
    cursor: &store::SessionCursor,
) -> Vec<TreeEntry> {
    build_tree_entries_inner(indices, leaf_id, cursor, false, false, None)
}

pub(super) fn hydrate_tree_entry_rows(
    indices: &[store::EventIndex],
    cursor: &store::SessionCursor,
    requested: &[(usize, TreeEntry)],
    mut emit: impl FnMut(Vec<(usize, TreeEntry)>) -> bool,
    cancelled: impl Fn() -> bool,
) {
    if requested.is_empty() || cancelled() {
        return;
    }
    let mut children_by_parent: HashMap<&store::IndexId, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<&store::IndexId, usize> = HashMap::new();
    for (index, entry) in indices.iter().enumerate() {
        if !entry.id.is_empty() {
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
        push_tree_entry(
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

pub(super) fn hydrate_tree_entry_window(
    indices: &[store::EventIndex],
    cursor: &store::SessionCursor,
    skeletons: &[TreeEntry],
    display_range: std::ops::Range<usize>,
    emit: impl FnMut(Vec<(usize, TreeEntry)>) -> bool,
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
    hydrate_tree_entry_rows(indices, cursor, &requested, emit, cancelled);
}

fn hidden_checkpoint_indices(
    indices: &[store::EventIndex],
    active_path: &[usize],
    cursor: &store::SessionCursor,
) -> std::collections::HashSet<usize> {
    let mut hidden = std::collections::HashSet::new();
    for (marker_pos, &idx) in active_path.iter().enumerate() {
        if indices[idx].kind != store::IndexKind::Compaction {
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

fn top_level_tree_nodes(
    indices: &[store::EventIndex],
    by_id: &HashMap<&store::IndexId, usize>,
) -> Vec<usize> {
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

fn build_tree_entries_inner(
    indices: &[store::EventIndex],
    leaf_id: Option<&str>,
    cursor: &store::SessionCursor,
    hydrate: bool,
    retain_prefill: bool,
    hydrate_only: Option<&std::collections::HashSet<usize>>,
) -> Vec<TreeEntry> {
    let native_tools = if hydrate && hydrate_only.is_none() {
        build_native_tool_map(indices, cursor)
    } else {
        HashMap::new()
    };
    let mut children_by_parent: HashMap<&store::IndexId, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<&store::IndexId, usize> = HashMap::new();
    for (i, ix) in indices.iter().enumerate() {
        if !ix.id.is_empty() {
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
        let connector = if is_last { "└─ " } else { "├─ " };
        let child_indent = if is_last { "   " } else { "│  " };
        push_tree_entry(&ctx, idx, connector, active_set.contains(&idx), &mut out);
        let mut branches: Vec<usize> = Vec::new();
        if indices[idx].kind == store::IndexKind::UserPrompt {
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
            .filter(|&i| {
                indices[i].kind == store::IndexKind::UserPrompt && !active_set.contains(&i)
            })
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
    indices: &[store::EventIndex],
    cursor: &store::SessionCursor,
) -> HashMap<String, Vec<(String, String)>> {
    let offsets: Vec<u64> = indices
        .iter()
        .filter(|entry| entry.kind == store::IndexKind::NativeTool)
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
            .find(|&&child| ctx.indices[child].kind != store::IndexKind::UserPrompt)
        else {
            break;
        };
        match ctx.indices[next].kind {
            store::IndexKind::NativeTool => offsets.push(ctx.indices[next].offset),
            store::IndexKind::TurnEnd | store::IndexKind::TurnFailed => break,
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
pub(super) fn active_path_from_index(
    indices: &[store::EventIndex],
    by_id: &HashMap<&store::IndexId, usize>,
    leaf_id: &str,
) -> Vec<usize> {
    let mut path = Vec::new();
    let leaf = store::IndexId::parse(leaf_id.to_string());
    let mut cur = by_id.get(&leaf).copied();
    while let Some(i) = cur {
        path.push(i);
        cur = indices[i]
            .parent_id
            .as_ref()
            .and_then(|p| by_id.get(p).copied());
    }
    path.reverse();
    path
}

fn render_branch_subtree(ctx: &TreeCtx, roots: &[usize], prefix: &str, out: &mut Vec<TreeEntry>) {
    let indices = ctx.indices;
    let children_by_parent = ctx.children_by_parent;
    let n = roots.len();
    for (pos, &root) in roots.iter().enumerate() {
        let is_last = pos == n - 1;
        let connector = if is_last { "└─ " } else { "├─ " };
        let child_indent = format!("{prefix}{}", if is_last { "   " } else { "│  " });
        let chain = walk_chain(root, indices, children_by_parent);
        let chain_set: std::collections::HashSet<usize> = chain.iter().copied().collect();
        let cn = chain.len();
        for (cpos, &idx) in chain.iter().enumerate() {
            let cprefix = if cpos == 0 {
                format!("{prefix}{connector}")
            } else {
                let cis_last = cpos == cn - 1;
                format!("{child_indent}{}", if cis_last { "└─ " } else { "├─ " })
            };
            let sub_indent = format!(
                "{child_indent}{}",
                if cpos == cn - 1 { "   " } else { "│  " }
            );
            push_tree_entry(ctx, idx, &cprefix, false, out);
            let mut sub_branches: Vec<usize> = Vec::new();
            if indices[idx].kind == store::IndexKind::UserPrompt {
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
                .filter(|&i| {
                    indices[i].kind == store::IndexKind::UserPrompt && !chain_set.contains(&i)
                })
                .collect();
            sub_branches.extend(user_children);
            if !sub_branches.is_empty() {
                render_branch_subtree(ctx, &sub_branches, &sub_indent, out);
            }
        }
    }
}

pub(super) fn find_next_user_prompt(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&store::IndexId, Vec<usize>>,
) -> Option<usize> {
    let children = children_by_parent.get(&indices[start].id)?;
    if let Some(&u) = children
        .iter()
        .find(|&&i| indices[i].kind == store::IndexKind::UserPrompt)
    {
        return Some(u);
    }
    let comp = children
        .iter()
        .copied()
        .find(|&i| indices[i].kind == store::IndexKind::Compaction)?;
    let comp_children = children_by_parent.get(&indices[comp].id)?;
    comp_children
        .iter()
        .copied()
        .find(|&i| indices[i].kind == store::IndexKind::UserPrompt)
}

pub(super) fn walk_chain(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&store::IndexId, Vec<usize>>,
) -> Vec<usize> {
    let mut chain = vec![start];
    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut cur = start;
    loop {
        let next = if indices[cur].kind == store::IndexKind::UserPrompt {
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

type TreeEntryFields = (String, String, String);

fn skeleton_tree_entry(ix: &store::EventIndex) -> Option<TreeEntryFields> {
    let label = match ix.kind {
        store::IndexKind::UserPrompt => "user: loading…",
        store::IndexKind::ToolResult => "tool: loading…",
        store::IndexKind::TurnEnd => "agent: loading…",
        store::IndexKind::TurnFailed => "agent: loading… (failed)",
        store::IndexKind::Compaction => "compact: loading…",
        _ => return None,
    }
    .to_string();
    let branch_point = match ix.kind {
        store::IndexKind::UserPrompt | store::IndexKind::Compaction => ix
            .parent_id
            .as_ref()
            .map(store::IndexId::to_event_id)
            .unwrap_or_default(),
        _ => ix.id.to_event_id(),
    };
    Some((label, String::new(), branch_point))
}

fn tool_result_tree_entry(ctx: &TreeCtx, idx: usize) -> TreeEntryFields {
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

fn compaction_tree_entry(ctx: &TreeCtx, ix: &store::EventIndex) -> TreeEntryFields {
    let (summarized, kept, checkpointed, first_kept) =
        load_compaction_details(ctx.cursor, ix.offset);
    let branch_point = if checkpointed && !first_kept.is_empty() {
        ctx.by_id
            .get(&store::IndexId::parse(first_kept))
            .and_then(|&i| {
                ctx.indices[i]
                    .parent_id
                    .as_ref()
                    .map(store::IndexId::to_event_id)
            })
            .unwrap_or_default()
    } else {
        ix.parent_id
            .as_ref()
            .map(store::IndexId::to_event_id)
            .unwrap_or_default()
    };
    (
        format!("compact: Compacted {summarized} messages · kept {kept}"),
        String::new(),
        branch_point,
    )
}

fn hydrated_tree_entry(ctx: &TreeCtx, idx: usize) -> Option<TreeEntryFields> {
    let ix = &ctx.indices[idx];
    match ix.kind {
        store::IndexKind::UserPrompt => {
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
                    .map(store::IndexId::to_event_id)
                    .unwrap_or_default(),
            ))
        }
        store::IndexKind::ToolResult => Some(tool_result_tree_entry(ctx, idx)),
        store::IndexKind::TurnEnd => {
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
        store::IndexKind::TurnFailed => Some((
            format!(
                "agent: {} (failed)",
                one_line(&load_failed_error(ctx.cursor, ix.offset))
            ),
            String::new(),
            ix.id.to_event_id(),
        )),
        store::IndexKind::Compaction => Some(compaction_tree_entry(ctx, ix)),
        _ => None,
    }
}

fn push_tree_entry(
    ctx: &TreeCtx,
    idx: usize,
    prefix: &str,
    is_active: bool,
    out: &mut Vec<TreeEntry>,
) {
    if ctx
        .hydrate_only
        .is_some_and(|sources| !sources.contains(&idx))
    {
        return;
    }
    let ix = &ctx.indices[idx];
    let fields = if ctx.hydrate {
        hydrated_tree_entry(ctx, idx)
    } else {
        skeleton_tree_entry(ix)
    };
    let Some((label, prefill, branch_point)) = fields else {
        return;
    };
    out.push(TreeEntry {
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

pub(super) fn is_tree_node(kind: store::IndexKind) -> bool {
    matches!(
        kind,
        store::IndexKind::UserPrompt
            | store::IndexKind::ToolResult
            | store::IndexKind::TurnEnd
            | store::IndexKind::TurnFailed
            | store::IndexKind::Compaction
    )
}

pub(super) fn find_turn_outcome(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&store::IndexId, Vec<usize>>,
) -> Option<usize> {
    let mut cur = start;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(cur) {
            return None;
        }
        match indices[cur].kind {
            store::IndexKind::TurnEnd | store::IndexKind::TurnFailed => return Some(cur),
            _ => {}
        }
        let children = children_by_parent.get(&indices[cur].id)?;
        cur = *children
            .iter()
            .find(|&&i| indices[i].kind != store::IndexKind::UserPrompt)?;
    }
}

pub(super) fn load_assistant_preview(
    turn_end_idx: usize,
    indices: &[store::EventIndex],
    by_id: &HashMap<&store::IndexId, usize>,
    cursor: &store::SessionCursor,
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
        if pentry.kind == store::IndexKind::UserPrompt {
            break;
        }
        if pentry.kind == store::IndexKind::AssistantMessage {
            if let Some(text) = load_assistant_text(cursor, pentry.offset) {
                return one_line(&text);
            }
        }
        cur = pidx;
    }
    String::new()
}

pub(super) fn load_prompt_text(cursor: &store::SessionCursor, offset: u64) -> String {
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

pub(super) fn load_tool_result(
    idx: usize,
    indices: &[store::EventIndex],
    by_id: &HashMap<&store::IndexId, usize>,
    cursor: &store::SessionCursor,
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

pub(super) fn load_assistant_text(cursor: &store::SessionCursor, offset: u64) -> Option<String> {
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

pub(super) fn load_failed_error(cursor: &store::SessionCursor, offset: u64) -> String {
    let Ok(ev) = cursor.event_at(offset) else {
        return String::new();
    };
    if let SessionEventKind::TurnFailed { error, .. } = ev.kind {
        error
    } else {
        String::new()
    }
}

fn load_compaction_details(
    cursor: &store::SessionCursor,
    offset: u64,
) -> (usize, usize, bool, String) {
    let Ok(ev) = cursor.event_at(offset) else {
        return (0, 0, false, String::new());
    };
    if let SessionEventKind::Compaction {
        summarized,
        kept,
        checkpointed_tail,
        first_kept_entry_id,
        ..
    } = ev.kind
    {
        (summarized, kept, checkpointed_tail, first_kept_entry_id)
    } else {
        (0, 0, false, String::new())
    }
}

pub(super) fn one_line(s: &str) -> String {
    const MAX: usize = 60;
    let collapsed = s.replace('\n', " ⏎ ");
    if collapsed.chars().count() <= MAX {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(MAX).collect();
        out.push('…');
        out
    }
}
