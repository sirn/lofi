//! API reference content and search for the `lofi` guest surface.
//!
//! The full reference is embedded at compile time from `docs/api.md`.
//! [`docs_index`] returns a compact name/summary list; [`docs_entry`] returns
//! the full text for one entry; [`docs_search`] does keyword search with name
//! matches weighted above body matches.

/// One entry in the API reference, parsed from a `##` section.
struct DocEntry {
    /// The canonical name, e.g. `lofi.read` (first dotted token of the
    /// header, without arguments or parentheses).
    name: String,
    /// The full header line after `## `.
    header: String,
    /// The body text (everything after the header line and its blank line).
    body: String,
    /// The first paragraph of the body, used as a one-line summary.
    summary: String,
}

/// The raw embedded markdown.
pub const DOCS_MD: &str = include_str!("docs/api.md");

/// Parse the embedded markdown into entries. Each `## ` heading starts a new
/// entry. The preamble (text before the first `## `) is skipped.
fn parse_entries() -> Vec<DocEntry> {
    let mut entries = Vec::new();
    let mut current_name = String::new();
    let mut current_header = String::new();
    let mut current_body = String::new();
    let mut in_entry = false;

    for line in DOCS_MD.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            if in_entry {
                entries.push(finish_entry(
                    std::mem::take(&mut current_name),
                    std::mem::take(&mut current_header),
                    std::mem::take(&mut current_body),
                ));
            }
            current_header = rest.to_string();
            current_name = extract_name(rest);
            current_body.clear();
            in_entry = true;
        } else if in_entry {
            current_body.push_str(line);
            current_body.push('\n');
        }
    }
    if in_entry {
        entries.push(finish_entry(current_name, current_header, current_body));
    }
    entries
}

/// Build a [`DocEntry`], extracting the summary from the first non-empty,
/// non-heading body paragraph.
fn finish_entry(name: String, header: String, body: String) -> DocEntry {
    let summary = body
        .lines()
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
        .to_string();
    DocEntry {
        name,
        header,
        body,
        summary,
    }
}

/// Extract the canonical name from a header line.
///
/// `lofi.read(path, opts?)` → `lofi.read`, `Truncated results and filtering`
/// → `Truncated results and filtering`.
fn extract_name(header: &str) -> String {
    if let Some(paren) = header.find('(') {
        header[..paren].trim().to_string()
    } else {
        header.trim().to_string()
    }
}

/// All parsed entries, each as (canonical name, header line, summary).
///
/// Exposed so other modules (e.g. the compaction hook) can use the docs
/// registry as the single source of truth for tool names and descriptions
/// instead of maintaining a parallel hard-coded table.
#[must_use]
pub fn entries() -> Vec<(String, String, String)> {
    parse_entries()
        .into_iter()
        .map(|e| (e.name, e.header, e.summary))
        .collect()
}

/// Compact index of all entries: `[{ name, summary }]`.
#[must_use]
pub fn docs_index() -> serde_json::Value {
    let entries = parse_entries();
    let arr: Vec<serde_json::Value> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name,
                "summary": e.summary,
            })
        })
        .collect();
    serde_json::json!({ "ok": true, "entries": arr })
}

/// Full text for one entry, looked up by canonical name (case-insensitive).
/// Returns `{ ok: true, name, content }` or `{ ok: false, error }`.
#[must_use]
pub fn docs_entry(name: &str) -> serde_json::Value {
    let entries = parse_entries();
    let needle = name.to_ascii_lowercase();
    for e in &entries {
        if e.name.to_ascii_lowercase() == needle {
            let content = format!("## {}\n\n{}", e.header, e.body);
            return serde_json::json!({
                "ok": true,
                "name": e.name,
                "content": content,
            });
        }
    }
    serde_json::json!({
        "ok": false,
        "error": format!("no doc entry named '{name}'"),
    })
}

