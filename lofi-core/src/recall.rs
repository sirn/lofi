#![allow(clippy::unwrap_used)]
//! Searchable, scoped session-history recall, including messages folded away
//! by compaction. The slash command and native tool share this engine and read
//! transcripts through the session cursor.

use std::collections::HashMap;
use std::fmt::Write;

use lofi_types::{ContentBlock, Message, NativeToolRecord, Role, SessionEvent, SessionEventKind};

use crate::session::store;

// Re-export the public request/scope/outcome shapes so callers can reach
// them as `lofi_core::recall::*` without depending on `lofi_types::recall`.
pub use lofi_types::recall::{CompactionTarget, RecallOutcome, RecallRequest, RecallScope};

mod file;
pub use file::recall_cursor;

const PAGE_SIZE: usize = 5;
const DEFAULT_RECENT: usize = 25;
/// Hard cap on total search results, so a broad query can't flood the turn.
const MAX_SEARCH_RESULTS: usize = 50;
/// Don't let the agent page past this many pages; narrow the query instead.
const MAX_PAGES: usize = 5;

/// Clip bound for the short `summary` (browse/search mode). Full content is
/// returned on `expand`.
const CLIP_SUMMARY: usize = 300;
const CLIP_THINKING: usize = 150;
const CLIP_TOOL_RESULT: usize = 200;

/// A flat, rendered view of one message: the unit recall returns. `index` is
/// the message's global index in file order (stable across scopes), so
/// `expand` indices line up regardless of which scope produced them.
#[derive(Debug, Clone)]
pub struct RecallEntry {
    pub index: usize,
    pub role: &'static str,
    pub summary: String,
    pub files: Vec<String>,
}

/// A search hit: an entry plus a context snippet around the first match and
/// the number of query terms that hit (for ranking).
#[derive(Debug, Clone)]
struct SearchHit {
    entry: RecallEntry,
    snippet: Option<String>,
    match_count: usize,
}

