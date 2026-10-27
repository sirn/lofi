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

/// Wrapper kinds whose word list is the inner command's argv: the wrapper
/// runs it in the foreground and forwards its outcome. `ShellC` is not one:
/// its argument is a shell program, not an argv. `DockerRun` is not one
/// either: `--detach` runs the container in the background, so the inner
/// command is never a foreground call.
const ARGV_WRAPPERS: &[WrapperKind] = &[
    WrapperKind::Env,
    WrapperKind::UtilityOperand,
    WrapperKind::Xargs,
];

/// The argv a command runs with the shell's transparent decorations gone:
/// the leading `NAME=value` words and argv wrappers (`env`, `sudo`, `nice`,
/// ...) per the configured wrapper rules. Callers classify this argv to
/// decide what the command actually runs.
#[must_use]
pub fn effective_argv(words: &[&str], wrappers: &WrapperRuleMap) -> Vec<String> {
    let mut argv: Vec<String> = words.iter().map(|w| (*w).to_string()).collect();
    strip_command_assignments(&mut argv);
    while let Some(kind) = argv
        .first()
        .and_then(|w| wrappers.get(&w.to_ascii_lowercase()))
        .copied()
    {
        if !ARGV_WRAPPERS.contains(&kind) {
            break;
        }
        let Some(inner) = unwrap_wrapper(kind, &argv) else {
            break;
        };
        argv = inner;
        // A wrapper can take its own NAME=value operands (`nice FOO=1
        // sleep 5`), which the one-time strip above does not reach, so
        // re-strip before the next wrapper check and the final argv.
        strip_command_assignments(&mut argv);
    }
    argv
}

fn strip_command_assignments(argv: &mut Vec<String>) {
    let mut assigned = 0;
    while argv.get(assigned).is_some_and(|w| is_command_assignment(w)) {
        assigned += 1;
    }
    argv.drain(..assigned);
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

/// A `name=value` operand `env` forwards to the environment. `env` is not
/// the shell: it takes names the shell would reject, so any word with a
/// non-leading `=` counts.
fn is_assignment(word: &str) -> bool {
    word.find('=').is_some_and(|i| i > 0)
}

/// A `NAME=value` word the shell applies to one command. A shell name is
/// required: `foo-bar=x` names a command, it is not an assignment.
fn is_command_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// Option arity is per-command, so these tables cover only the common
// value-taking options of the configured argv wrappers (`sudo -u`,
// `nice -n`, `env -u`, ...). Bundled or unknown options keep the old
// behavior (only the dash word is skipped), and a positional operand
// like `timeout 5 sleep 5` is a known gap.
const UTILITY_VALUE_OPTIONS: &[&str] = &["-u", "-g", "-p", "-r", "-t", "-C", "-n"];

const ENV_VALUE_OPTIONS: &[&str] = &["-u", "-C", "-S"];

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
            let mut i = 1;
            while i < words.len() {
                let w = &words[i];
                if !saw_dash && w == "--" {
                    saw_dash = true;
                    i += 1;
                    continue;
                }
                if !saw_dash && w.starts_with('-') {
                    if UTILITY_VALUE_OPTIONS.contains(&w.as_str()) {
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                return Some(words[i..].to_vec());
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
                    if ENV_VALUE_OPTIONS.contains(&w.as_str()) {
                        i += 2;
                    } else {
                        i += 1;
                    }
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
