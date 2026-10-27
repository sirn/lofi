//! Steering refusals for the `bash` tool. Steering matches the parsed
//! command, never a substring, and refuses the command before it runs.
//! `steer: false` on the call opts out and runs the command.

use crate::policy::extract::effective_argv;
use crate::policy::token::{tokenize, Token};
use crate::policy::WrapperRuleMap;

/// Line two of every refusal: names the opt-out argument.
const OPT_OUT: &str = "Set steer: false to run this command.";

/// The one steering refusal for `cmd`, or None when no pattern matches.
/// The two patterns cannot match the same command: a whole-command
/// `sleep` is never the first-word `claude` or `codex` CLI.
#[must_use]
pub fn refusal_for(cmd: &str, wrappers: &WrapperRuleMap) -> Option<String> {
    let tokens = tokenize(cmd).ok()?;
    sleep_refusal(&tokens, wrappers).or_else(|| cli_refusal(&tokens, wrappers))
}

/// A command that is only a sleep call: `sleep 5`, `sleep 0.5`, `sleep 30s`.
/// One simple invocation of plain words: no operator, redirect, or group.
/// Argv wrappers are transparent, so `env sleep 5` counts; a wrapper that
/// takes a shell program (`sh -c`) is not a sleep call.
fn sleep_refusal(tokens: &[Token], wrappers: &WrapperRuleMap) -> Option<String> {
    let words = plain_words(tokens)?;
    let argv = effective_argv(&words, wrappers);
    let (program, operands) = argv.split_first()?;
    if program.as_str() != "sleep" {
        return None;
    }
    // `sleep` takes durations only. `--` ends option parsing and is not
    // one; a real option (`--help`) makes this not a plain sleep call.
    let mut durations = 0;
    for arg in operands {
        if arg.as_str() == "--" {
            continue;
        }
        if !is_duration(arg) {
            return None;
        }
        durations += 1;
    }
    if durations == 0 {
        return None;
    }
    Some(format!(
        "Steering: this command is a foreground sleep. For a short wait use lofi.sleep(5). End the turn when the wait is long and let the notification wake you (for example lofi.notifyAt(\"2026-01-01T09:00:00+00:00\", \"msg\"), or lofi.notifyEvery(\"5m\", \"tick\") for a repeating wake-up).
{OPT_OUT}"
    ))
}

/// A command that runs the `claude` or `codex` CLI: the first word, after
/// `NAME=value` words and argv wrappers, is the bare CLI name. Never a
/// path, an argument, or a search pattern.
fn cli_refusal(tokens: &[Token], wrappers: &WrapperRuleMap) -> Option<String> {
    let words = leading_words(tokens);
    let argv = effective_argv(&words, wrappers);
    let name = argv.first()?;
    if !matches!(name.as_str(), "claude" | "codex") {
        return None;
    }
    Some(format!(
        "Steering: this command runs the {name} CLI. Use lofi.subagent({{ prompt: \"review this\" }}) instead.
{OPT_OUT}"
    ))
}

/// The words of a command made only of plain words: no operator, redirect,
/// or group anywhere.
fn plain_words(tokens: &[Token]) -> Option<Vec<&str>> {
    tokens
        .iter()
        .map(|t| match t {
            Token::Word(w) => Some(w.as_str()),
            _ => None,
        })
        .collect()
}

/// The words at the head of a command, up to the first operator, redirect,
/// or group: what follows starts another command.
fn leading_words(tokens: &[Token]) -> Vec<&str> {
    tokens
        .iter()
        .map_while(|t| match t {
            Token::Word(w) => Some(w.as_str()),
            _ => None,
        })
        .collect()
}

