#![allow(clippy::wildcard_imports)]

use super::*;

/// Build the '/tree' picker entries from a lightweight event index.
///
/// Uses [`store::load_index`] (id + `parent_id` + kind discriminant only —
/// no `ContentBlock` deserialization) to build the tree shape, then loads
/// labels on demand via [`store::load_event_at`]. This keeps `/tree` fast
/// on large sessions: the full [`store::load`] is avoided entirely.
///
/// The active path (root → `leaf_id`, or the file's last event when
/// `leaf_id` is `None`) is the trunk — rendered flat. Only actual branches
/// (non-active sibling turns) create indentation, so the common case is two
/// levels deep regardless of conversation length.
///
/// Node kinds:
/// - `user:` — a user-prompt event. Selecting rolls back to BEFORE the
///   prompt and prefills the input (edit and resend).
/// - `agent:` — a `turn_end`/`turn_failed`. Selecting rolls back to AFTER
///   the turn (inclusive), input empty (continue from here).
///
/// Shared context for tree-building functions — avoids passing many params
/// through every recursive call.
struct TreeCtx<'a> {
    indices: &'a [store::EventIndex],
    children_by_parent: &'a HashMap<&'a str, Vec<usize>>,
    by_id: &'a HashMap<&'a str, usize>,
    path: &'a Path,
    native_tools: &'a HashMap<String, Vec<(String, String)>>,
}

pub(super) fn build_tree_entries(
    indices: &[store::EventIndex],
    leaf_id: Option<&str>,
    path: &Path,
) -> Vec<TreeEntry> {
    // Build a map from exec tool-call id to its native tool calls
    // (name, args) for the `exec:` label.
    let native_tools = build_native_tool_map(indices, path);
    let mut children_by_parent: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (i, ix) in indices.iter().enumerate() {
        if !ix.id.is_empty() {
            by_id.insert(ix.id.as_str(), i);
        }
        if let Some(p) = ix.parent_id.as_deref() {
            if !p.is_empty() {
                children_by_parent.entry(p).or_default().push(i);
            }
        }
    }
    let active_path: Vec<usize> = match leaf_id {
        Some(id) if !id.is_empty() => active_path_from_index(indices, &by_id, id),
        // `leaf_id` is `None` (normal linear continuation) or `Some("")`
        // (rolled back to before the root prompt). For `None`, walk from
        // the file's last event. For `Some("")`, the active path is
        // empty — the trunk loop below renders nothing, and we instead
        // treat the root events as branch roots so the whole tree is
        // visible (nothing highlighted).
        None => indices
            .last()
            .map(|ix| active_path_from_index(indices, &by_id, &ix.id))
            .unwrap_or_default(),
        Some(_) => Vec::new(),
    };
    let active_set: std::collections::HashSet<usize> =
        active_path.iter().copied().collect();

    // Trunk = active path filtered to displayable nodes.
    let trunk: Vec<usize> = active_path
        .iter()
        .copied()
        .filter(|&i| is_tree_node(indices[i].kind))
        .collect();

    let n = trunk.len();
    let mut out = Vec::new();
    let ctx = TreeCtx {
        indices,
        children_by_parent: &children_by_parent,
        by_id: &by_id,
        path,
        native_tools: &native_tools,
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
            .get(indices[idx].id.as_str())
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
    // Rolled back to before the root prompt: the trunk is empty, so
    // render every top-level tree node (a tree node whose nearest
    // tree-node ancestor — walking up the parent chain — is absent) as a
    // branch. Nothing is active. The file's root may be a system message
    // (not a tree node), so we can't just take parent_id.is_none().
    if trunk.is_empty() && out.is_empty() {
        let roots: Vec<usize> = indices
            .iter()
            .enumerate()
            .filter(|&(_, ix)| {
                if !is_tree_node(ix.kind) {
                    return false;
                }
                // Walk up the parent chain; this is a top-level tree node
                // iff no ancestor is a tree node.
                let mut cur = ix.parent_id.as_deref();
                while let Some(pid) = cur {
                    let Some(&pidx) = by_id.get(pid) else { break };
                    if is_tree_node(indices[pidx].kind) {
                        return false;
                    }
                    cur = indices[pidx].parent_id.as_deref();
                }
                true
            })
            .map(|(i, _)| i)
            .collect();
        if !roots.is_empty() {
            render_branch_subtree(&ctx, &roots, "", &mut out);
        }
    }
    out
}

/// Build a map from exec tool-call id to its native tool calls (name, args)
/// for the `exec:` label in `/tree`. Scans the index for `NativeTool` events
/// and loads each one from disk to extract the `parent` (exec tool-call id)
/// and `name`/`args`.
fn build_native_tool_map(
    indices: &[store::EventIndex],
    path: &Path,
) -> HashMap<String, Vec<(String, String)>> {
    let mut map: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for ix in indices {
        if ix.kind != store::IndexKind::NativeTool {
            continue;
        }
        let Ok(ev) = store::load_event_at(path, ix.offset) else {
            continue;
        };
        if let SessionEventKind::NativeTool(rec) = ev.kind {
            map.entry(rec.parent.clone())
                .or_default()
                .push((rec.name, one_line(&rec.args)));
        }
    }
    map
}

/// Active path (root-first indices) from a leaf id, using the lightweight
/// index instead of fully-loaded events.
pub(super) fn active_path_from_index(
    indices: &[store::EventIndex],
    by_id: &HashMap<&str, usize>,
    leaf_id: &str,
) -> Vec<usize> {
    let mut path = Vec::new();
    let mut cur = by_id.get(leaf_id).copied();
    while let Some(i) = cur {
        path.push(i);
        cur = indices[i]
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied());
    }
    path.reverse();
    path
}