/// Run recall over a full session event log and return the rendered output.
///
/// `events` is the entire transcript (every line after the header), in file
/// order — the same slice `store::load` returns. Scoping filters *which*
/// messages render, never the global indexing.
#[must_use]
// One cohesive transcript walk; extracting sub-steps would scatter the flow.
#[allow(clippy::too_many_lines)]
pub fn recall(events: &[SessionEvent], req: &RecallRequest) -> RecallOutcome {
    let scope = resolve_scope(events, &req.scope);
    let allowed_ids = match &scope {
        Scope::Lineage(ids) | Scope::Compaction { ids, .. } => Some(ids.clone()),
        Scope::All => None,
    };

    // Index native tool records by their parent exec id so the assistant
    // renderer can attach them. Built once over the full event list.
    let mut native_by_parent: HashMap<String, Vec<&NativeToolRecord>> = HashMap::new();
    for e in events {
        if let SessionEventKind::NativeTool(rec) = &e.kind {
            native_by_parent
                .entry(rec.parent.clone())
                .or_default()
                .push(rec);
        }
    }

    let has_query = req.query.as_deref().is_some_and(|q| !q.trim().is_empty());
    let expand_set: std::collections::HashSet<usize> = req.expand.iter().copied().collect();
    let has_expand = !expand_set.is_empty();

    // expand-without-query: render the requested indices at full content.
    if has_expand && !has_query {
        let entries = load_all_messages(events, true, allowed_ids.as_ref(), &native_by_parent);
        let by_index: HashMap<usize, RecallEntry> =
            entries.into_iter().map(|e| (e.index, e)).collect();
        let mut expanded: Vec<RecallEntry> = Vec::new();
        let mut invalid: Vec<usize> = Vec::new();
        for &i in &req.expand {
            match by_index.get(&i) {
                Some(e) => expanded.push(e.clone()),
                None => invalid.push(i),
            }
        }
        if !invalid.is_empty() {
            return RecallOutcome {
                text: format!(
                    "Cannot expand indices outside {}: {}",
                    scope_label(&scope),
                    invalid
                        .iter()
                        .map(std::string::ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                status: format!("{} invalid", invalid.len()),
            };
        }
        let text = format_recall_output(&expanded, None, None);
        return RecallOutcome {
            text,
            status: format!("expanded {}", expanded.len()),
        };
    }

    // Load clipped entries for display. Keep only borrowed references to raw
    // messages for search/expand: cloning the full transcript here used to
    // duplicate every tool result, and search then duplicated it a second time
    // into a Vec<String>. On a 55 MiB session that created a ~165 MiB peak.
    let entries = load_all_messages(events, false, allowed_ids.as_ref(), &native_by_parent);

    if !has_query {
        // Browse mode: most recent DEFAULT_RECENT entries, flat.
        let start = entries.len().saturating_sub(DEFAULT_RECENT);
        let recent = &entries[start..];
        let label = if matches!(scope, Scope::All) {
            Some("Scope: all".to_string())
        } else {
            None
        };
        let text = format_recall_output(recent, None, label);
        let status = format!("{} entries", recent.len());
        return RecallOutcome { text, status };
    }

    let raw_messages = raw_messages(events, allowed_ids.as_ref());
    let query = req.query.as_deref().unwrap_or("").trim();
    let page = req.page.max(1);
    let all_hits = search_entries(&entries, &raw_messages, query);
    if all_hits.is_empty() {
        return RecallOutcome {
            text: format!("No matches for \"{query}\" in {}.", scope_label(&scope)),
            status: "0 matches".to_string(),
        };
    }

    let total_pages = all_hits.len().div_ceil(PAGE_SIZE).max(1);
    if page > MAX_PAGES.min(total_pages) {
        return RecallOutcome {
            text: format!(
                "Too many results to page through ({} matches across {} pages). \
                 Try a more specific query or scope:compaction:N to narrow the range.",
                all_hits.len(),
                total_pages
            ),
            status: format!("{} matches", all_hits.len()),
        };
    }
    let start = (page - 1) * PAGE_SIZE;
    let mut page_hits: Vec<SearchHit> =
        all_hits[start..(start + PAGE_SIZE).min(all_hits.len())].to_vec();

    // Expand: swap the clipped snippet for full content on paged hits whose
    // index is in expand_set. Re-render from the parallel raw message.
    let mut expanded: Vec<usize> = Vec::new();
    if has_expand {
        let raw_by_index: HashMap<usize, &Message> = raw_messages
            .iter()
            .enumerate()
            .map(|(i, message)| (entries[i].index, *message))
            .collect();
        for hit in &mut page_hits {
            if !expand_set.contains(&hit.entry.index) {
                continue;
            }
            let Some(raw) = raw_by_index.get(&hit.entry.index) else {
                continue;
            };
            let full = render_message(raw, hit.entry.index, true, &native_by_parent);
            hit.entry.summary.clone_from(&full.summary);
            hit.snippet = Some(full.summary);
            expanded.push(hit.entry.index);
        }
    }

    let header = if total_pages > 1 {
        format!(
            "Page {page}/{total_pages} ({} total matches{})",
            all_hits.len(),
            scope_suffix(&scope)
        )
    } else {
        format!("{} matches{}", all_hits.len(), scope_suffix(&scope))
    };
    let mut footer: Vec<String> = Vec::new();
    if page < total_pages && page < MAX_PAGES {
        footer.push(format!("--- Use page:{} for more results ---", page + 1));
    } else if total_pages > MAX_PAGES {
        footer.push(format!(
            "--- Results truncated at {MAX_PAGES} pages. Narrow the query or scope. ---"
        ));
    }
    if has_expand {
        let not_expanded: Vec<usize> = req
            .expand
            .iter()
            .copied()
            .filter(|i| !expanded.contains(i))
            .collect();
        let noun = if expanded.len() == 1 {
            "entry"
        } else {
            "entries"
        };
        if !expanded.is_empty() && not_expanded.is_empty() {
            footer.push(format!(
                "--- expanded {} {} to full content ---",
                expanded.len(),
                noun
            ));
        } else if !expanded.is_empty() {
            footer.push(format!(
                "--- expanded {} {} to full content; not on this page: {} ---",
                expanded.len(),
                noun,
                not_expanded
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else {
            footer.push("--- no expand indices on this page ---".to_string());
        }
    }
    let footer_text = if footer.is_empty() {
        String::new()
    } else {
        format!("\n{}", footer.join("\n"))
    };

    // format_recall_output takes entries; map hits back to entries (snippet
    // carried separately). We render via the segment formatter by passing
    // entries and letting it re-derive matches from the query — simpler to
    // hand the hits to a dedicated formatter.
    let mut text = format_search_output(&page_hits, query, &header);
    text.push_str(&footer_text);
    RecallOutcome {
        text,
        status: format!("{} matches", all_hits.len()),
    }
}

// ── scope resolution ──────────────────────────────────────────────────────

/// A resolved scope: either a set of allowed event ids (lineage / compaction
/// range) or the whole file (`All`).
enum Scope {
    Lineage(std::collections::HashSet<String>),
    All,
    /// `ids` is the summarized range's event ids; `label` is for display.
    Compaction {
        ids: std::collections::HashSet<String>,
        label: String,
    },
}

fn resolve_scope(events: &[SessionEvent], scope: &RecallScope) -> Scope {
    match scope {
        RecallScope::All => Scope::All,
        RecallScope::Lineage => {
            let path = store::active_path_from_leaf(events);
            let ids: std::collections::HashSet<String> =
                path.iter().map(|&i| events[i].id.clone()).collect();
            Scope::Lineage(ids)
        }
        RecallScope::Compaction(target) => {
            // Collect compaction markers on the active path, in root->leaf
            // order, and resolve the target's summarized_range to event ids.
            let active = store::active_path_from_leaf(events);
            let compactions: Vec<&SessionEvent> = active
                .iter()
                .map(|&i| &events[i])
                .filter(|e| matches!(e.kind, SessionEventKind::Compaction { .. }))
                .collect();
            let Some(target_ev) = (match target {
                CompactionTarget::Latest => compactions.last().copied(),
                CompactionTarget::Index(n) => compactions.get(*n).copied(),
            }) else {
                return Scope::Lineage(default_lineage_ids(events));
            };
            let range = match &target_ev.kind {
                SessionEventKind::Compaction {
                    summarized_range, ..
                } => summarized_range.clone(),
                _ => unreachable!(),
            };
            // Map the [first, last] event ids to every message event id in
            // the file between them (inclusive). On the active path this is
            // exactly the folded range; off-path messages inside the span are
            // excluded by intersecting with the lineage below.
            let ids = collect_range_ids(events, &range[0], &range[1]);
            Scope::Compaction {
                ids,
                label: format!(
                    "scope:compaction:{}",
                    match target {
                        CompactionTarget::Latest => "latest".to_string(),
                        CompactionTarget::Index(n) => n.to_string(),
                    }
                ),
            }
        }
    }
}

fn default_lineage_ids(events: &[SessionEvent]) -> std::collections::HashSet<String> {
    store::active_path_from_leaf(events)
        .iter()
        .map(|&i| events[i].id.clone())
        .collect()
}

/// Every message event id whose file position falls within `[first, last]`
/// (inclusive). Scanning file order keeps the global-index alignment. If
/// either endpoint id is absent (compact-all collapsed the whole live list,
/// or a malformed marker), the range is empty.
fn collect_range_ids(
    events: &[SessionEvent],
    first: &str,
    last: &str,
) -> std::collections::HashSet<String> {
    if first.is_empty() && last.is_empty() {
        return std::collections::HashSet::new();
    }
    let mut in_range = first.is_empty(); // compact-all with empty first: span to last
    let mut ids = std::collections::HashSet::new();
    for e in events {
        if !in_range && (e.id == first || (first.is_empty() && e.id == last)) {
            in_range = true;
        }
        if in_range && matches!(e.kind, SessionEventKind::Message(_)) {
            ids.insert(e.id.clone());
        }
        if in_range && e.id == last {
            break;
        }
    }
    ids
}

fn scope_label(scope: &Scope) -> &'static str {
    match scope {
        Scope::All => "session history",
        Scope::Lineage(_) => "active lineage",
        Scope::Compaction { .. } => "compaction range",
    }
}

fn scope_suffix(scope: &Scope) -> String {
    match scope {
        Scope::All => " (scope: all)".to_string(),
        Scope::Lineage(_) => String::new(),
        Scope::Compaction { label, .. } => format!(" ({label})"),
    }
}

// ── loading & rendering ────────────────────────────────────────────────────

/// Render every in-scope message to a flat entry, assigning global indices
/// in file order. `full` controls clipping (search/browse = clipped, expand
/// = full).
fn load_all_messages(
    events: &[SessionEvent],
    full: bool,
    allowed_ids: Option<&std::collections::HashSet<String>>,
    native_by_parent: &HashMap<String, Vec<&NativeToolRecord>>,
) -> Vec<RecallEntry> {
    let mut out: Vec<RecallEntry> = Vec::new();
    let mut message_index = 0usize;
    for e in events {
        let allowed = allowed_ids.is_none_or(|ids| ids.contains(&e.id));
        if let SessionEventKind::Message(m) = &e.kind {
            if allowed {
                out.push(render_message(m, message_index, full, native_by_parent));
            }
            message_index += 1;
        } else if allowed && matches!(e.kind, SessionEventKind::NativeTool(_)) {
            // NativeTool events are sidecars, not messages; they don't get
            // their own index but are counted into the parent above.
        } else if matches!(e.kind, SessionEventKind::NativeTool(_)) {
            // not a message; index unaffected
        }
    }
    out
}

/// The raw messages parallel to `load_all_messages`'s output (same scope,
/// same order, same global indices), kept for full-text search and expand.
fn raw_messages<'a>(
    events: &'a [SessionEvent],
    allowed_ids: Option<&std::collections::HashSet<String>>,
) -> Vec<&'a Message> {
    events
        .iter()
        .filter(|event| allowed_ids.is_none_or(|ids| ids.contains(&event.id)))
        .filter_map(|event| match &event.kind {
            SessionEventKind::Message(message) => Some(message),
            _ => None,
        })
        .collect()
}

/// Render one message to a flat entry. Native tool records whose parent is
/// a `ToolUse` id in this assistant message are attached as the tool-call
/// surface (lofi's exec wrapper is the only LLM-facing tool).
fn render_message(
    msg: &Message,
    index: usize,
    full: bool,
    native_by_parent: &HashMap<String, Vec<&NativeToolRecord>>,
) -> RecallEntry {
    match msg.role {
        Role::User => RecallEntry {
            index,
            role: "user",
            summary: clip(&text_of(msg), if full { usize::MAX } else { CLIP_SUMMARY }),
            files: Vec::new(),
        },
        Role::Tool => RecallEntry {
            index,
            role: "tool_result",
            summary: render_tool_result(msg, full),
            files: Vec::new(),
        },
        Role::System => RecallEntry {
            index,
            role: "system",
            summary: clip(&text_of(msg), if full { usize::MAX } else { CLIP_SUMMARY }),
            files: Vec::new(),
        },
        Role::Assistant => render_assistant(msg, index, full, native_by_parent),
    }
}

fn render_assistant(
    msg: &Message,
    index: usize,
    full: bool,
    native_by_parent: &HashMap<String, Vec<&NativeToolRecord>>,
) -> RecallEntry {
    let mut tools: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut exec_ids: Vec<String> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    for b in &msg.blocks {
        match b {
            ContentBlock::ToolUse { id, name, input } => {
                exec_ids.push(id.clone());
                // The exec call itself; show a short code clip so recall can
                // match against what the agent ran, not just native tools.
                if let Some(code) = input.get("code").and_then(|v| v.as_str()) {
                    let clip_len = if full { 200 } else { 80 };
                    tools.push(format!("{name}({})", clip(code, clip_len)));
                } else {
                    tools.push(name.clone());
                }
            }
            ContentBlock::Text { text } => text_parts.push(text.clone()),
            ContentBlock::Thinking { text, .. } => thinking_parts.push(text.clone()),
            ContentBlock::ToolResult { .. } => {}
        }
    }
    // Attach native tool calls (the real file/shell actions) keyed by exec id.
    for eid in &exec_ids {
        if let Some(recs) = native_by_parent.get(eid) {
            for rec in recs {
                tools.push(format!(
                    "lofi.{}({})",
                    rec.name,
                    summarize_native_args(&rec.name, &rec.args)
                ));
                if let Some(p) = extract_path(&rec.name, &rec.args) {
                    files.push(p);
                }
            }
        }
    }
    let mut summary = String::new();
    if !tools.is_empty() {
        summary.push_str(&tools.join(", "));
        summary.push('\n');
    }
    if !thinking_parts.is_empty() {
        let t = thinking_parts.join("\n");
        let clip_len = if full { usize::MAX } else { CLIP_THINKING };
        let _ = writeln!(summary, "[thinking] {}", clip(&t, clip_len));
    }
    if !text_parts.is_empty() {
        let t = text_parts.join("\n");
        let clip_len = if full { usize::MAX } else { CLIP_SUMMARY };
        summary.push_str(&clip(&t, clip_len));
    }
    RecallEntry {
        index,
        role: "assistant",
        summary: summary.trim_end().to_string(),
        files,
    }
}

fn render_tool_result(msg: &Message, full: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    for b in &msg.blocks {
        if let ContentBlock::ToolResult {
            content, is_error, ..
        } = b
        {
            let prefix = if *is_error { "ERROR " } else { "" };
            let clip_len = if full { usize::MAX } else { CLIP_TOOL_RESULT };
            parts.push(format!("{prefix}[exec] {}", clip(content, clip_len)));
        }
    }
    parts.join("\n")
}

/// First text block's text (for user/system messages, which carry one).
fn text_of(msg: &Message) -> String {
    msg.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A short, single-line summary of a native tool's args, for the tool-call
/// line. Shows the most informative scalar arg per tool.
fn summarize_native_args(name: &str, args_json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return clip(args_json, 60);
    };
    let pick = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
                return Some(clip(s, 80));
            }
        }
        None
    };
    let s = match name {
        "read" | "bash_read" | "edit" | "write" | "ls" => pick(&["path", "file", "dir"]),
        "find" => pick(&["glob", "dir"]),
        "grep" => pick(&["pattern", "regex", "query", "path"]),
        "bash" => pick(&["cmd", "command"]),
        "agent" => pick(&["prompt"]),
        _ => pick(&["path", "cmd", "pattern"]),
    };
    s.unwrap_or_default()
}

