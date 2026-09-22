use std::collections::HashMap;
use std::sync::Arc;

use lofi_types::{
    clip, CompactBlock, CompactionHook, ContentBlock, Message, NativeToolRecord, Role,
};

use super::{user_text, HANDOFF_PREAMBLE, RESULT_RECALL_INSTRUCTION};

pub(super) fn summarize(
    messages: &[&Message],
    native_by_parent: &HashMap<String, Vec<NativeToolRecord>>,
    hooks: &[Arc<dyn CompactionHook>],
    previous: Option<&str>,
) -> String {
    let blocks = normalize(messages, native_by_parent);
    let fresh = build_summary(&blocks, hooks);
    match previous {
        Some(previous) => merge_previous(previous, &fresh),
        None => fresh,
    }
}

fn normalize(
    messages: &[&Message],
    native_by_parent: &HashMap<String, Vec<NativeToolRecord>>,
) -> Vec<CompactBlock> {
    let mut out = Vec::new();
    for m in messages {
        match m.role {
            Role::User => {
                let text = user_text(m);
                if !text.trim().is_empty() {
                    out.push(CompactBlock::User { text });
                }
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = b
                    {
                        out.push(CompactBlock::ToolResult {
                            id: tool_use_id.clone(),
                            text: crate::agent::exec_result_display(content, *is_error),
                            is_error: *is_error,
                        });
                    }
                }
            }
            Role::Assistant => {
                let mut text_buf = String::new();
                for b in &m.blocks {
                    match b {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                text_buf.push_str(text);
                            }
                        }
                        ContentBlock::ToolUse { id, name, input } if name == "exec" => {
                            if !text_buf.is_empty() {
                                out.push(CompactBlock::Assistant {
                                    text: std::mem::take(&mut text_buf),
                                });
                            }
                            let (code, label) = crate::agent::exec_input_code_and_label(input);
                            let native = native_by_parent.get(id).cloned().unwrap_or_default();
                            out.push(CompactBlock::ToolCall {
                                id: id.clone(),
                                code,
                                label,
                                native,
                            });
                        }
                        ContentBlock::ToolUse { id, name, .. } => {
                            if !text_buf.is_empty() {
                                out.push(CompactBlock::Assistant {
                                    text: std::mem::take(&mut text_buf),
                                });
                            }
                            out.push(CompactBlock::ToolCall {
                                id: id.clone(),
                                code: String::new(),
                                label: Some(name.clone()),
                                native: Vec::new(),
                            });
                        }
                        ContentBlock::Thinking { .. }
                        | ContentBlock::PartSignature { .. }
                        | ContentBlock::ToolResult { .. }
                        | ContentBlock::Image { .. } => {}
                    }
                }
                if !text_buf.is_empty() {
                    out.push(CompactBlock::Assistant { text: text_buf });
                }
            }
            Role::Tool => {
                for b in &m.blocks {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                        ..
                    } = b
                    {
                        out.push(CompactBlock::ToolResult {
                            id: tool_use_id.clone(),
                            text: crate::agent::exec_result_display(content, *is_error),
                            is_error: *is_error,
                        });
                    }
                }
            }
            Role::System => {}
        }
    }
    out
}

const SEPARATOR: &str = "\n\n---\n\n";