/// Render branch subtrees. Each root is rendered as its own subtree:
/// the root's linear chain (root → turn outcome → next user prompt → …)
/// sits under the root's connector at this indentation level, and sibling
/// roots are siblings of each other — not flattened into one list. Only
/// actual sub-branches (divergences within a chain) create further
/// indentation.
fn render_branch_subtree(
    ctx: &TreeCtx,
    roots: &[usize],
    prefix: &str,
    out: &mut Vec<TreeEntry>,
) {
    let indices = ctx.indices;
    let children_by_parent = ctx.children_by_parent;
    let n = roots.len();
    for (pos, &root) in roots.iter().enumerate() {
        let is_last = pos == n - 1;
        let connector = if is_last { "└─ " } else { "├─ " };
        let child_indent = format!("{prefix}{}", if is_last { "   " } else { "│  " });
        // Chain = this root + its linear descendants (flat at this level,
        // under the root's connector).
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
            let sub_indent = format!("{child_indent}{}", if cpos == cn - 1 { "   " } else { "│  " });
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
                .get(indices[idx].id.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| {
                    indices[i].kind == store::IndexKind::UserPrompt
                        && !chain_set.contains(&i)
                })
                .collect();
            sub_branches.extend(user_children);
            if !sub_branches.is_empty() {
                render_branch_subtree(ctx, &sub_branches, &sub_indent, out);
            }
        }
    }
}

/// Walk the linear chain from `start`: user → turn outcome → next user
/// prompt → …, following the first user-prompt child at each `turn_end` and
/// the turn outcome at each user prompt.
/// Find the next user-prompt event after a non-user node (a turn outcome
/// or a compaction). After a compaction the next prompt is a grandchild —
/// the compaction's child — so route through a compaction child when there
/// is no direct user-prompt child. Keeps `walk_chain`'s flat chain intact
/// across a compaction boundary.
pub(super) fn find_next_user_prompt(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&str, Vec<usize>>,
) -> Option<usize> {
    let children = children_by_parent.get(indices[start].id.as_str())?;
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
    let comp_children = children_by_parent.get(indices[comp].id.as_str())?;
    comp_children
        .iter()
        .copied()
        .find(|&i| indices[i].kind == store::IndexKind::UserPrompt)
}