/// Extract a path-like arg from a native tool's args for the `files` list.
fn extract_path(name: &str, args_json: &str) -> Option<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) else {
        return None;
    };
    let keys: &[&str] = match name {
        "read" | "bash_read" | "edit" | "write" | "ls" => &["path", "file", "dir"],
        "find" => &["dir", "glob"],
        "grep" => &["path"],
        _ => return None,
    };
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    None
}

// ── search (BM25-lite + regex fallback) ─────────────────────────────────────

/// Search the rendered entries. A query with regex metacharacters is treated
/// as one pattern; otherwise it's tokenized into terms and ranked by BM25.
#[allow(clippy::too_many_lines)]
fn search_entries(entries: &[RecallEntry], messages: &[&Message], query: &str) -> Vec<SearchHit> {
    debug_assert_eq!(entries.len(), messages.len());
    let raw_query = query.trim();
    if raw_query.is_empty() {
        return Vec::new();
    }

    if looks_like_regex(raw_query) {
        let re = safe_regex(raw_query);
        let mut hits: Vec<SearchHit> = Vec::new();
        for (index, message) in messages.iter().enumerate() {
            if message_matches(message, |text| re.is_match(text)) {
                let snippet = message_snippet(message, &re);
                hits.push(SearchHit {
                    entry: entries[index].clone(),
                    snippet,
                    match_count: 1,
                });
                if hits.len() >= MAX_SEARCH_RESULTS {
                    break;
                }
            }
        }
        return hits;
    }

    let raw_terms: Vec<&str> = raw_query.split_whitespace().collect();
    let terms = filter_stopwords(&raw_terms);
    if terms.is_empty() {
        return Vec::new();
    }
    let patterns: Vec<regex::Regex> = terms
        .iter()
        .map(|term| regex::Regex::new(&regex::escape(term)).unwrap())
        .collect();

    // Compute BM25 statistics directly over borrowed block strings. The old
    // path assembled a full String for every message and retained all of them
    // in a docs Vec, duplicating the complete transcript during every recall.
    let n = messages.len();
    let lengths: Vec<usize> = messages
        .iter()
        .map(|message| message_word_count(message))
        .collect();
    let avg_dl = lengths.iter().sum::<usize>() as f64 / n.max(1) as f64;
    let mut df: Vec<usize> = vec![0; terms.len()];
    for message in messages {
        for (index, pattern) in patterns.iter().enumerate() {
            if message_matches(message, |text| pattern.is_match(text)) {
                df[index] += 1;
            }
        }
    }

    let min_match = if terms.len() >= 3 { 2 } else { 1 };
    let mut scored: Vec<(f64, SearchHit)> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let mut match_count = 0;
        let mut score = 0.0;
        let dl = lengths[index] as f64;
        for (term_index, pattern) in patterns.iter().enumerate() {
            let tf = message_match_count(message, pattern);
            if tf == 0 {
                continue;
            }
            match_count += 1;
            let idf =
                (((n - df[term_index]) as f64 + 0.5) / (df[term_index] as f64 + 0.5) + 1.0).ln();
            let k = 1.2;
            let b = 0.75;
            let tfn =
                (tf as f64 * (k + 1.0)) / (tf as f64 + k * (1.0 - b + b * dl / avg_dl.max(1.0)));
            score += idf * tfn;
        }
        if match_count < min_match {
            continue;
        }
        let snippet_re = regex::Regex::new(
            &patterns
                .iter()
                .map(regex::Regex::as_str)
                .collect::<Vec<_>>()
                .join("|"),
        )
        .unwrap();
        scored.push((
            score,
            SearchHit {
                entry: entries[index].clone(),
                snippet: message_snippet(message, &snippet_re),
                match_count,
            },
        ));
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    if scored.len() > 1 && terms.len() >= 2 {
        let top = scored[0].0;
        if top > 0.0 {
            let threshold = top * 0.1;
            if let Some(cut) = scored.iter().position(|(score, _)| *score < threshold) {
                scored.truncate(cut);
            }
        }
    }
    if scored.len() > MAX_SEARCH_RESULTS {
        scored.truncate(MAX_SEARCH_RESULTS);
    }
    scored.into_iter().map(|(_, hit)| hit).collect()
}

