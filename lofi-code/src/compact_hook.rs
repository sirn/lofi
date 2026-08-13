use std::collections::HashSet;

use lofi_types::{CompactBlock, CompactionHook, NativeToolRecord, SummarySection};

use crate::docs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileEffect {
    Create,
    Modify,
    Read,
}

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

fn native_records(blocks: &[CompactBlock]) -> impl Iterator<Item = &NativeToolRecord> {
    blocks.iter().flat_map(CompactBlock::native_records)
}

fn loaded_skills(blocks: &[CompactBlock]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut items: Vec<String> = Vec::new();
    for rec in native_records(blocks) {
        if rec.name != "skill" || rec.args.is_empty() || rec.is_error {
            continue;
        }
        if !seen.insert(rec.args.clone()) {
            continue;
        }
        let desc = skill_blurb(&rec.result);
        if desc.is_empty() {
            items.push(rec.args.clone());
        } else {
            items.push(format!("{} — {desc}", rec.args));
        }
    }
    items
}

fn skill_blurb(result: &str) -> String {
    let parsed = serde_json::from_str::<serde_json::Value>(result).ok();
    let content = parsed
        .as_ref()
        .and_then(|v| v.get("content").and_then(|c| c.as_str()))
        .unwrap_or(result);
    let desc = yaml_description(content).unwrap_or_else(|| first_prose_line(content));
    clip(&desc, 80)
}

fn yaml_description(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let (front, _) = rest.split_once("\n---")?;
    for line in front.lines() {
        let Some(value) = line.trim().strip_prefix("description:") else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']).trim();
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn first_prose_line(content: &str) -> String {
    content
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && *line != "---")
        .map(str::to_string)
        .unwrap_or_default()
}

#[derive(Default)]
pub struct CodeCompactionHook;

impl CompactionHook for CodeCompactionHook {
    fn sections(&self, blocks: &[CompactBlock]) -> Vec<SummarySection> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut items: Vec<String> = Vec::new();

        items.push(
            "exec({code, strings?, display?}) — the TypeScript sandbox tool. Call lofi.* methods inside it."
                .to_string(),
        );
        seen.insert("exec".to_string());

        for rec in native_records(blocks) {
            if seen.insert(rec.name.clone()) {
                items.push(tool_description(&rec.name, &rec.args));
            }
        }

        let mut sections = vec![SummarySection {
            title: "APIs Used".to_string(),
            items,
        }];
        let skills = loaded_skills(blocks);
        if !skills.is_empty() {
            sections.push(SummarySection {
                title: "Skills".to_string(),
                items: skills,
            });
        }
        sections
    }

    fn file_changes(&self, blocks: &[CompactBlock]) -> Vec<String> {
        let mut modified: HashSet<String> = HashSet::new();
        let mut created: HashSet<String> = HashSet::new();
        let mut read: HashSet<String> = HashSet::new();

        for rec in native_records(blocks) {
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

        for rec in native_records(blocks) {
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
        out
    }

    fn compress_tool_result(&self, text: &str, max: usize) -> Option<String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Some(String::new());
        }

        if !trimmed.contains('\n') {
            return Some(clip(trimmed, max));
        }

        if trimmed.starts_with('{') {
            if let Some(summary) = compress_json_result(trimmed, max) {
                return Some(summary);
            }
        }

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
    let re = regex::Regex::new(r"\b[0-9a-f]{7,12}\b").ok()?;
    re.find(text).map(|m| m.as_str().to_string())
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::NativeToolRecord;

    fn skill_rec(name: &str, content: &str) -> NativeToolRecord {
        NativeToolRecord {
            parent: "t1".into(),
            call_id: 1,
            name: "skill".into(),
            args: name.into(),
            result: serde_json::json!({
                "ok": true,
                "name": name,
                "content": content,
            })
            .to_string(),
            is_error: false,
        }
    }

    fn call(native: Vec<NativeToolRecord>) -> CompactBlock {
        CompactBlock::ToolCall {
            id: "t1".into(),
            code: String::new(),
            label: None,
            native,
        }
    }

    #[test]
    fn sections_list_loaded_skills_with_frontmatter_description() {
        let blocks = [call(vec![skill_rec(
            "code-commit",
            "---\nname: code-commit\ndescription: Write a commit message\n---\n\n# Commit\n",
        )])];
        let sections = CodeCompactionHook.sections(&blocks);
        let skills = sections
            .iter()
            .find(|s| s.title == "Skills")
            .expect("Skills section");
        assert_eq!(skills.items, vec!["code-commit — Write a commit message"]);
        assert!(sections.iter().any(|s| s.title == "APIs Used"));
    }

    #[test]
    fn sections_skill_blurb_falls_back_to_first_prose_line() {
        let blocks = [call(vec![skill_rec(
            "outline",
            "# Outline\n\nStructural search over a file.\n",
        )])];
        let sections = CodeCompactionHook.sections(&blocks);
        let skills = sections
            .iter()
            .find(|s| s.title == "Skills")
            .expect("Skills section");
        assert_eq!(
            skills.items,
            vec!["outline — Structural search over a file."]
        );
    }

    #[test]
    fn sections_skip_failed_and_duplicate_skills() {
        let mut failed = skill_rec("missing", "");
        failed.is_error = true;
        let blocks = [call(vec![
            skill_rec(
                "code-commit",
                "---\ndescription: Write a commit message\n---\n",
            ),
            skill_rec(
                "code-commit",
                "---\ndescription: Write a commit message\n---\n",
            ),
            failed,
        ])];
        let sections = CodeCompactionHook.sections(&blocks);
        let skills = sections
            .iter()
            .find(|s| s.title == "Skills")
            .expect("Skills section");
        assert_eq!(skills.items.len(), 1);
    }
}
