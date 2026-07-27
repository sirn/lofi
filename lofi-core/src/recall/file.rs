use std::collections::{HashMap, HashSet};
use std::path::Path;

use lofi_error::Result;
use lofi_types::{NativeToolRecord, SessionEventKind};
use serde::Deserialize;

use super::{
    filter_stopwords, format_recall_output, format_search_output, looks_like_regex,
    message_match_count, message_matches, message_snippet, message_word_count, render_message,
    safe_regex, store, CompactionTarget, RecallEntry, RecallOutcome, RecallRequest, RecallScope,
    SearchHit, DEFAULT_RECENT, MAX_PAGES, MAX_SEARCH_RESULTS, PAGE_SIZE,
};

#[derive(Default)]
struct FileScope {
    allowed: Option<HashSet<String>>,
}

#[derive(Deserialize)]
struct NativeSidecar {
    parent: String,
    name: String,
    args: String,
}

/// Run recall over an on-disk transcript while retaining only one parsed
/// event at a time plus clipped entries / search statistics.
#[must_use]
pub fn recall_file(path: &Path, req: &RecallRequest) -> RecallOutcome {
    recall_file_inner(path, req).unwrap_or_else(|_| RecallOutcome {
        text: "recall: session file unreadable.".to_string(),
        status: "error".to_string(),
    })
}

fn recall_file_inner(path: &Path, req: &RecallRequest) -> Result<RecallOutcome> {
    let cursor = store::SessionCursor::open(path.to_path_buf())?;
    let snapshot = cursor.tree_snapshot()?;
    let index = snapshot.index;
    let scope = resolve_scope(path, &index, snapshot.leaf_id.as_deref(), &req.scope)?;
    let selected: Vec<(u64, usize)> = index
        .iter()
        .scan(0usize, |global, event| {
            if !is_message(event.kind) {
                return Some(None);
            }
            let index = *global;
            *global += 1;
            Some(
                scope
                    .allowed
                    .as_ref()
                    .is_none_or(|ids| ids.contains(&event.id))
                    .then_some((event.offset, index)),
            )
        })
        .flatten()
        .collect();
    let offsets: Vec<u64> = selected.iter().map(|(offset, _)| *offset).collect();
    let globals: Vec<usize> = selected.iter().map(|(_, index)| *index).collect();

    // Native-tool result bodies account for roughly half of large transcript
    // files, but recall only needs their parent/name/args for assistant labels
    // and file annotations. Deserialize that borrowed sidecar shape so the
    // large result field is skipped rather than retained.
    let native_offsets: Vec<u64> = index
        .iter()
        .filter(|event| event.kind == store::IndexKind::NativeTool)
        .filter(|event| {
            scope
                .allowed
                .as_ref()
                .is_none_or(|ids| ids.contains(&event.id))
        })
        .map(|event| event.offset)
        .collect();
    let mut native_records = Vec::with_capacity(native_offsets.len());
    store::visit_event_lines(path, &native_offsets, |line| {
        let sidecar: NativeSidecar = serde_json::from_str(line)
            .map_err(|error| lofi_error::Error::State(format!("parse native sidecar: {error}")))?;
        native_records.push(NativeToolRecord {
            parent: sidecar.parent,
            call_id: 0,
            name: sidecar.name,
            args: sidecar.args,
            result: String::new(),
            is_error: false,
        });
        Ok(())
    })?;
    let mut native_by_parent: HashMap<String, Vec<&NativeToolRecord>> = HashMap::new();
    for record in &native_records {
        native_by_parent
            .entry(record.parent.clone())
            .or_default()
            .push(record);
    }

    let mut entries = Vec::with_capacity(offsets.len());
    let mut position = 0usize;
    store::visit_event_lines(path, &offsets, |line| {
        let event = store::parse_event(line)?;
        if let SessionEventKind::Message(message) = event.kind {
            entries.push(render_message(
                &message,
                globals[position],
                false,
                &native_by_parent,
            ));
            position += 1;
        }
        Ok(())
    })?;

    let query = req.query.as_deref().unwrap_or("").trim();
    if query.is_empty() {
        if req.expand.is_empty() {
            let start = entries.len().saturating_sub(DEFAULT_RECENT);
            let recent = &entries[start..];
            return Ok(RecallOutcome {
                text: format_recall_output(
                    recent,
                    None,
                    matches!(req.scope, RecallScope::All).then(|| "Scope: all".to_string()),
                ),
                status: format!("{} entries", recent.len()),
            });
        }
        return expand(path, &offsets, &globals, req, &native_by_parent);
    }

    search(
        path,
        &offsets,
        &globals,
        &entries,
        query,
        req,
        &native_by_parent,
    )
}