fn for_each_search_text(message: &Message, mut visit: impl FnMut(&str)) {
    visit(match message.role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool_result",
        Role::System => "system",
    });
    for block in &message.blocks {
        match block {
            ContentBlock::Text { text }
            | ContentBlock::Thinking { text, .. }
            | ContentBlock::ToolResult { content: text, .. } => visit(text),
            ContentBlock::ToolUse { name, input, .. } => {
                visit(name);
                if let Some(code) = input.get("code").and_then(serde_json::Value::as_str) {
                    visit(code);
                }
            }
        }
    }
}

fn message_matches(message: &Message, mut predicate: impl FnMut(&str) -> bool) -> bool {
    let mut matched = false;
    for_each_search_text(message, |text| matched |= predicate(text));
    matched
}

fn message_match_count(message: &Message, pattern: &regex::Regex) -> usize {
    let mut count = 0;
    for_each_search_text(message, |text| count += pattern.find_iter(text).count());
    count
}

fn message_word_count(message: &Message) -> usize {
    let mut count = 0;
    for_each_search_text(message, |text| count += text.split_whitespace().count());
    count
}

fn message_snippet(message: &Message, pattern: &regex::Regex) -> Option<String> {
    let mut snippet = None;
    for_each_search_text(message, |text| {
        if snippet.is_none() && pattern.is_match(text) {
            snippet = line_snippet(text, pattern);
        }
    });
    snippet
}