/// A sleep duration: digits with an optional fraction and an optional
/// `s`/`m`/`h`/`d` suffix (`5`, `0.5`, `30s`). GNU `sleep` takes a
/// leading-dot fraction (`.5`), so an empty whole part counts when the
/// fraction is not empty.
fn is_duration(arg: &str) -> bool {
    let arg = arg
        .strip_suffix('s')
        .or_else(|| arg.strip_suffix('m'))
        .or_else(|| arg.strip_suffix('h'))
        .or_else(|| arg.strip_suffix('d'))
        .unwrap_or(arg);
    let mut parts = arg.split('.');
    let (whole, frac) = (parts.next().unwrap_or(""), parts.next());
    parts.next().is_none()
        && whole.bytes().all(|b| b.is_ascii_digit())
        && frac.is_none_or(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
        && (!whole.is_empty() || frac.is_some())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::ShellPolicyConfig;

    /// The default wrapper rules: what the policy layer calls a wrapper.
    fn wrappers() -> WrapperRuleMap {
        crate::policy::defaults::resolve(&ShellPolicyConfig::default()).wrappers
    }

    fn refusal(cmd: &str) -> Option<String> {
        refusal_for(cmd, &wrappers())
    }

    #[test]
    fn plain_sleep_gets_the_sleep_refusal() {
        for cmd in ["sleep 30", "sleep 5", "sleep 0.5", "sleep .5", "sleep 30s"] {
            let r = refusal(cmd).unwrap_or_else(|| panic!("{cmd}: expected refusal"));
            assert!(r.contains("End the turn"), "{cmd}: {r}");
            assert!(r.contains("lofi.sleep"), "{cmd}: {r}");
            assert!(r.contains("notifyAt"), "{cmd}: {r}");
            assert!(r.contains("notifyEvery"), "{cmd}: {r}");
        }
    }

    #[test]
    fn sleep_refusal_names_the_opt_out() {
        let r = refusal("sleep 30").unwrap();
        let lines: Vec<_> = r.lines().collect();
        assert_eq!(lines.len(), 2, "{r}");
        assert!(lines[1].contains("steer: false"), "{r}");
    }

    #[test]
    fn compound_commands_get_no_sleep_refusal() {
        for cmd in [
            "sleep 5 && make",
            "echo hi > /tmp/x; sleep 1",
            "sleep 5 | cat",
            "sleep 5 &",
            "sh -c 'sleep 5'",
            "sleep 1 && sleep 2",
        ] {
            assert!(refusal(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn a_sleep_call_with_an_env_assignment_still_matches() {
        let r = refusal("FOO=1 sleep 5").unwrap();
        assert!(r.contains("End the turn"), "{r}");
    }

    #[test]
    fn non_sleep_arguments_get_no_refusal() {
        for cmd in [
            "sleep",
            "sleep --",
            "sleep abc",
            "sleep .",
            "sleep --help",
            "sleep $SECONDS",
        ] {
            assert!(refusal(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn external_agent_clis_get_the_subagent_refusal() {
        for cmd in ["claude -p 'review this'", "FOO=1 codex exec hello", "codex"] {
            let r = refusal(cmd).unwrap_or_else(|| panic!("{cmd}: expected refusal"));
            assert!(r.contains("lofi.subagent"), "{cmd}: {r}");
        }
    }

    #[test]
    fn cli_refusal_names_the_opt_out() {
        let r = refusal("claude -p 'review this'").unwrap();
        let lines: Vec<_> = r.lines().collect();
        assert_eq!(lines.len(), 2, "{r}");
        assert!(lines[1].contains("steer: false"), "{r}");
    }

    #[test]
    fn paths_arguments_and_search_patterns_get_no_cli_refusal() {
        for cmd in [
            "grep -rn codex .",
            "cat tmp/codex-notes",
            "/usr/bin/claude -p x",
            "./claude -p x",
            "echo claude",
            "make && claude -p x",
        ] {
            assert!(refusal(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn sleep_with_an_option_terminator_still_matches() {
        for cmd in ["sleep -- 5", "sleep -- 5s", "sleep 1 -- 2"] {
            let r = refusal(cmd).unwrap_or_else(|| panic!("{cmd}: expected refusal"));
            assert!(r.contains("End the turn"), "{cmd}: {r}");
        }
    }

    #[test]
    fn argv_wrapped_clis_get_the_subagent_refusal() {
        for cmd in [
            "env claude -p 'review this'",
            "FOO=1 env codex exec hello",
            "env -- claude",
            "command claude -p x",
        ] {
            let r = refusal(cmd).unwrap_or_else(|| panic!("{cmd}: expected refusal"));
            assert!(r.contains("lofi.subagent"), "{cmd}: {r}");
        }
    }

    #[test]
    fn argv_wrapped_sleep_gets_the_sleep_refusal() {
        for cmd in ["env sleep 5", "sudo sleep 5", "nice sleep 5"] {
            let r = refusal(cmd).unwrap_or_else(|| panic!("{cmd}: expected refusal"));
            assert!(r.contains("End the turn"), "{cmd}: {r}");
        }
    }

    #[test]
    fn wrapper_options_and_assignments_still_reveal_the_inner_command() {
        for cmd in [
            "env -u FOO sleep 5",
            "nice FOO=1 sleep 5",
            "nice -n 5 sleep 5",
            "sudo -u root sleep 5",
            "env FOO=1 sleep 5",
        ] {
            let r = refusal(cmd).unwrap_or_else(|| panic!("{cmd}: expected refusal"));
            assert!(r.contains("End the turn"), "{cmd}: {r}");
        }
        let r = refusal("sudo FOO=1 claude").unwrap();
        assert!(r.contains("lofi.subagent"), "{r}");
    }

    #[test]
    fn shell_program_wrappers_get_no_refusal() {
        for cmd in [
            "sh -c 'sleep 5'",
            "sh -c 'claude -p x'",
            "bash -c 'codex exec hi'",
        ] {
            assert!(refusal(cmd).is_none(), "{cmd}");
        }
    }

    #[test]
    fn detachable_container_wrappers_get_no_refusal() {
        for cmd in ["docker run img sleep 5", "podman run img claude -p x"] {
            assert!(refusal(cmd).is_none(), "{cmd}");
        }
    }
}
