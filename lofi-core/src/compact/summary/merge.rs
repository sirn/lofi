#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) const SECTION_HEADERS: &[&str] = &[
    "Session Goal",
    "User Preferences",
    "Files And Changes",
    "Commits",
    "APIs Used",
    "Skills",
    "Outstanding Context",
];

pub(super) fn merge_previous(prev: &str, fresh: &str) -> String {
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

pub(super) fn format_handoff(body: &str) -> String {
    format!("{HANDOFF_PREAMBLE}\n\n{RESULT_RECALL_INSTRUCTION}\n\n{body}")
}

pub(super) fn strip_preamble(text: &str) -> String {
    let rest = text
        .strip_prefix(HANDOFF_PREAMBLE)
        .map_or(text, |rest| rest.trim_start_matches('\n'));
    rest.strip_prefix(RESULT_RECALL_INSTRUCTION)
        .unwrap_or(rest)
        .trim_start_matches('\n')
        .to_string()
}

pub(super) fn split_headers_brief(body: &str) -> (String, String) {
    match body.split_once(SEPARATOR) {
        Some((h, b)) => (h.trim().to_string(), b.trim().to_string()),
        None => (body.trim().to_string(), String::new()),
    }
}

pub(super) fn section_of(headers: &str, name: &str) -> String {
    let tag = format!("[{name}]");
    let start = headers.find(&tag).map(|i| i + tag.len());
    let Some(start) = start else {
        return String::new();
    };
    let rest = &headers[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

pub(super) fn extra_section_names(
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

pub(super) fn merge_section(name: &str, prev: &str, fresh: &str) -> String {
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

pub(super) fn merge_files(prev: &str, fresh: &str) -> String {
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

pub(super) fn split_paths(rest: &str) -> Vec<String> {
    let no_recall = rest.split("+recall:").next().unwrap_or(rest);
    no_recall
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

pub(super) fn first_line(text: &str, max: usize) -> String {
    let line = text.split('\n').next().unwrap_or("").trim();
    clip(line, max)
}