fn looks_like_regex(s: &str) -> bool {
    "|*+?{}()[]\\^$.".chars().any(|c| s.contains(c))
}

/// Try the string as a regex; fall back to an escaped literal. Caps the
/// source length to refuse pathological patterns.
fn safe_regex(pattern: &str) -> regex::Regex {
    if pattern.len() > 256 {
        let head: String = pattern.chars().take(64).collect();
        return regex::Regex::new(&regex::escape(&head)).unwrap();
    }
    regex::Regex::new(pattern)
        .unwrap_or_else(|_| regex::Regex::new(&regex::escape(pattern)).unwrap())
}

/// ±2 lines around the first regex match, with elision markers.
///
/// Each included line is bounded independently. Tool outputs are commonly a
/// single JSON line, so line-count context alone is not a size bound: copying
/// one 100 KiB matching line used to make a five-result recall return hundreds
/// of KiB and leave that output capacity retained by the sandbox.
fn line_snippet(text: &str, re: &regex::Regex) -> Option<String> {
    const CONTEXT_LINES: usize = 2;
    const MAX_LINE_CHARS: usize = 500;

    let mut previous: std::collections::VecDeque<(usize, &str)> =
        std::collections::VecDeque::with_capacity(CONTEXT_LINES);
    let mut selected: Vec<(usize, &str)> = Vec::with_capacity(CONTEXT_LINES * 2 + 1);
    let mut hit = None;
    let mut total_lines = 0usize;
    for (index, line) in text.lines().enumerate() {
        total_lines = index + 1;
        if let Some(hit_index) = hit {
            if index <= hit_index + CONTEXT_LINES {
                selected.push((index, line));
            }
            continue;
        }
        if re.is_match(line) {
            hit = Some(index);
            selected.extend(previous.drain(..));
            selected.push((index, line));
        } else {
            if previous.len() == CONTEXT_LINES {
                previous.pop_front();
            }
            previous.push_back((index, line));
        }
    }
    let hit = hit?;
    let mut parts = Vec::with_capacity(selected.len() + 2);
    let first = selected.first().map_or(hit, |(index, _)| *index);
    if first > 0 {
        parts.push(format!("...({first} lines above)"));
    }
    for (index, line) in &selected {
        parts.push(if *index == hit {
            clip_line_around_match(line, re, MAX_LINE_CHARS)
        } else {
            clip(line, MAX_LINE_CHARS)
        });
    }
    let shown_through = selected.last().map_or(hit + 1, |(index, _)| index + 1);
    if shown_through < total_lines {
        parts.push(format!("...({} lines below)", total_lines - shown_through));
    }
    Some(parts.join("\n"))
}