/// Keyword search across entry names and bodies. Name matches score 3x
/// header matches score 2x body matches. Results are sorted by score
/// descending and limited to the top 10.
#[must_use]
pub fn docs_search(query: &str) -> serde_json::Value {
    let entries = parse_entries();
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    if terms.is_empty() {
        return serde_json::json!({ "ok": true, "results": [] });
    }

    let mut scored: Vec<(usize, &DocEntry)> = Vec::new();
    for e in &entries {
        let name_lo = e.name.to_ascii_lowercase();
        let header_lo = e.header.to_ascii_lowercase();
        let body_lo = e.body.to_ascii_lowercase();
        let mut score = 0usize;
        for term in &terms {
            if name_lo.contains(term) {
                score += 3;
            }
            if header_lo.contains(term) {
                score += 2;
            }
            if body_lo.contains(term) {
                score += 1;
            }
        }
        if score > 0 {
            scored.push((score, e));
        }
    }
    scored.sort_by_key(|&(score, _)| std::cmp::Reverse(score));

    let results: Vec<serde_json::Value> = scored
        .iter()
        .take(10)
        .map(|(score, e)| {
            let excerpt = e
                .body
                .lines()
                .find(|l| {
                    let lo = l.to_ascii_lowercase();
                    terms.iter().any(|t| lo.contains(t))
                })
                .unwrap_or(&e.summary)
                .trim()
                .to_string();
            serde_json::json!({
                "name": e.name,
                "score": score,
                "excerpt": excerpt,
            })
        })
        .collect();
    serde_json::json!({ "ok": true, "results": results })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn index_has_entries() {
        let idx = docs_index();
        let entries = idx["entries"].as_array().unwrap();
        assert!(!entries.is_empty(), "index should not be empty");
        // Core APIs should be present.
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"lofi.read"));
        assert!(names.contains(&"lofi.bash"));
        assert!(names.contains(&"lofi.agent"));
        assert!(names.contains(&"lofi.models"));
    }

    #[test]
    fn entry_returns_full_text() {
        let entry = docs_entry("lofi.read");
        assert_eq!(entry["ok"], true);
        assert_eq!(entry["name"], "lofi.read");
        let content = entry["content"].as_str().unwrap();
        assert!(content.contains("path"));
        assert!(content.contains("offset"));
    }

    #[test]
    fn entry_case_insensitive() {
        let entry = docs_entry("LOFI.READ");
        assert_eq!(entry["ok"], true);
    }

    #[test]
    fn entry_not_found() {
        let entry = docs_entry("lofi.nonexistent");
        assert_eq!(entry["ok"], false);
        assert!(entry["error"].as_str().unwrap().contains("nonexistent"));
    }

    #[test]
    fn search_finds_by_name() {
        let res = docs_search("bash");
        let results = res["results"].as_array().unwrap();
        assert!(!results.is_empty());
        // lofi.bash should be the top hit.
        assert_eq!(results[0]["name"], "lofi.bash");
    }

    #[test]
    fn search_finds_by_concept() {
        let res = docs_search("truncated filtering");
        let results = res["results"].as_array().unwrap();
        assert!(!results.is_empty());
        let names: Vec<&str> = results
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"Truncated results and filtering"));
    }

    #[test]
    fn search_empty_query_returns_empty() {
        let res = docs_search("");
        assert_eq!(res["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn search_multi_term_scores_higher() {
        // "read file" should match lofi.read (both terms in name/body) and
        // lofi.write ("file" in body), but lofi.read should score higher.
        let res = docs_search("read file");
        let results = res["results"].as_array().unwrap();
        assert!(!results.is_empty());
        assert_eq!(results[0]["name"], "lofi.read");
    }

    #[test]
    fn search_limited_to_ten() {
        // A very common term should not return more than 10 results.
        let res = docs_search("lofi");
        let results = res["results"].as_array().unwrap();
        assert!(results.len() <= 10);
    }
}
