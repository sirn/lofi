#[allow(clippy::wildcard_imports)]
use super::*;

/// Session Goal: the first few substantive user prompts, clipped.
/// Prioritises the original intent (first 2 prompts) and the current
/// direction (most recent 3), dropping the middle to avoid evicting the
/// original goal on long sessions.
pub(super) fn extract_session_goal(blocks: &[CompactBlock]) -> Vec<String> {
    const HEAD: usize = 2;
    const TAIL: usize = 3;
    const MAX: usize = 8;

    let prompts: Vec<&str> = blocks
        .iter()
        .filter_map(|b| {
            let CompactBlock::User { text } = b else {
                return None;
            };
            let t = text.trim();
            (!t.is_empty()).then_some(t)
        })
        .collect();
    if prompts.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let n = prompts.len();
    let mut keep: Vec<usize> = Vec::new();
    if n <= MAX {
        (0..n).for_each(|i| keep.push(i));
    } else {
        (0..HEAD.min(n)).for_each(|i| keep.push(i));
        let tail_start = n.saturating_sub(TAIL);
        for i in tail_start..n {
            if !keep.contains(&i) {
                keep.push(i);
            }
        }
        // Fill from the middle, preferring earlier (closer to the original
        // goal) so the thread of intent is preserved.
        for i in HEAD..tail_start {
            if keep.len() >= MAX {
                break;
            }
            keep.push(i);
        }
    }
    keep.sort_unstable();
    for &i in &keep {
        let summarized = summarize_prompt(prompts[i], 200);
        let key = summarized.to_lowercase();
        if seen.insert(key) {
            out.push(summarized);
        }
    }
    out
}

/// User Preferences: user clauses that read as a directive.
pub(super) fn extract_preferences(blocks: &[CompactBlock]) -> Vec<String> {
    const SIGNALS: &[&str] = &[
        "prefer",
        "use ",
        "don't",
        "do not",
        "always",
        "never",
        "please",
        "make sure",
        "avoid",
        "keep ",
        "no need",
        "no longer",
        "instead of",
        "rather than",
    ];
    fn has_signal_near_start(lower: &str) -> bool {
        const MAX_PREFIX: usize = 60;
        let prefix = lower
            .char_indices()
            .nth(MAX_PREFIX)
            .map_or(lower, |(end, _)| &lower[..end]);
        SIGNALS.iter().any(|s| prefix.contains(s))
    }
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in blocks {
        let CompactBlock::User { text } = b else {
            continue;
        };
        let directives = text_units(text)
            .into_iter()
            .filter(|unit| {
                !unit.trim_end().ends_with('?') && has_signal_near_start(&unit.to_lowercase())
            })
            .collect::<Vec<_>>();
        if directives.is_empty() {
            continue;
        }
        let line = summarize_units(&directives, 160, SummaryKind::Prompt);
        if line.chars().count() < 8 {
            continue;
        }
        let key = line.to_lowercase();
        if seen.insert(key) {
            out.push(line);
        }
        if out.len() >= 15 {
            break;
        }
    }
    out
}

pub(super) fn extract_files(blocks: &[CompactBlock]) -> Vec<String> {
    let mut modified: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut created: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut read: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rec in blocks.iter().flat_map(CompactBlock::native_records) {
        if rec.is_error {
            continue;
        }
        match rec.name.as_str() {
            "edit" if !rec.args.is_empty() => {
                modified.insert(rec.args.clone());
            }
            "write" if !rec.args.is_empty() => {
                created.insert(rec.args.clone());
            }
            "read" | "bash_read" if !rec.args.is_empty() => {
                read.insert(rec.args.clone());
            }
            _ => {}
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

pub(super) fn extract_commits(blocks: &[CompactBlock]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for rec in blocks.iter().flat_map(CompactBlock::native_records) {
        if rec.name != "bash" || !rec.args.contains("git commit") {
            continue;
        }
        let msg = extract_commit_message(&rec.args).unwrap_or_else(|| "(git commit)".to_string());
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

pub(super) fn extract_commit_message(cmd: &str) -> Option<String> {
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

pub(super) fn first_hash(text: &str) -> Option<String> {
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| (7..=12).contains(&word.len()))
        .map(str::to_string)
}

pub(super) fn blocker_regex() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(BLOCKER_RE).ok())
        .as_ref()
}

pub(super) fn extract_outstanding(
    blocks: &[CompactBlock],
    hooks: &[Arc<dyn CompactionHook>],
) -> Vec<String> {
    let blocker = blocker_regex();
    let tail = if blocks.len() > 25 {
        &blocks[blocks.len() - 25..]
    } else {
        blocks
    };
    let compress = |text: &str, max: usize| -> String {
        for hook in hooks {
            if let Some(s) = hook.compress_tool_result(text, max) {
                return s;
            }
        }
        compress_tool_result(text, max)
    };
    let mut items: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for b in tail {
        match b {
            CompactBlock::ToolResult {
                text,
                is_error: true,
                ..
            } => {
                let s = format!("[ERROR] {}", compress(text, 150));
                if seen.insert(s.clone()) {
                    items.push(s);
                }
            }
            CompactBlock::ToolCall { native, .. } => {
                for rec in native {
                    if !rec.is_error {
                        continue;
                    }
                    let s = format!("[ERROR] {}: {}", rec.name, first_line(&rec.result, 120));
                    if seen.insert(s.clone()) {
                        items.push(s);
                    }
                }
            }
            CompactBlock::Assistant { text } | CompactBlock::User { text } => {
                for line in non_empty_lines(text) {
                    if line.len() < 15 || !blocker.is_some_and(|regex| regex.is_match(&line)) {
                        continue;
                    }
                    let s = clip(&line, 150);
                    if seen.insert(s.clone()) {
                        items.push(s);
                    }
                    break;
                }
            }
            CompactBlock::ToolResult {
                is_error: false, ..
            } => {}
        }
        if items.len() >= 8 {
            break;
        }
    }
    items
}

pub(super) const BLOCKER_RE: &str = r"(?i)(fail(ed|s|ure|ing)?|broken|cannot|can't|won't work|does not work|doesn't work|still (broken|failing|wrong)|blocked|blocker|not (fixed|resolved|working)|crash(es|ed|ing)?)";