fn expand(
    path: &Path,
    offsets: &[u64],
    globals: &[usize],
    req: &RecallRequest,
    native_by_parent: &HashMap<String, Vec<&NativeToolRecord>>,
) -> Result<RecallOutcome> {
    let wanted: HashSet<usize> = req.expand.iter().copied().collect();
    let mut expanded = HashMap::new();
    let mut position = 0usize;
    store::visit_event_lines(path, offsets, |line| {
        let event = store::parse_event(line)?;
        if let SessionEventKind::Message(message) = event.kind {
            let global = globals[position];
            if wanted.contains(&global) {
                expanded.insert(
                    global,
                    render_message(&message, global, true, native_by_parent),
                );
            }
            position += 1;
        }
        Ok(())
    })?;
    let invalid: Vec<usize> = req
        .expand
        .iter()
        .copied()
        .filter(|i| !expanded.contains_key(i))
        .collect();
    if !invalid.is_empty() {
        return Ok(RecallOutcome {
            text: format!(
                "Cannot expand indices outside selected scope: {}",
                invalid
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            status: format!("{} invalid", invalid.len()),
        });
    }
    let ordered: Vec<RecallEntry> = req
        .expand
        .iter()
        .filter_map(|i| expanded.get(i).cloned())
        .collect();
    Ok(RecallOutcome {
        text: format_recall_output(&ordered, None, None),
        status: format!("expanded {}", ordered.len()),
    })
}

#[allow(clippy::too_many_lines)]
fn search(
    path: &Path,
    offsets: &[u64],
    globals: &[usize],
    entries: &[RecallEntry],
    query: &str,
    req: &RecallRequest,
    native_by_parent: &HashMap<String, Vec<&NativeToolRecord>>,
) -> Result<RecallOutcome> {
    let raw_query = query.trim();
    let mut hits = if looks_like_regex(raw_query) {
        let pattern = safe_regex(raw_query);
        let mut hits = Vec::new();
        let mut position = 0usize;
        store::visit_event_lines(path, offsets, |line| {
            let event = store::parse_event(line)?;
            if let SessionEventKind::Message(message) = event.kind {
                if hits.len() < MAX_SEARCH_RESULTS
                    && message_matches(&message, |text| pattern.is_match(text))
                {
                    hits.push(SearchHit {
                        entry: entries[position].clone(),
                        snippet: message_snippet(&message, &pattern),
                        match_count: 1,
                    });
                }
                position += 1;
            }
            Ok(())
        })?;
        hits
    } else {
        let raw_terms: Vec<&str> = raw_query.split_whitespace().collect();
        let terms = filter_stopwords(&raw_terms);
        if terms.is_empty() {
            Vec::new()
        } else {
            let patterns: Vec<regex::Regex> = terms
                .iter()
                .map(|term| regex::Regex::new(&regex::escape(term)).unwrap())
                .collect();
            let mut lengths = Vec::with_capacity(offsets.len());
            let mut df = vec![0usize; terms.len()];
            store::visit_event_lines(path, offsets, |line| {
                let event = store::parse_event(line)?;
                if let SessionEventKind::Message(message) = event.kind {
                    lengths.push(message_word_count(&message));
                    for (i, pattern) in patterns.iter().enumerate() {
                        if message_matches(&message, |text| pattern.is_match(text)) {
                            df[i] += 1;
                        }
                    }
                }
                Ok(())
            })?;
            let n = lengths.len();
            let avg_dl = lengths.iter().sum::<usize>() as f64 / n.max(1) as f64;
            let min_match = if terms.len() >= 3 { 2 } else { 1 };
            let snippet_pattern = regex::Regex::new(
                &patterns
                    .iter()
                    .map(regex::Regex::as_str)
                    .collect::<Vec<_>>()
                    .join("|"),
            )
            .unwrap();
            let mut scored = Vec::new();
            let mut position = 0usize;
            store::visit_event_lines(path, offsets, |line| {
                let event = store::parse_event(line)?;
                if let SessionEventKind::Message(message) = event.kind {
                    let mut match_count = 0;
                    let mut score = 0.0;
                    let dl = lengths[position] as f64;
                    for (term_index, pattern) in patterns.iter().enumerate() {
                        let tf = message_match_count(&message, pattern);
                        if tf == 0 {
                            continue;
                        }
                        match_count += 1;
                        let idf = (((n - df[term_index]) as f64 + 0.5)
                            / (df[term_index] as f64 + 0.5)
                            + 1.0)
                            .ln();
                        let k = 1.2;
                        let b = 0.75;
                        let tfn = (tf as f64 * (k + 1.0))
                            / (tf as f64 + k * (1.0 - b + b * dl / avg_dl.max(1.0)));
                        score += idf * tfn;
                    }
                    if match_count >= min_match {
                        scored.push((
                            score,
                            SearchHit {
                                entry: entries[position].clone(),
                                snippet: message_snippet(&message, &snippet_pattern),
                                match_count,
                            },
                        ));
                    }
                    position += 1;
                }
                Ok(())
            })?;
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
            scored.truncate(MAX_SEARCH_RESULTS.min(scored.len()));
            scored.into_iter().map(|(_, hit)| hit).collect()
        }
    };

    let total = hits.len();
    if total == 0 {
        return Ok(RecallOutcome {
            text: format!("No matches for \"{query}\" in selected scope."),
            status: "0 matches".to_string(),
        });
    }
    let total_pages = total.div_ceil(PAGE_SIZE).max(1);
    let page = req.page.max(1);
    if page > MAX_PAGES.min(total_pages) {
        return Ok(RecallOutcome {
            text: format!("Too many results to page through ({total} matches across {total_pages} pages). Try a more specific query or scope."),
            status: format!("{total} matches"),
        });
    }
    let start = (page - 1) * PAGE_SIZE;
    hits = hits[start..(start + PAGE_SIZE).min(total)].to_vec();
    if !req.expand.is_empty() {
        let positions: HashMap<usize, usize> = globals
            .iter()
            .enumerate()
            .map(|(position, global)| (*global, position))
            .collect();
        for hit in &mut hits {
            if !req.expand.contains(&hit.entry.index) {
                continue;
            }
            let Some(&position) = positions.get(&hit.entry.index) else {
                continue;
            };
            let event = store::load_event_at(path, offsets[position])?;
            if let SessionEventKind::Message(message) = event.kind {
                hit.entry = render_message(&message, hit.entry.index, true, native_by_parent);
                hit.snippet = Some(hit.entry.summary.clone());
            }
        }
    }
    let header = if total_pages > 1 {
        format!("Page {page}/{total_pages} ({total} total matches)")
    } else {
        format!("{total} matches")
    };
    Ok(RecallOutcome {
        text: format_search_output(&hits, query, &header),
        status: format!("{total} matches"),
    })
}

fn is_message(kind: store::IndexKind) -> bool {
    matches!(
        kind,
        store::IndexKind::UserPrompt
            | store::IndexKind::AssistantMessage
            | store::IndexKind::ToolResult
            | store::IndexKind::SystemMessage
    )
}

fn resolve_scope(
    path: &Path,
    index: &[store::EventIndex],
    leaf_id: Option<&str>,
    scope: &RecallScope,
) -> Result<FileScope> {
    if matches!(scope, RecallScope::All) {
        return Ok(FileScope::default());
    }
    let by_id: HashMap<&str, usize> = index
        .iter()
        .enumerate()
        .map(|(i, event)| (event.id.as_str(), i))
        .collect();
    let mut current = leaf_id.and_then(|id| by_id.get(id).copied());
    let mut lineage = Vec::new();
    while let Some(i) = current {
        lineage.push(i);
        current = index[i]
            .parent_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
    }
    lineage.reverse();
    if matches!(scope, RecallScope::Lineage) {
        return Ok(FileScope {
            allowed: Some(lineage.iter().map(|&i| index[i].id.clone()).collect()),
        });
    }
    let compactions: Vec<usize> = lineage
        .iter()
        .copied()
        .filter(|&i| index[i].kind == store::IndexKind::Compaction)
        .collect();
    let RecallScope::Compaction(target) = scope else {
        unreachable!()
    };
    let selected = match target {
        CompactionTarget::Latest => compactions.last().copied(),
        CompactionTarget::Index(n) => compactions.get(*n).copied(),
    };
    let Some(selected) = selected else {
        return Ok(FileScope {
            allowed: Some(lineage.iter().map(|&i| index[i].id.clone()).collect()),
        });
    };
    let marker = store::load_event_at(path, index[selected].offset)?;
    let SessionEventKind::Compaction {
        summarized_range, ..
    } = marker.kind
    else {
        unreachable!()
    };
    let lineage_ids: HashSet<&str> = lineage.iter().map(|&i| index[i].id.as_str()).collect();
    let first = index
        .iter()
        .position(|event| event.id == summarized_range[0]);
    let last = index
        .iter()
        .position(|event| event.id == summarized_range[1]);
    let allowed = match (first, last) {
        (Some(first), Some(last)) if first <= last => index[first..=last]
            .iter()
            .filter(|event| lineage_ids.contains(event.id.as_str()))
            .map(|event| event.id.clone())
            .collect(),
        _ => HashSet::new(),
    };
    Ok(FileScope {
        allowed: Some(allowed),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use lofi_types::{ContentBlock, Message, Role, SessionEvent};

    use super::*;

    fn transcript(events: &[SessionEvent]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "type": "meta",
                "version": 2,
                "created": 0,
                "cwd": "/tmp",
                "model": {"provider":"p", "id":"m", "thinking":"off"}
            })
        )
        .unwrap();
        for event in events {
            writeln!(file, "{}", serde_json::to_string(event).unwrap()).unwrap();
        }
        file
    }

    fn message(id: &str, parent: Option<&str>, role: Role, text: &str) -> SessionEvent {
        SessionEvent {
            id: id.to_string(),
            parent_id: parent.map(str::to_string),
            kind: SessionEventKind::Message(Message {
                role,
                blocks: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            }),
        }
    }

    #[test]
    fn file_search_preserves_global_indices_and_scope() {
        let events = vec![
            message("a", None, Role::User, "root needle"),
            message("b", Some("a"), Role::Assistant, "active needle"),
            message("x", Some("a"), Role::Assistant, "branch needle"),
            message("c", Some("b"), Role::User, "leaf"),
        ];
        let file = transcript(&events);
        let outcome = recall_file(
            file.path(),
            &RecallRequest {
                query: Some("needle".to_string()),
                scope: RecallScope::Lineage,
                ..Default::default()
            },
        );
        assert!(outcome.text.contains("#0 [user]"), "{}", outcome.text);
        assert!(outcome.text.contains("#1 [assistant]"), "{}", outcome.text);
        assert!(!outcome.text.contains("#2 [assistant]"), "{}", outcome.text);
    }

    #[test]
    fn file_expand_reads_full_selected_message() {
        let long = "x".repeat(500);
        let file = transcript(&[message("a", None, Role::User, &long)]);
        let outcome = recall_file(
            file.path(),
            &RecallRequest {
                scope: RecallScope::All,
                expand: vec![0],
                ..Default::default()
            },
        );
        assert!(outcome.text.contains(&long));
        assert!(!outcome.text.contains('…'));
    }
}
