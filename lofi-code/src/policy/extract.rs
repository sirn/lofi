//! Command extraction and wrapper unwrapping.
//!
//! Splits a token stream into command segments (on operators), recursively
//! descends into subshells and command substitutions, and unwraps wrapper
//! commands (`sudo`, `env`, `bash -c`, `docker run`, …) to evaluate the
//! inner command rather than only the outer wrapper.

use std::collections::HashMap;

use super::token::{tokenize, GroupKind, Token};
use lofi_types::WrapperKind;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    Direct,
    Subshell,
    Substitution,
    WrapperArg,
}

#[derive(Debug, Clone)]
pub struct ExtractedCommand {
    pub name: String,
    pub full_text: String,
    pub words: Vec<String>,
    pub redirects: Vec<(String, String)>,
    pub source: CommandSource,
}

pub type WrapperRuleMap = HashMap<String, WrapperKind>;

#[must_use]
pub fn build_wrapper_map(entries: &[lofi_types::WrapperRuleConfig]) -> WrapperRuleMap {
    entries
        .iter()
        .map(|e| (e.name.to_ascii_lowercase(), e.kind))
        .collect()
}

const SKIP_SEGMENT: &[&str] = &["for", "case", "select", "in", "done", "fi", "esac"];

const STRIP_KEYWORDS: &[&str] = &["while", "until", "if", "elif", "do", "then", "else"];

#[must_use]
pub fn extract_commands(
    tokens: &[Token],
    source: CommandSource,
    wrappers: &WrapperRuleMap,
) -> Vec<ExtractedCommand> {
    let mut results = Vec::new();

    let mut segments: Vec<Vec<&Token>> = Vec::new();
    let mut current: Vec<&Token> = Vec::new();
    for tok in tokens {
        if matches!(tok, Token::Operator(_)) {
            if !current.is_empty() {
                segments.push(std::mem::take(&mut current));
            }
        } else {
            current.push(tok);
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }

    for seg in &segments {
        for tok in seg {
            if let Token::Group { tokens, kind } = tok {
                let src = match kind {
                    GroupKind::Subshell => CommandSource::Subshell,
                    _ => CommandSource::Substitution,
                };
                results.extend(extract_commands(tokens, src, wrappers));
            }
        }

        let mut word_tokens: Vec<&str> = Vec::new();
        let mut redirects: Vec<(String, String)> = Vec::new();
        for tok in seg {
            match tok {
                Token::Word(w) => word_tokens.push(w.as_str()),
                Token::Redirect { op, target } => redirects.push((op.clone(), target.clone())),
                _ => {}
            }
        }
        if word_tokens.is_empty() {
            continue;
        }

        if SKIP_SEGMENT.contains(&word_tokens[0]) {
            continue;
        }
        let word_slice = if STRIP_KEYWORDS.contains(&word_tokens[0]) {
            &word_tokens[1..]
        } else {
            &word_tokens[..]
        };
        if word_slice.is_empty() {
            continue;
        }

        let cmd_name = word_slice[0].to_string();
        if cmd_name.is_empty() {
            continue;
        }

        let words: Vec<String> = word_slice
            .iter()
            .map(std::string::ToString::to_string)
            .collect();
        let full_text = words.join(" ");
        results.push(ExtractedCommand {
            name: cmd_name.clone(),
            full_text,
            words,
            redirects,
            source,
        });

        if let Some(kind) = wrappers.get(&word_slice[0].to_ascii_lowercase()) {
            if let Some(inner_words) = unwrap_wrapper(
                *kind,
                &word_slice
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>(),
            ) {
                if !inner_words.is_empty() {
                    let inner_text = inner_words.join(" ");
                    if let Ok(inner_tokens) = tokenize(&inner_text) {
                        results.extend(extract_commands(
                            &inner_tokens,
                            CommandSource::WrapperArg,
                            wrappers,
                        ));
                    }
                }
            }
        }
    }

    results
}

fn is_assignment(word: &str) -> bool {
    word.find('=').is_some_and(|i| i > 0)
}

fn unwrap_wrapper(kind: WrapperKind, words: &[String]) -> Option<Vec<String>> {
    match kind {
        WrapperKind::ShellC => {
            let mut i = 1;
            while i + 1 < words.len() {
                if words[i] == "-c" {
                    return Some(vec![words[i + 1].clone()]);
                }
                i += 1;
            }
            None
        }
        WrapperKind::UtilityOperand | WrapperKind::Xargs => {
            let mut saw_dash = false;
            for w in &words[1..] {
                if !saw_dash && w == "--" {
                    saw_dash = true;
                    continue;
                }
                if !saw_dash && w.starts_with('-') {
                    continue;
                }
                return Some(
                    words
                        .iter()
                        .skip_while(|x| !std::ptr::eq(*x, w))
                        .cloned()
                        .collect(),
                );
            }
            None
        }
        WrapperKind::Env => {
            let mut saw_dash = false;
            let mut i = 1;
            while i < words.len() {
                let w = &words[i];
                if !saw_dash && w == "--" {
                    saw_dash = true;
                    i += 1;
                    continue;
                }
                if !saw_dash && w.starts_with('-') {
                    i += 1;
                    continue;
                }
                if is_assignment(w) {
                    i += 1;
                    continue;
                }
                return Some(words[i..].to_vec());
            }
            None
        }
        WrapperKind::DockerRun => {
            if words.len() < 3 {
                return None;
            }
            let sub = words[1].to_ascii_lowercase();
            if !matches!(sub.as_str(), "run" | "exec" | "create") {
                return None;
            }
            let mut i = 2;
            while i < words.len() {
                let val = &words[i];
                if val == "--" {
                    i += 2;
                    return words.get(i..).map(<[std::string::String]>::to_vec);
                }
                if val.starts_with("--") && val.contains('=') {
                    i += 1;
                    continue;
                }
                if is_docker_bool_flag(val) {
                    i += 1;
                    continue;
                }
                if val.starts_with('-') && !val.starts_with("--") && val.len() > 2 {
                    i += 1;
                    continue;
                }
                if val.starts_with("--") {
                    i += 2;
                    continue;
                }
                if val.starts_with('-') && val.len() == 2 {
                    i += 2;
                    continue;
                }
                i += 1;
                return words.get(i..).map(<[std::string::String]>::to_vec);
            }
            None
        }
    }
}

const DOCKER_BOOL_FLAGS: &[&str] = &[
    "--detach",
    "--interactive",
    "--tty",
    "--rm",
    "--privileged",
    "--init",
    "--read-only",
    "--publish-all",
    "--oom-kill-disable",
    "--no-healthcheck",
    "--sig-proxy",
    "--help",
    "-d",
    "-i",
    "-t",
    "-P",
];

fn is_docker_bool_flag(val: &str) -> bool {
    DOCKER_BOOL_FLAGS.contains(&val)
}
