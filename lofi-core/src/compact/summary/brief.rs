#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) const BRIEF_MAX_LINES: usize = 120;

pub(super) const TRUNC_USER: usize = 256;

pub(super) const TRUNC_ASSISTANT: usize = 200;

pub(super) const MAX_SUMMARY_TOKENS: usize = 4_000;

#[allow(clippy::too_many_lines)]
pub(super) fn build_brief(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> String {
    let compress = |text: &str, max: usize| -> String {
        for hook in hooks {
            if let Some(s) = hook.compress_tool_result(text, max) {
                return s;
            }
        }
        compress_tool_result(text, max)
    };
    let full_path = |text: &str| -> Option<String> {
        for hook in hooks {
            if let Some(p) = hook.full_output_path(text) {
                return Some(p);
            }
        }
        extract_full_output_path(text).map(str::to_string)
    };

    let mut lines: Vec<String> = Vec::new();
    let mut last_header = "";
    for b in blocks {
        match b {
            CompactBlock::User { text } => {
                let t = summarize_prompt(text, TRUNC_USER);
                if t.is_empty() {
                    continue;
                }
                if last_header != "[user]" {
                    lines.push("[user]".to_string());
                    last_header = "[user]";
                }
                lines.push(t);
            }
            CompactBlock::Assistant { text } => {
                let t = summarize_response(text, TRUNC_ASSISTANT);
                if t.is_empty() {
                    continue;
                }
                if last_header != "[assistant]" {
                    lines.push("[assistant]".to_string());
                    last_header = "[assistant]";
                }
                lines.push(t);
            }
            CompactBlock::ToolCall {
                code,
                label,
                native,
                ..
            } => {
                if last_header != "[assistant]" {
                    lines.push("[assistant]".to_string());
                    last_header = "[assistant]";
                }
                if native.is_empty() {
                    let l = label.clone().unwrap_or_else(|| first_line(code, 60));
                    lines.push(format!("* exec \"{l}\""));
                } else {
                    let mut i = 0;
                    while i < native.len() {
                        let rec = &native[i];
                        let args = clip(&rec.args, 80);
                        let marker = if rec.is_error { " (error)" } else { "" };
                        let mut count = 1;
                        while i + count < native.len()
                            && native[i + count].name == rec.name
                            && native[i + count].args == rec.args
                            && native[i + count].is_error == rec.is_error
                        {
                            count += 1;
                        }
                        if count > 1 {
                            lines.push(format!("* {} \"{}\"{} (x{count})", rec.name, args, marker));
                        } else {
                            lines.push(format!("* {} \"{}\"{}", rec.name, args, marker));
                        }
                        i += count;
                    }
                }
            }
            CompactBlock::ToolResult {
                text,
                is_error: true,
                ..
            } => {
                let body = compress(text, 150);
                if body.is_empty() {
                    continue;
                }
                if last_header != "[tool_error]" {
                    lines.push("[tool_error]".to_string());
                    last_header = "[tool_error]";
                }
                lines.push(body);
            }
            CompactBlock::ToolResult {
                text,
                is_error: false,
                ..
            } => {
                let body = compress(text, 120);
                if body.is_empty() {
                    continue;
                }
                if last_header != "[tool_result]" {
                    lines.push("[tool_result]".to_string());
                    last_header = "[tool_result]";
                }
                if let Some(path) = full_path(text) {
                    lines.push(format!("{body} ... Full output: {path}"));
                } else {
                    lines.push(body);
                }
            }
        }
    }
    lines.join("\n")
}

pub(super) fn cap_brief(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let (kept, omitted) = if lines.len() <= BRIEF_MAX_LINES {
        (lines.as_slice(), 0)
    } else {
        let omitted = lines.len() - BRIEF_MAX_LINES;
        (&lines[lines.len() - BRIEF_MAX_LINES..], omitted)
    };
    let mut out = if omitted > 0 {
        format!("...({omitted} earlier lines omitted)\n\n")
    } else {
        String::new()
    };
    out.push_str(&kept.join("\n"));
    out
}

/// Trim the brief transcript (already line-capped by `cap_brief`) so the
/// full summary stays within the soft token budget. Removes lines from the
/// front of the brief (oldest first) until the estimated token count of the
/// *entire* summary body fits. The brief is the most volatile part and the
/// structured sections are always preserved.
pub(super) fn trim_brief_to_budget(headers: &str, brief: &str) -> String {
    if MAX_SUMMARY_TOKENS == 0 {
        return brief.to_string();
    }
    let header_tokens = headers.len() / 4;
    let budget = MAX_SUMMARY_TOKENS.saturating_sub(header_tokens);
    let brief_tokens = brief.len() / 4;
    if brief_tokens <= budget {
        return brief.to_string();
    }
    let lines: Vec<&str> = brief.split('\n').collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut kept_chars = 0;
    let budget_chars = budget * 4;
    for line in lines.iter().rev() {
        if kept_chars + line.len() + 1 > budget_chars && !kept.is_empty() {
            break;
        }
        kept_chars += line.len() + 1;
        kept.push(line);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    if omitted > 0 {
        format!(
            "...({omitted} earlier lines trimmed)\n\n{}",
            kept.join("\n")
        )
    } else {
        kept.join("\n")
    }
}
