#[allow(clippy::wildcard_imports)]
use super::*;

#[derive(Debug)]
pub(super) struct SummaryUnit {
    text: String,
    position: usize,
    salience: f64,
}

#[derive(Clone, Copy)]
pub(super) enum SummaryKind {
    Prompt,
    Response,
}

/// Produce a deterministic extractive summary. Units retain their source
/// text, so compaction cannot rewrite paths, identifiers, numbers, or negation.
pub(super) fn summarize_prompt(text: &str, max: usize) -> String {
    summarize_text(text, max, SummaryKind::Prompt)
}

pub(super) fn summarize_response(text: &str, max: usize) -> String {
    summarize_text(text, max, SummaryKind::Response)
}

pub(super) fn summarize_text(text: &str, max: usize, kind: SummaryKind) -> String {
    summarize_units(&text_units(text), max, kind)
}

pub(super) fn summarize_units(units: &[String], max: usize, kind: SummaryKind) -> String {
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

pub(super) fn text_units(text: &str) -> Vec<String> {
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

pub(super) fn push_prompt_unit(units: &mut Vec<String>, current: &mut String) {
    let unit = current.trim().trim_start_matches(['-', '*', '•']).trim();
    if !unit.is_empty() {
        units.push(unit.to_string());
    }
    current.clear();
}

pub(super) fn term_frequencies(units: &[String]) -> std::collections::HashMap<String, usize> {
    let mut frequencies = std::collections::HashMap::new();
    for unit in units {
        for term in content_terms(unit) {
            *frequencies.entry(term).or_default() += 1;
        }
    }
    frequencies
}

pub(super) fn content_terms(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|ch: char| !ch.is_alphanumeric() && ch != '_' && ch != '-')
        .filter(|term| term.chars().count() > 2)
        .map(str::to_lowercase)
}

pub(super) fn unit_salience(
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

pub(super) fn prompt_role_score(lower: &str, normalized: &str) -> f64 {
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

pub(super) fn response_role_score(lower: &str, normalized: &str) -> f64 {
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

pub(super) fn ngram_similarity(left: &str, right: &str) -> f64 {
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