pub(super) fn walk_chain(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&str, Vec<usize>>,
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

/// Append one `TreeEntry` for index `idx`, loading the label lazily from
/// disk via the event's byte offset.
fn push_tree_entry(
    ctx: &TreeCtx,
    idx: usize,
    prefix: &str,
    is_active: bool,
    out: &mut Vec<TreeEntry>,
) {
    let ix = &ctx.indices[idx];
    let (label, prefill, branch_point) = match ix.kind {
        store::IndexKind::UserPrompt => {
            let prompt = load_prompt_text(ctx.path, ix.offset);
            (
                format!("user: {}", one_line(&prompt)),
                prompt,
                ix.parent_id.clone().unwrap_or_default(),
            )
        }
        store::IndexKind::ToolResult => {
            let (name, tool_use_id, content, is_error) =
                load_tool_result(idx, ctx.indices, ctx.by_id, ctx.path);
            let marker = if is_error { "\u{2717} " } else { "" };
            // For `exec` tool results, show the native tool calls made
            // inside the exec block instead of the raw result.
            let label = if name == "exec" {
                if let Some(tools) = ctx.native_tools.get(&tool_use_id) {
                    let summary = tools
                        .iter()
                        .map(|(n, a)| format!("{n} {a}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("exec: {marker}{}", one_line(&summary))
                } else {
                    format!("exec: {marker}{}", one_line(&content))
                }
            } else {
                format!("tool: {marker}{name}: {}", one_line(&content))
            };
            (
                label,
                String::new(),
                ix.id.clone(),
            )
        }
        store::IndexKind::TurnEnd => {
            let preview = load_assistant_preview(idx, ctx.indices, ctx.by_id, ctx.path);
            (
                format!(
                    "agent: {}",
                    if preview.is_empty() {
                        "(turn end)".to_string()
                    } else {
                        preview
                    }
                ),
                String::new(),
                ix.id.clone(),
            )
        }
        store::IndexKind::TurnFailed => {
            let error = load_failed_error(ctx.path, ix.offset);
            (
                format!("agent: {} (failed)", one_line(&error)),
                String::new(),
                ix.id.clone(),
            )
        }
        store::IndexKind::Compaction => {
            let (summarized, kept) = load_compaction_counts(ctx.path, ix.offset);
            (
                format!("compact: Compacted {summarized} messages \u{00b7} kept {kept}"),
                String::new(),
                // Roll back to the compaction's parent — the pre-compaction
                // leaf — so the active path excludes the compaction and the
                // full un-folded history is restored. Selecting this node is
                // "revert to before the compact".
                ix.parent_id.clone().unwrap_or_default(),
            )
        }
        store::IndexKind::AssistantMessage | store::IndexKind::NativeTool | store::IndexKind::Other => return,
    };
    out.push(TreeEntry {
        prefix: prefix.to_string(),
        label,
        prefill,
        branch_point,
        is_active,
    });
}

/// Whether a kind is a displayable tree node (user prompt or turn outcome).
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

/// Walk the descendant chain from `start` (a user-prompt event) to find the
/// first `turn_end/turn_failed` — the outcome of this turn. Follows the
/// in-turn chain (assistant → tool → thinking → …), skipping user-prompt
/// children that are branches.
pub(super) fn find_turn_outcome(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&str, Vec<usize>>,
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
        let children = children_by_parent.get(indices[cur].id.as_str())?;
        cur = *children
            .iter()
            .find(|&&i| indices[i].kind != store::IndexKind::UserPrompt)?;
    }
}

/// Preview of the last assistant text in the turn ending at `turn_end_idx`:
/// walk the parent chain (using the index) back to the user prompt, loading
/// only assistant-message events to find the first text block.
pub(super) fn load_assistant_preview(
    turn_end_idx: usize,
    indices: &[store::EventIndex],
    by_id: &HashMap<&str, usize>,
    path: &Path,
) -> String {
    let mut cur = turn_end_idx;
    let mut visited = std::collections::HashSet::new();
    while let Some(parent_id) = indices[cur].parent_id.as_deref() {
        if !visited.insert(cur) {
            break;
        }
        let Some(&pidx) = by_id.get(parent_id) else { break };
        let pentry = &indices[pidx];
        if pentry.kind == store::IndexKind::UserPrompt {
            break;
        }
        if pentry.kind == store::IndexKind::AssistantMessage {
            if let Some(text) = load_assistant_text(path, pentry.offset) {
                return one_line(&text);
            }
        }
        cur = pidx;
    }
    String::new()
}

/// Load a user-prompt event and extract its first text block.
pub(super) fn load_prompt_text(path: &Path, offset: u64) -> String {
    let Ok(ev) = store::load_event_at(path, offset) else {
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

/// Load a tool-result message and cross-reference the tool name from the
/// parent assistant message's `ToolUse` block (matched by `tool_use_id`).
/// Returns `(tool_name, result_content, is_error)`.
pub(super) fn load_tool_result(
    idx: usize,
    indices: &[store::EventIndex],
    by_id: &HashMap<&str, usize>,
    path: &Path,
) -> (String, String, String, bool) {
    let ix = &indices[idx];
    let Ok(ev) = store::load_event_at(path, ix.offset) else {
        return (String::new(), String::new(), String::new(), false);
    };
    let SessionEventKind::Message(m) = ev.kind else {
        return (String::new(), String::new(), String::new(), false);
    };
    let Some(block) = m.blocks.iter().find_map(|b| match b {
        ContentBlock::ToolResult { tool_use_id, content, is_error } => {
            Some((tool_use_id.clone(), content.clone(), *is_error))
        }
        _ => None,
    }) else {
        return (String::new(), String::new(), String::new(), false);
    };
    let (tool_use_id, content, is_error) = block;
    // Walk to the parent assistant message and find the matching ToolUse
    // block to get the tool name.
    let name = ix
        .parent_id
        .as_deref()
        .and_then(|pid| by_id.get(pid).copied())
        .and_then(|pidx| {
            let pentry = &indices[pidx];
            let pev = store::load_event_at(path, pentry.offset).ok()?;
            let SessionEventKind::Message(pm) = pev.kind else { return None };
            pm.blocks.iter().find_map(|b| match b {
                ContentBlock::ToolUse { id, name, .. } if id == &tool_use_id => {
                    Some(name.clone())
                }
                _ => None,
            })
        })
        .unwrap_or_else(|| "?".to_string());
    (name, tool_use_id, content, is_error)
}

/// Load an assistant-message event and extract its first text block.
pub(super) fn load_assistant_text(path: &Path, offset: u64) -> Option<String> {
    let ev = store::load_event_at(path, offset).ok()?;
    let SessionEventKind::Message(m) = ev.kind else { return None };
    if m.role != Role::Assistant {
        return None;
    }
    m.blocks
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
}

/// Load a `turn_failed` event and extract its error message.
pub(super) fn load_failed_error(path: &Path, offset: u64) -> String {
    let Ok(ev) = store::load_event_at(path, offset) else {
        return String::new();
    };
    if let SessionEventKind::TurnFailed { error, .. } = ev.kind {
        error
    } else {
        String::new()
    }
}

/// Load a `compaction` event and extract its `summarized`/`kept` counts
/// for the `/tree` node label.
pub(super) fn load_compaction_counts(path: &Path, offset: u64) -> (usize, usize) {
    let Ok(ev) = store::load_event_at(path, offset) else {
        return (0, 0);
    };
    if let SessionEventKind::Compaction { summarized, kept, .. } = ev.kind {
        (summarized, kept)
    } else {
        (0, 0)
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