fn build_summary(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> String {
    let goal = extract_session_goal(blocks);
    let prefs = extract_preferences(blocks);
    let outstanding = extract_outstanding(blocks, hooks);

    let files: Vec<String> = hooks
        .iter()
        .find_map(|h| {
            let items = h.file_changes(blocks);
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_else(|| extract_files(blocks));
    let commits: Vec<String> = hooks
        .iter()
        .find_map(|h| {
            let items = h.commits(blocks);
            (!items.is_empty()).then_some(items)
        })
        .unwrap_or_else(|| extract_commits(blocks));

    let brief = build_brief(blocks, hooks);

    let mut stable: Vec<String> = [
        section("Session Goal", &goal),
        section("User Preferences", &prefs),
        section("Files And Changes", &files),
        section("Commits", &commits),
    ]
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect();

    for hook in hooks {
        for sec in hook.sections(blocks) {
            let s = section(&sec.title, &sec.items);
            if !s.is_empty() {
                stable.push(s);
            }
        }
    }

    let volatile: Vec<String> = [section("Outstanding Context", &outstanding)]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    let headers_text = [stable, volatile].concat().join("\n\n");
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
    let body = parts.join(SEPARATOR);
    format_handoff(&body)
}

fn section(title: &str, items: &[String]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let body = items
        .iter()
        .map(|i| format!("- {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[{title}]\n{body}")
}

/// Session Goal: the first few substantive user prompts, clipped.
/// Prioritises the original intent (first 2 prompts) and the current
/// direction (most recent 3), dropping the middle to avoid evicting the
/// original goal on long sessions.
fn extract_session_goal(blocks: &[CompactBlock]) -> Vec<String> {
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
fn extract_preferences(blocks: &[CompactBlock]) -> Vec<String> {
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

#[derive(Debug)]
struct SummaryUnit {
    text: String,
    position: usize,
    salience: f64,
}

#[derive(Clone, Copy)]
enum SummaryKind {
    Prompt,
    Response,
}

/// Produce a deterministic extractive summary. Units retain their source
/// text, so compaction cannot rewrite paths, identifiers, numbers, or negation.
fn summarize_prompt(text: &str, max: usize) -> String {
    summarize_text(text, max, SummaryKind::Prompt)
}

fn summarize_response(text: &str, max: usize) -> String {
    summarize_text(text, max, SummaryKind::Response)
}

fn summarize_text(text: &str, max: usize, kind: SummaryKind) -> String {
    summarize_units(&text_units(text), max, kind)
}

fn summarize_units(units: &[String], max: usize, kind: SummaryKind) -> String {
    if units.is_empty() || max == 0 {
        return String::new();
    }
    let frequencies = term_frequencies(units);
    let last = units.len().saturating_sub(1);
    let candidates = units
        .iter()
        .enumerate()
        .map(|(position, text)| SummaryUnit {
            text: text.clone(),
            position,
            salience: unit_salience(text, position, last, &frequencies, kind),
        })
        .collect::<Vec<_>>();
    let max_salience = candidates
        .iter()
        .map(|candidate| candidate.salience)
        .fold(1.0_f64, f64::max);

    let mut selected = Vec::<usize>::new();
    let mut used = 0;
    while selected.len() < 3 {
        let mut best: Option<(usize, f64)> = None;
        for (index, candidate) in candidates.iter().enumerate() {
            if selected.contains(&index) {
                continue;
            }
            let separator = usize::from(!selected.is_empty()) * 2;
            let size = candidate.text.chars().count() + separator;
            if used + size > max {
                continue;
            }
            let relevance = candidate.salience / max_salience;
            if !selected.is_empty() && relevance < 0.3 {
                continue;
            }
            let redundancy = selected
                .iter()
                .map(|&chosen| ngram_similarity(&candidate.text, &candidates[chosen].text))
                .fold(0.0_f64, f64::max);
            let mmr = 0.75 * relevance - 0.25 * redundancy;
            let replace = best.is_none_or(|(best_index, best_score)| {
                mmr.total_cmp(&best_score).is_gt()
                    || (mmr.total_cmp(&best_score).is_eq()
                        && candidate.position < candidates[best_index].position)
            });
            if replace {
                best = Some((index, mmr));
            }
        }
        let Some((index, _)) = best else {
            break;
        };
        used += candidates[index].text.chars().count() + usize::from(!selected.is_empty()) * 2;
        selected.push(index);
    }

    if selected.is_empty() {
        let Some(best) = candidates.iter().max_by(|left, right| {
            left.salience
                .total_cmp(&right.salience)
                .then_with(|| right.position.cmp(&left.position))
        }) else {
            return String::new();
        };
        return clip(&best.text, max);
    }
    selected.sort_by_key(|&index| candidates[index].position);
    selected
        .into_iter()
        .map(|index| candidates[index].text.as_str())
        .collect::<Vec<_>>()
        .join("; ")
}

fn text_units(text: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut current = String::new();
    let mut chars = text.trim().chars().peekable();
    while let Some(ch) = chars.next() {
        current.push(ch);
        let sentence_period = ch == '.' && chars.peek().is_none_or(|next| next.is_whitespace());
        if sentence_period || matches!(ch, '\n' | '?' | '!' | ';' | '。' | '？' | '！' | '；') {
            push_prompt_unit(&mut units, &mut current);
        }
    }
    push_prompt_unit(&mut units, &mut current);

    let mut split = Vec::new();
    for unit in units {
        if unit.chars().count() <= 100 {
            split.push(unit);
            continue;
        }
        let mut current = String::new();
        for ch in unit.chars() {
            current.push(ch);
            if matches!(ch, ',' | '，') && current.chars().count() >= 30 {
                push_prompt_unit(&mut split, &mut current);
            }
        }
        push_prompt_unit(&mut split, &mut current);
    }
    split
}

fn push_prompt_unit(units: &mut Vec<String>, current: &mut String) {
    let unit = current.trim().trim_start_matches(['-', '*', '•']).trim();
    if !unit.is_empty() {
        units.push(unit.to_string());
    }
    current.clear();
}

fn term_frequencies(units: &[String]) -> std::collections::HashMap<String, usize> {
    let mut frequencies = std::collections::HashMap::new();
    for unit in units {
        for term in content_terms(unit) {
            *frequencies.entry(term).or_default() += 1;
        }
    }
    frequencies
}

fn content_terms(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|term| term.chars().count() > 2)
        .map(str::to_lowercase)
}

fn unit_salience(
    text: &str,
    position: usize,
    last: usize,
    frequencies: &std::collections::HashMap<String, usize>,
    kind: SummaryKind,
) -> f64 {
    let lower = text.to_lowercase();
    let normalized = lower.trim_start_matches(['#', '*', '_', ' ']);
    let protected = usize::from(text.contains("://") || text.contains('/') || text.contains('`'))
        + usize::from(text.chars().any(|ch| ch.is_ascii_digit()))
        + usize::from(text.split_whitespace().any(|word| word.contains('_')));
    let repeated = content_terms(text)
        .collect::<std::collections::HashSet<_>>()
        .iter()
        .filter_map(|term| frequencies.get(term))
        .filter(|&&count| count > 1)
        .count();
    let role_score = match kind {
        SummaryKind::Prompt => prompt_role_score(&lower, normalized),
        SummaryKind::Response => response_role_score(&lower, normalized),
    };
    let position_score = match kind {
        SummaryKind::Prompt => {
            (if position == 0 { 1.0 } else { 0.0 }) + (if position == last { 1.5 } else { 0.0 })
        }
        SummaryKind::Response => {
            (if position == 0 { 0.5 } else { 0.0 }) + (if position == last { 2.0 } else { 0.0 })
        }
    };

    1.0 + role_score
        + position_score
        + (protected.min(2) as f64 * 1.5)
        + (repeated.min(3) as f64 * 0.5)
}

fn prompt_role_score(lower: &str, normalized: &str) -> f64 {
    const DIRECTIVES: &[&str] = &[
        "please",
        "prefer",
        "do not",
        "don't",
        "must",
        "should",
        "make sure",
        "avoid",
        "implement",
        "add ",
        "fix ",
        "remove",
        "use ",
        "keep ",
        "look up",
        "inspect",
    ];
    const REQUESTS: &[&str] = &[
        "can you",
        "could you",
        "would you",
        "will you",
        "how ",
        "what ",
        "why ",
    ];
    const FILLER: &[&str] = &[
        "thanks",
        "thank you",
        "hello",
        "hi ",
        "for context",
        "background:",
    ];
    let directive = DIRECTIVES.iter().any(|signal| lower.contains(signal));
    let request = REQUESTS.iter().any(|signal| normalized.starts_with(signal));
    let filler = FILLER.iter().any(|prefix| normalized.starts_with(prefix));
    (if directive { 3.0 } else { 0.0 }) + if request { 2.0 } else { 0.0 }
        - if filler { 1.5 } else { 0.0 }
}

fn response_role_score(lower: &str, normalized: &str) -> f64 {
    const OUTCOMES: &[&str] = &[
        "implemented",
        "fixed",
        "added",
        "removed",
        "changed",
        "found",
        "cause",
        "result",
        "passes",
        "passed",
        "failed",
        "error",
        "verified",
        "complete",
        "done",
        "remaining",
        "blocked",
        "cannot",
        "warning",
    ];
    const PROGRESS: &[&str] = &[
        "i will",
        "i'll",
        "i am ",
        "i'm ",
        "let me",
        "next i",
        "now i",
        "checking",
        "running",
        "reviewing",
        "inspecting",
    ];
    let outcome = OUTCOMES.iter().any(|signal| lower.contains(signal));
    let progress = PROGRESS.iter().any(|prefix| normalized.starts_with(prefix));
    (if outcome { 3.0 } else { 0.0 }) - if progress { 2.5 } else { 0.0 }
}

fn ngram_similarity(left: &str, right: &str) -> f64 {
    fn ngrams(text: &str) -> std::collections::HashSet<String> {
        let chars = text.to_lowercase().chars().collect::<Vec<_>>();
        if chars.len() < 3 {
            return [chars.into_iter().collect()].into_iter().collect();
        }
        chars
            .windows(3)
            .map(|window| window.iter().collect())
            .collect()
    }
    let left = ngrams(left);
    let right = ngrams(right);
    let union = left.union(&right).count();
    if union == 0 {
        0.0
    } else {
        left.intersection(&right).count() as f64 / union as f64
    }
}

fn extract_files(blocks: &[CompactBlock]) -> Vec<String> {
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

fn extract_commits(blocks: &[CompactBlock]) -> Vec<String> {
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
    text.split(|c: char| !c.is_ascii_hexdigit())
        .find(|word| (7..=12).contains(&word.len()))
        .map(str::to_string)
}

fn blocker_regex() -> Option<&'static regex::Regex> {
    static RE: std::sync::OnceLock<Option<regex::Regex>> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(BLOCKER_RE).ok())
        .as_ref()
}

fn extract_outstanding(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> Vec<String> {
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

const BRIEF_MAX_LINES: usize = 120;
const TRUNC_USER: usize = 256;
const TRUNC_ASSISTANT: usize = 200;

const MAX_SUMMARY_TOKENS: usize = 4_000;

#[allow(clippy::too_many_lines)]
fn build_brief(blocks: &[CompactBlock], hooks: &[Arc<dyn CompactionHook>]) -> String {
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

fn cap_brief(text: &str) -> String {
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
fn trim_brief_to_budget(headers: &str, brief: &str) -> String {
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

const BLOCKER_RE: &str = r"(?i)(fail(ed|s|ure|ing)?|broken|cannot|can't|won't work|does not work|doesn't work|still (broken|failing|wrong)|blocked|blocker|not (fixed|resolved|working)|crash(es|ed|ing)?)";

const SECTION_HEADERS: &[&str] = &[
    "Session Goal",
    "User Preferences",
    "Files And Changes",
    "Commits",
    "APIs Used",
    "Skills",
    "Outstanding Context",
];

fn merge_previous(prev: &str, fresh: &str) -> String {
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

fn format_handoff(body: &str) -> String {
    format!("{HANDOFF_PREAMBLE}\n\n{RESULT_RECALL_INSTRUCTION}\n\n{body}")
}

fn strip_preamble(text: &str) -> String {
    let rest = text
        .strip_prefix(HANDOFF_PREAMBLE)
        .map_or(text, |rest| rest.trim_start_matches('\n'));
    rest.strip_prefix(RESULT_RECALL_INSTRUCTION)
        .unwrap_or(rest)
        .trim_start_matches('\n')
        .to_string()
}

fn split_headers_brief(body: &str) -> (String, String) {
    match body.split_once(SEPARATOR) {
        Some((h, b)) => (h.trim().to_string(), b.trim().to_string()),
        None => (body.trim().to_string(), String::new()),
    }
}

fn section_of(headers: &str, name: &str) -> String {
    let tag = format!("[{name}]");
    let start = headers.find(&tag).map(|i| i + tag.len());
    let Some(start) = start else {
        return String::new();
    };
    let rest = &headers[start..];
    let end = rest.find("\n[").unwrap_or(rest.len());
    rest[..end].trim().to_string()
}

fn extra_section_names(
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

fn merge_section(name: &str, prev: &str, fresh: &str) -> String {
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

fn merge_files(prev: &str, fresh: &str) -> String {
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

fn split_paths(rest: &str) -> Vec<String> {
    let no_recall = rest.split("+recall:").next().unwrap_or(rest);
    no_recall
        .split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.split('\n').next().unwrap_or("").trim();
    clip(line, max)
}

fn compress_tool_result(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if !trimmed.contains('\n') {
        return clip(trimmed, max);
    }

    if trimmed.starts_with('{') {
        if let Some(summary) = compress_json_result(trimmed, max) {
            return summary;
        }
    }

    let lines: Vec<&str> = trimmed
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect();
    if lines.is_empty() {
        return String::new();
    }
    let joined = lines.join(" | ");
    clip(&joined, max)
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

fn extract_full_output_path(text: &str) -> Option<&str> {
    let rest = text.split("Full output: ").nth(1)?;
    let path = rest.split(". Use lofi.read").next()?.trim();
    if path.starts_with('/') {
        Some(path)
    } else {
        None
    }
}

fn non_empty_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn native(parent: &str, name: &str, args: &str) -> NativeToolRecord {
        NativeToolRecord {
            parent: parent.into(),
            call_id: 0,
            name: name.into(),
            args: args.into(),
            result: String::new(),
            is_error: false,
        }
    }

    #[test]
    fn merge_previous_accumulates_goals() {
        let prev =
            format!("{HANDOFF_PREAMBLE}\n\n[Session Goal]\n- goal one\n\n---\n\n[user]\nold");
        let fresh =
            format!("{HANDOFF_PREAMBLE}\n\n[Session Goal]\n- goal two\n\n---\n\n[user]\nnew");
        let merged = merge_previous(&prev, &fresh);
        assert!(merged.contains("goal one"));
        assert!(merged.contains("goal two"));
        assert_eq!(merged.matches(RESULT_RECALL_INSTRUCTION).count(), 1);
    }

    #[test]
    fn merge_previous_keeps_apis_and_skills_before_outstanding() {
        let prev = format!(
            "{HANDOFF_PREAMBLE}\n\n[APIs Used]\n- exec\n\n[Skills]\n- code-commit — Write a commit message\n\n[Outstanding Context]\n- old blocker"
        );
        let fresh = format!(
            "{HANDOFF_PREAMBLE}\n\n[APIs Used]\n- exec\n- lofi.read\n\n[Skills]\n- code-iterate — Iterate until clean\n\n[Outstanding Context]\n- new blocker"
        );
        let merged = merge_previous(&prev, &fresh);
        let apis = merged.find("[APIs Used]").unwrap();
        let skills = merged.find("[Skills]").unwrap();
        let outstanding = merged.find("[Outstanding Context]").unwrap();
        assert!(apis < skills && skills < outstanding);
        assert!(merged.contains("lofi.read"));
        assert!(merged.contains("code-commit"));
        assert!(merged.contains("code-iterate"));
        assert!(merged.contains("new blocker"));
        assert!(!merged.contains("old blocker"));
    }

    #[test]
    fn merge_previous_preserves_original_goals() {
        let prev_goals: Vec<String> = (0..8).map(|i| format!("- original goal {i}")).collect();
        let fresh_goals: Vec<String> = (0..5).map(|i| format!("- fresh goal {i}")).collect();
        let prev = format!(
            "{HANDOFF_PREAMBLE}\n\n[Session Goal]\n{}\n\n---\n\n[user]\nold",
            prev_goals.join("\n")
        );
        let fresh = format!(
            "{HANDOFF_PREAMBLE}\n\n[Session Goal]\n{}\n\n---\n\n[user]\nnew",
            fresh_goals.join("\n")
        );
        let merged = merge_previous(&prev, &fresh);
        assert!(merged.contains("original goal 0"));
        assert!(merged.contains("original goal 1"));
        assert!(merged.contains("fresh goal 4"));
        let goal_count = merged
            .split("[Session Goal]")
            .nth(1)
            .unwrap_or("")
            .lines()
            .filter(|l| l.starts_with("- "))
            .count();
        assert!(goal_count <= 8, "goal_count={goal_count} should be <= 8");
    }

    #[test]
    fn extract_session_goal_selects_request_after_long_context() {
        let blocks = vec![CompactBlock::User {
            text: format!(
                "Background information that cannot fit in the summary by itself: {}. Please fix src/auth.rs without changing its public API.",
                "context ".repeat(30)
            ),
        }];
        let goals = extract_session_goal(&blocks);
        assert_eq!(
            goals,
            vec!["Please fix src/auth.rs without changing its public API."]
        );
    }

    #[test]
    fn brief_summarizes_user_prompt_instead_of_clipping_prefix() {
        let blocks = vec![CompactBlock::User {
            text: format!(
                "Background information that exceeds the brief limit: {}. Please fix src/auth.rs without changing its public API.",
                "context ".repeat(40)
            ),
        }];
        let brief = build_brief(&blocks, &[]);
        assert!(brief.contains("Please fix src/auth.rs"));
        assert!(!brief.contains("context context"));
    }

    #[test]
    fn brief_summarizes_agent_outcomes_instead_of_clipping_progress() {
        let blocks = vec![CompactBlock::Assistant {
            text: format!(
                "I will inspect the implementation before making changes. {}. Found the crash cause in src/compact.rs. Fixed the UTF-8 boundary handling. All 238 tests passed.",
                "Progress details ".repeat(20)
            ),
        }];
        let brief = build_brief(&blocks, &[]);
        assert!(brief.contains("Found the crash cause in src/compact.rs."));
        assert!(brief.contains("All 238 tests passed."));
        assert!(!brief.contains("I will inspect"));
        assert!(!brief.contains("Progress details"));
    }

    #[test]
    fn response_summary_keeps_failures_and_remaining_work() {
        let summary = summarize_response(
            "I am checking the build now. Formatting passed. Clippy failed in src/lib.rs. Remaining work: fix the lint error.",
            100,
        );
        assert!(summary.contains("Formatting passed."));
        assert!(summary.contains("Clippy failed in src/lib.rs."));
        assert!(summary.contains("Remaining work: fix the lint error."));
        assert!(!summary.contains("checking the build"));
    }

    #[test]
    fn prompt_summary_prefers_distinct_clauses() {
        let summary = summarize_prompt(
            "Please inspect auth failures. Please inspect auth failures. Add regression tests. Keep the public API unchanged.",
            90,
        );
        assert_eq!(summary.matches("inspect auth failures").count(), 1);
        assert!(summary.contains("regression tests"));
        assert!(summary.contains("public API unchanged"));
    }

    #[test]
    fn prompt_summary_preserves_multibyte_units_and_urls() {
        let summary = summarize_prompt(
            "背景です。https://example.com/tickets/42 を確認してください。認証処理を修正してください。",
            60,
        );
        assert!(summary.contains("https://example.com/tickets/42"));
        assert!(summary.contains("認証処理"));
        assert!(summary.chars().count() <= 60);
    }

    #[test]
    fn extract_preferences_skips_questions() {
        let blocks = vec![
            CompactBlock::User {
                text: "Can you use the openai provider?".to_string(),
            },
            CompactBlock::User {
                text: "Please always use 2-space indentation".to_string(),
            },
        ];
        let prefs = extract_preferences(&blocks);
        assert!(prefs.iter().all(|p| !p.contains("openai provider")));
        assert!(prefs.iter().any(|p| p.contains("2-space indentation")));
    }

    #[test]
    fn extract_preferences_handles_multibyte_prefix() {
        let blocks = vec![CompactBlock::User {
            text: format!("please {}", "グ".repeat(30)),
        }];
        let prefs = extract_preferences(&blocks);
        assert_eq!(prefs, vec![format!("please {}", "グ".repeat(30))]);
    }

    #[test]
    fn brief_collapses_consecutive_identical_tool_calls() {
        let blocks = vec![CompactBlock::ToolCall {
            id: "t1".to_string(),
            code: String::new(),
            label: None,
            native: vec![
                native("t1", "read", "foo.rs"),
                native("t1", "read", "foo.rs"),
                native("t1", "read", "foo.rs"),
            ],
        }];
        let brief = build_brief(&blocks, &[]);
        assert!(
            brief.contains("(x3)"),
            "brief should contain repeat count: {brief}"
        );
    }
}
