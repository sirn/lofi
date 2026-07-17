//! Compaction hook for lofi-code.
//!
//! This hook implements all lofi-code-specific knowledge that the compaction
//! system in lofi-core needs: tool API descriptions, file-change tracking,
//! commit extraction, and tool-result compression. By routing through the
//! `CompactionHook` trait, lofi-core remains agnostic of lofi-code's tool
//! vocabulary and result formats.

use std::collections::HashSet;

use lofi_types::{CompactBlock, CompactionHook, SummarySection};

use crate::docs;

// ── tool vocabulary ───────────────────────────────────────────────────────

/// Categorize a native tool name by its file-system effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileEffect {
    /// Creates or overwrites a file.
    Create,
    /// Modifies an existing file in place.
    Modify,
    /// Reads file contents.
    Read,
}

/// Look up the file-system effect of a native tool from the docs registry.
///
/// Tools whose docs summary mentions "write" or "create" are classified as
/// `Create`; "edit" or "replace" as `Modify`; "read" or "list" or "search"
/// as `Read`. Unknown tools return `None`.
fn file_effect(tool_name: &str) -> Option<FileEffect> {
    let entries = docs::entries();
    let entry = entries
        .iter()
        .find(|(name, _, _)| name == tool_name || name.ends_with(&format!(".{tool_name}")))?;
    let lower = entry.2.to_ascii_lowercase();
    if lower.contains("write") || lower.contains("create") {
        Some(FileEffect::Create)
    } else if lower.contains("edit") || lower.contains("replace") {
        Some(FileEffect::Modify)
    } else if lower.contains("read") || lower.contains("list") || lower.contains("search") {
        Some(FileEffect::Read)
    } else {
        None
    }
}

/// A short description for a tool, looked up from the docs registry.
///
/// Returns the docs header line (e.g. `lofi.read(path, opts?)`) prefixed
/// with a dash. Falls back to `lofi.{name}({args})` when the tool is not
/// in the registry.
fn tool_description(name: &str, args: &str) -> String {
    let entries = docs::entries();
    if let Some((_, header, summary)) = entries
        .iter()
        .find(|(n, _, _)| n == name || n.ends_with(&format!(".{name}")))
    {
        format!("{header} — {summary}")
    } else {
        format!("lofi.{name}({args})")
    }
}

// ── the hook ──────────────────────────────────────────────────────────────

/// The compaction hook for lofi-code.
#[derive(Default)]
pub struct CodeCompactionHook;

impl CompactionHook for CodeCompactionHook {
    fn sections(&self, blocks: &[CompactBlock]) -> Vec<SummarySection> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut items: Vec<String> = Vec::new();

        // The exec tool itself is always used (it's the only LLM-facing tool).
        items.push(
            "exec({code, strings?, display?}) — the TypeScript sandbox tool. Call lofi.* methods inside it."
                .to_string(),
        );
        seen.insert("exec".to_string());

        for b in blocks {
            let CompactBlock::ToolCall { native, .. } = b else {
                continue;
            };
            for rec in native {
                if seen.insert(rec.name.clone()) {
                    items.push(tool_description(&rec.name, &rec.args));
                }
            }
        }

        if items.is_empty() {
            Vec::new()
        } else {
            vec![SummarySection {
                title: "APIs Used".to_string(),
                items,
            }]
        }
    }

    fn file_changes(&self, blocks: &[CompactBlock]) -> Vec<String> {
        let mut modified: HashSet<String> = HashSet::new();
        let mut created: HashSet<String> = HashSet::new();
        let mut read: HashSet<String> = HashSet::new();

        for b in blocks {
            let CompactBlock::ToolCall { native, .. } = b else {
                continue;
            };
            for rec in native {
                if rec.is_error || rec.args.is_empty() {
                    continue;
                }
                match file_effect(&rec.name) {
                    Some(FileEffect::Modify) => {
                        modified.insert(rec.args.clone());
                    }
                    Some(FileEffect::Create) => {
                        created.insert(rec.args.clone());
                    }
                    Some(FileEffect::Read) => {
                        read.insert(rec.args.clone());
                    }
                    None => {}
                }
            }
        }

        // Files that were both created and later modified are just Modified.
        for p in &modified {
            created.remove(p);
        }

        let cap = |set: &HashSet<String>, limit: usize| -> String {
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

    fn commits(&self, blocks: &[CompactBlock]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();

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

    fn compress_tool_result(&self, text: &str, max: usize) -> Option<String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Some(String::new());
        }

        // Single-line results: just clip.
        if !trimmed.contains('\n') {
            return Some(clip(trimmed, max));
        }

        // Try JSON parsing for structured tool output (bash results, etc.).
        if trimmed.starts_with('{') {
            if let Some(summary) = compress_json_result(trimmed, max) {
                return Some(summary);
            }
        }

        // For multi-line text, take up to 3 non-empty lines and join with " | ".
        let lines: Vec<&str> = trimmed
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .take(3)
            .collect();
        if lines.is_empty() {
            return Some(String::new());
        }
        let joined = lines.join(" | ");
        Some(clip(&joined, max))
    }

    fn full_output_path(&self, text: &str) -> Option<String> {
        let rest = text.split("Full output: ").nth(1)?;
        let path = rest.split(". Use lofi.read").next()?.trim();
        if path.starts_with('/') {
            Some(path.to_string())
        } else {
            None
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

/// Clip text to max chars on a word boundary.
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

/// Extract the -m "message" (or -m 'message') from a git commit command.
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
    let re = regex::Regex::new(r"\b[0-9a-f]{7,12}\b").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
}

/// Extract a compact summary from a JSON tool result object.
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

/// Render a JSON value as a brief string.
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