/// Clip one long matching line while keeping the first match in view.
fn clip_line_around_match(line: &str, re: &regex::Regex, max: usize) -> String {
    let Some(found) = re.find(line) else {
        return clip(line, max);
    };
    let total = line.chars().count();
    if total <= max {
        return line.to_string();
    }
    let match_start = line[..found.start()].chars().count();
    let match_len = line[found.start()..found.end()].chars().count().max(1);
    let visible_match = match_len.min(max);
    let side_budget = max.saturating_sub(visible_match);
    let mut start = match_start.saturating_sub(side_budget / 2);
    let mut end = (start + max).min(total);
    if end - start < max {
        start = end.saturating_sub(max);
    }
    end = (start + max).min(total);
    let body: String = line.chars().skip(start).take(end - start).collect();
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        body,
        if end < total { "…" } else { "" }
    )
}

const STOPWORDS: &[&str] = &[
    "the", "a", "an", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "can", "shall", "of",
    "in", "to", "for", "with", "on", "at", "from", "by", "as", "into", "through", "during",
    "before", "after", "above", "below", "between", "out", "off", "over", "under", "again",
    "further", "then", "once", "here", "there", "when", "where", "why", "how", "all", "both",
    "each", "few", "more", "most", "other", "some", "such", "no", "nor", "not", "only", "own",
    "same", "so", "than", "too", "very", "just", "about", "it", "its", "that", "this", "what",
    "which", "who", "whom", "these", "those",
];

fn filter_stopwords<'a>(terms: &[&'a str]) -> Vec<&'a str> {
    let meaningful: Vec<&'a str> = terms
        .iter()
        .copied()
        .filter(|t| t.len() > 1 && !STOPWORDS.contains(&t.to_lowercase().as_str()))
        .collect();
    if meaningful.is_empty() {
        terms.to_vec()
    } else {
        meaningful
    }
}

// ── output formatting ──────────────────────────────────────────────────────

/// Browse mode: one flat block per entry.
fn format_recall_output(
    entries: &[RecallEntry],
    _query: Option<&str>,
    header_override: Option<String>,
) -> String {
    if entries.is_empty() {
        return "No entries in session history.".to_string();
    }
    let header =
        header_override.unwrap_or_else(|| format!("Session history ({} entries):", entries.len()));
    let body = entries
        .iter()
        .map(|e| {
            let file_suffix = if e.files.is_empty() {
                String::new()
            } else {
                format!(" files:[{}]", e.files.join(", "))
            };
            format!("#{} [{}]{} {}", e.index, e.role, file_suffix, e.summary)
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    format!("{header}\n\n{body}")
}

/// Search mode: segment by turn (a segment starts at a user/assistant
/// boundary and runs through its tool calls/results), mark matched entries,
/// and show one segment of context on each side of the first match.
fn format_search_output(hits: &[SearchHit], query: &str, header: &str) -> String {
    if hits.is_empty() {
        return format!("No matches for \"{query}\".");
    }
    let matched_count = hits.len();
    let seg_count = count_segments(hits);
    let prefix = if seg_count > 1 {
        format!("{matched_count} matches across {seg_count} segments")
    } else {
        format!("{matched_count} matches in 1 segment")
    };
    let mut lines = vec![format!("{header} for \"{query}\" — {prefix}")];
    let range = segment_range(hits);
    lines.push(format!("--- {range} ---"));
    for hit in hits {
        let mark = ">";
        let file_suffix = if hit.entry.files.is_empty() {
            String::new()
        } else {
            format!(" files:[{}]", hit.entry.files.join(", "))
        };
        let body = hit
            .snippet
            .clone()
            .unwrap_or_else(|| hit.entry.summary.clone());
        lines.push(format!(
            "{mark} #{} [{}]{} ({} term{}) {}",
            hit.entry.index,
            hit.entry.role,
            file_suffix,
            hit.match_count,
            if hit.match_count == 1 { "" } else { "s" },
            body
        ));
    }
    lines.join("\n")
}

/// A segment spans from the first hit to the last contiguous run sharing a
/// turn. For a single page of hits this is just `#first-#last`.
fn count_segments(hits: &[SearchHit]) -> usize {
    if hits.is_empty() {
        return 0;
    }
    // Pages are small (<=5); treat each user/assistant role change as a
    // boundary and count the resulting runs.
    let mut segs = 1;
    for w in hits.windows(2) {
        let prev = &w[0].entry.role;
        let cur = &w[1].entry.role;
        if is_segment_boundary(prev, cur) {
            segs += 1;
        }
    }
    segs
}

fn is_segment_boundary(prev: &str, cur: &str) -> bool {
    matches!(cur, "user" | "assistant") && !matches!(prev, "tool_result")
        || (cur == "user" && prev != "user")
}

fn segment_range(hits: &[SearchHit]) -> String {
    let first = hits[0].entry.index;
    let last = hits[hits.len() - 1].entry.index;
    if first == last {
        format!("#{first}")
    } else {
        format!("#{first}-#{last}")
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Clip a string to `max` chars on a char boundary, appending `…`.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn ev(id: &str, kind: SessionEventKind) -> SessionEvent {
        SessionEvent {
            id: id.to_string(),
            parent_id: None,
            kind,
        }
    }
    fn user(id: &str, text: &str) -> SessionEvent {
        ev(
            id,
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            }),
        )
    }
    fn assistant(id: &str, text: &str) -> SessionEvent {
        ev(
            id,
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            }),
        )
    }

    #[test]
    fn browse_returns_recent_entries_flat() {
        let events: Vec<SessionEvent> = (0..30)
            .map(|i| user(&format!("u{i}"), &format!("prompt {i}")))
            .collect();
        let out = recall(
            &events,
            &RecallRequest {
                scope: RecallScope::All,
                ..Default::default()
            },
        );
        // Last 25 entries.
        assert!(out.text.contains("#5 [user]"));
        assert!(out.text.contains("#29 [user]"));
        assert!(!out.text.contains("#4 [user]"));
        assert_eq!(out.status, "25 entries");
    }

    #[test]
    fn search_finds_matching_messages() {
        let events = vec![
            user("a", "fix the login bug"),
            assistant("b", "I will look at the auth module"),
            user("c", "now refactor the tests"),
            assistant("d", "done with the refactor"),
        ];
        let req = RecallRequest {
            query: Some("refactor".to_string()),
            scope: RecallScope::All,
            ..Default::default()
        };
        let out = recall(&events, &req);
        assert!(out.text.contains("#2 [user]"), "matched user: {}", out.text);
        assert!(
            out.text.contains("#3 [assistant]"),
            "matched assistant: {}",
            out.text
        );
        assert!(out.status.contains("matches"));
    }

    #[test]
    fn regex_query_matches_pattern() {
        let events = vec![
            user("a", "error: ENOENT"),
            assistant("b", "file not found"),
            user("c", "error: timeout"),
        ];
        let req = RecallRequest {
            query: Some("ENOENT|timeout".to_string()),
            scope: RecallScope::All,
            ..Default::default()
        };
        let out = recall(&events, &req);
        assert!(out.text.contains("#0 [user]"));
        assert!(out.text.contains("#2 [user]"));
        assert!(!out.text.contains("#1 [assistant]"));
    }

    #[test]
    fn compaction_scope_resolves_summarized_range() {
        // Four messages, a compaction marker folding the first two into a
        // summary, keeping the last two. summarized_range = [a, b].
        let events = vec![
            user("a", "old prompt one"),
            assistant("b", "old reply one"),
            user("c", "kept prompt"),
            assistant("d", "kept reply"),
            ev(
                "cmp",
                SessionEventKind::Compaction {
                    summary: "SUMMARY".to_string(),
                    first_kept_entry_id: "c".to_string(),
                    summarized_range: ["a".to_string(), "b".to_string()],
                    checkpointed_tail: false,
                    summarized: 2,
                    represented: 2,
                    kept: 2,
                },
            ),
        ];
        let req = RecallRequest {
            query: Some("old".to_string()),
            scope: RecallScope::Compaction(CompactionTarget::Latest),
            ..Default::default()
        };
        let out = recall(&events, &req);
        assert!(
            out.text.contains("#0 [user]"),
            "folded message visible via recall: {}",
            out.text
        );
        assert!(out.text.contains("#1 [assistant]"));
        // Kept messages are outside the compaction range.
        assert!(!out.text.contains("#2 [user]"));
    }

    #[test]
    fn search_snippet_bounds_a_huge_single_line_around_match() {
        let text = format!("{}needle{}", "a".repeat(20_000), "z".repeat(20_000));
        let events = vec![user("a", &text)];
        let out = recall(
            &events,
            &RecallRequest {
                query: Some("needle".to_string()),
                scope: RecallScope::All,
                ..Default::default()
            },
        );
        assert!(out.text.contains("needle"), "{}", out.text);
        assert!(
            out.text.len() < 1_000,
            "snippet was {} bytes",
            out.text.len()
        );
        assert!(out.text.contains('…'));
    }

    #[test]
    fn search_snippet_bounds_neighbor_context_lines() {
        let text = format!("{}\nneedle\n{}", "a".repeat(20_000), "z".repeat(20_000));
        let events = vec![user("a", &text)];
        let out = recall(
            &events,
            &RecallRequest {
                query: Some("needle".to_string()),
                scope: RecallScope::All,
                ..Default::default()
            },
        );
        assert!(out.text.contains("needle"));
        assert!(
            out.text.len() < 1_500,
            "snippet was {} bytes",
            out.text.len()
        );
    }

    #[test]
    fn expand_returns_full_content() {
        let long = "x".repeat(500);
        let events = vec![user("a", &long)];
        let req = RecallRequest {
            scope: RecallScope::All,
            expand: vec![0],
            ..Default::default()
        };
        let out = recall(&events, &req);
        // Full content (no ellipsis clip at 300).
        assert!(!out.text.contains('…'));
        assert!(out.text.contains(&long));
    }
}
