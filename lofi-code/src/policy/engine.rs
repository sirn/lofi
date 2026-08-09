use lofi_types::{CommandEntry, HeredocPolicy, MatchMode, PolicyAction, RedirectPolicy};

use super::extract::{extract_commands, CommandSource, ExtractedCommand, WrapperRuleMap};
use super::token::{tokenize, Token};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub action: PolicyAction,
    pub reason: String,
    pub matched_command: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedPolicy {
    pub allow: Vec<CommandEntry>,
    pub ask: Vec<CommandEntry>,
    pub deny: Vec<CommandEntry>,
    pub wrappers: WrapperRuleMap,
    pub redirects: RedirectPolicy,
    pub heredocs: HeredocPolicy,
    pub yolo: bool,
    /// When true (unrestricted mode), unmatched commands are allowed
    /// instead of failing closed to ask.
    pub allow_by_default: bool,
}

impl ResolvedPolicy {
    #[must_use]
    pub fn evaluate(&self, command: &str) -> Decision {
        let tokens = match tokenize(command) {
            Ok(t) => t,
            Err(e) => {
                return Decision {
                    action: PolicyAction::Ask,
                    reason: format!("unparseable: {e}"),
                    matched_command: Some(command.to_string()),
                };
            }
        };

        // Hard-coded — not a per-rule deny — so a permissive config cannot
        // re-enable a shape the Bash tool fundamentally cannot serve
        // (a detached child outlives the call; the model never sees its
        // output).
        if let Some(reason) = detect_backgrounding(&tokens) {
            return Decision {
                action: PolicyAction::Deny,
                reason,
                matched_command: Some(command.to_string()),
            };
        }

        let cmds = extract_commands(&tokens, CommandSource::Direct, &self.wrappers);
        if cmds.is_empty() {
            return Decision {
                action: PolicyAction::Ask,
                reason: "empty or unrecognized command".into(),
                matched_command: Some(command.to_string()),
            };
        }

        let cmd_decision = self.evaluate_commands(&cmds);
        if cmd_decision.action == PolicyAction::Deny {
            return cmd_decision;
        }

        let redir_decision = self.evaluate_redirects(&cmds);
        if redir_decision.action == PolicyAction::Deny {
            return redir_decision;
        }

        let here_decision = self.evaluate_heredocs(&cmds);
        if here_decision.action == PolicyAction::Deny {
            return here_decision;
        }

        let winner = [&cmd_decision, &redir_decision, &here_decision]
            .into_iter()
            .max_by_key(|d| action_rank(d.action))
            .unwrap_or(&cmd_decision);

        let action = if winner.action == PolicyAction::Allow
            || (self.yolo && winner.action != PolicyAction::Deny)
        {
            PolicyAction::Allow
        } else {
            PolicyAction::Ask
        };

        Decision {
            action,
            reason: if action == PolicyAction::Ask && winner.reason.is_empty() {
                "no policy match".into()
            } else {
                winner.reason.clone()
            },
            matched_command: winner.matched_command.clone(),
        }
    }

    fn evaluate_commands(&self, cmds: &[ExtractedCommand]) -> Decision {
        let mut result = PolicyAction::Allow;
        let mut reason = String::new();
        let mut matched = None;
        let mut saw_direct_unmatched = false;

        for cmd in cmds {
            let deny_match = self.deny.iter().find(|e| match_entry(cmd, e));
            if deny_match.is_some() {
                return Decision {
                    action: PolicyAction::Deny,
                    reason: format!("denied: {}", cmd.name),
                    matched_command: Some(cmd.full_text.clone()),
                };
            }

            let ask_match = self.ask.iter().find(|e| match_entry(cmd, e));
            if ask_match.is_some() && result != PolicyAction::Deny {
                result = PolicyAction::Ask;
                reason = format!("requires confirmation: {}", cmd.name);
                matched = Some(cmd.full_text.clone());
            }

            let allow_match = self.allow.iter().find(|e| match_entry(cmd, e));
            if allow_match.is_some() {
                if result == PolicyAction::Allow
                    && (cmd.source != CommandSource::Direct || !saw_direct_unmatched)
                {
                    reason = format!("allowed: {}", cmd.name);
                    matched = Some(cmd.full_text.clone());
                }
            } else if ask_match.is_none() {
                if cmd.source == CommandSource::Direct {
                    saw_direct_unmatched = true;
                }
                if result == PolicyAction::Allow && !self.allow_by_default {
                    result = PolicyAction::Ask;
                    reason = format!("no policy match: {}", cmd.name);
                    matched = Some(cmd.full_text.clone());
                }
            }
        }

        Decision {
            action: result,
            reason,
            matched_command: matched,
        }
    }

    fn evaluate_redirects(&self, cmds: &[ExtractedCommand]) -> Decision {
        for cmd in cmds {
            for (op, target) in &cmd.redirects {
                let base = op.trim_start_matches(|c: char| c.is_ascii_digit());
                if matches!(base, "<" | "<<" | "<<-" | "<<<") {
                    continue;
                }
                if op.ends_with('&') && self.redirects.allow_fd_dup {
                    continue;
                }
                if self.redirects.safe_targets.iter().any(|t| t == target) {
                    continue;
                }
                if self.redirects.action != PolicyAction::Allow {
                    return Decision {
                        action: self.redirects.action,
                        reason: format!("redirect {op} {target}"),
                        matched_command: Some(cmd.full_text.clone()),
                    };
                }
            }
        }
        Decision {
            action: PolicyAction::Allow,
            reason: String::new(),
            matched_command: None,
        }
    }

    fn evaluate_heredocs(&self, cmds: &[ExtractedCommand]) -> Decision {
        for cmd in cmds {
            if cmd
                .redirects
                .iter()
                .any(|(op, _)| op == "<<" || op == "<<-")
                && self.heredocs.action != PolicyAction::Allow
            {
                return Decision {
                    action: self.heredocs.action,
                    reason: "heredoc detected".into(),
                    matched_command: Some(cmd.full_text.clone()),
                };
            }
        }
        Decision {
            action: PolicyAction::Allow,
            reason: String::new(),
            matched_command: None,
        }
    }
}

/// Detect a shell idiom that detaches work from the Bash call. Returns a
/// human-readable deny reason on match, None when the input is clean.
///
/// Recurses into subshells and command substitution so "sh -c 'sleep 1 &'"
/// cannot sneak past the top-level scan.
fn detect_backgrounding(tokens: &[Token]) -> Option<String> {
    const DETACH_WORDS: &[&str] = &["nohup", "setsid", "disown"];

    let mut start_of_segment = true;
    for tok in tokens {
        match tok {
            Token::Operator(op) if op == "&" => {
                return Some("bare & is not allowed; use lofi.jobSpawn for background jobs".into());
            }
            Token::Operator(_) => {
                start_of_segment = true;
            }
            Token::Word(w) => {
                if start_of_segment && DETACH_WORDS.contains(&w.as_str()) {
                    return Some(format!(
                        "{w} is not allowed; use lofi.jobSpawn for background jobs"
                    ));
                }
                start_of_segment = false;
            }
            Token::Group { tokens, .. } => {
                if let Some(reason) = detect_backgrounding(tokens) {
                    return Some(reason);
                }
            }
            Token::Redirect { .. } => {}
        }
    }
    None
}

fn action_rank(a: PolicyAction) -> u8 {
    match a {
        PolicyAction::Allow => 1,
        PolicyAction::Ask => 2,
        PolicyAction::Deny => 3,
    }
}

fn match_entry(cmd: &ExtractedCommand, entry: &CommandEntry) -> bool {
    match entry.mode {
        MatchMode::Exact => cmd.full_text.trim().eq_ignore_ascii_case(&entry.match_str),
        MatchMode::Prefix => {
            let lower = cmd.full_text.trim_start().to_ascii_lowercase();
            let m = entry.match_str.to_ascii_lowercase();
            lower == m || lower.starts_with(&format!("{m} "))
        }
        MatchMode::Substring => {
            let match_tokens: Vec<&str> = entry.match_str.split_whitespace().collect();
            if match_tokens.is_empty() || match_tokens.len() > cmd.words.len() {
                return false;
            }
            let cmd_lower: Vec<String> = cmd.words.iter().map(|w| w.to_ascii_lowercase()).collect();
            let match_lower: Vec<String> = match_tokens
                .iter()
                .map(|t| t.to_ascii_lowercase())
                .collect();
            (0..=cmd_lower.len().saturating_sub(match_lower.len())).any(|i| {
                cmd_lower[i..i + match_lower.len()]
                    .iter()
                    .zip(&match_lower)
                    .all(|(a, b)| a == b)
            })
        }
        MatchMode::Args => {
            let (prefix, required) = parse_args_pattern(&entry.match_str);
            if prefix != "*"
                && !cmd
                    .full_text
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with(&prefix.to_ascii_lowercase())
            {
                return false;
            }
            let prefix_words = if prefix == "*" {
                0
            } else {
                prefix.split_whitespace().count()
            };
            let cmd_args: Vec<String> = cmd.words[prefix_words..]
                .iter()
                .map(|w| w.to_ascii_lowercase())
                .collect();
            required.iter().all(|r| cmd_args.iter().any(|a| a == r))
        }
    }
}

fn parse_args_pattern(pattern: &str) -> (String, Vec<String>) {
    match pattern.find(':') {
        None => (pattern.to_string(), Vec::new()),
        Some(idx) => {
            let prefix = pattern[..idx].to_string();
            let required: Vec<String> = pattern[idx + 1..]
                .split_whitespace()
                .map(str::to_ascii_lowercase)
                .collect();
            (prefix, required)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::defaults;
    use super::*;
    use lofi_types::*;

    fn policy_for(mode: ShellPolicyMode) -> ResolvedPolicy {
        defaults::resolve(&ShellPolicyConfig {
            mode,
            ..Default::default()
        })
    }

    #[test]
    fn confirm_mode_asks_for_every_non_denied_command() {
        let p = policy_for(ShellPolicyMode::Confirm);
        for command in [
            "ls",
            "cargo build",
            "cat ~/.ssh/id_rsa",
            "curl https://example.com",
        ] {
            assert_eq!(p.evaluate(command).action, PolicyAction::Ask, "{command}");
        }
        assert_eq!(p.evaluate("sudo ls").action, PolicyAction::Deny);
    }

    #[test]
    fn redirects_ask_by_default() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("echo x > /tmp/outside-workspace");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn network_clients_are_not_preset_allows() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        assert_eq!(
            p.evaluate("curl https://example.com").action,
            PolicyAction::Ask
        );
        assert_eq!(
            p.evaluate("wget https://example.com").action,
            PolicyAction::Ask
        );
    }

    #[test]
    fn allow_ls() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("ls -la");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn allow_cargo() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("cargo build");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn deny_sudo() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("sudo rm -rf /");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn ask_rm() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("rm file.txt");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn ask_git_commit() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("git commit -m test");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn deny_bare_ampersand_backgrounds() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("sleep 30 &");
        assert_eq!(d.action, PolicyAction::Deny);
        assert!(d.reason.contains("jobSpawn"), "reason: {}", d.reason);
    }

    #[test]
    fn deny_ampersand_after_otherwise_allowed_command() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        // Even when every segment is allowed, the bare "&" is a hard deny.
        let d = p.evaluate("cargo build & cargo test");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn deny_nohup_setsid_disown() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        for cmd in ["nohup sleep 5", "setsid cargo watch", "disown %1"] {
            let d = p.evaluate(cmd);
            assert_eq!(d.action, PolicyAction::Deny, "cmd: {cmd}");
        }
    }

    #[test]
    fn deny_backgrounding_inside_subshell_and_substitution() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        for cmd in ["(sleep 5 &)", "echo $(sleep 5 &)", "echo `sleep 5 &`"] {
            let d = p.evaluate(cmd);
            assert_eq!(d.action, PolicyAction::Deny, "cmd: {cmd}");
        }
    }

    #[test]
    fn allow_double_ampersand_and_pipeline() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        // "&&" and "|" are sequencing, not backgrounding.
        let d = p.evaluate("cargo build && cargo test");
        assert_eq!(d.action, PolicyAction::Allow);
        let d = p.evaluate("cargo build | tail -3");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn deny_find_root() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("find / -name foo");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn readonly_denies_cargo() {
        let p = policy_for(ShellPolicyMode::ReadOnly);
        let d = p.evaluate("cargo build");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn readonly_allows_ls() {
        let p = policy_for(ShellPolicyMode::ReadOnly);
        let d = p.evaluate("ls");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn unrestricted_allows_cargo() {
        let p = policy_for(ShellPolicyMode::Unrestricted);
        let d = p.evaluate("cargo build");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn unrestricted_denies_sudo() {
        let p = policy_for(ShellPolicyMode::Unrestricted);
        let d = p.evaluate("sudo ls");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn yolo_allows_ask() {
        let p = defaults::resolve(&ShellPolicyConfig {
            mode: ShellPolicyMode::WorkspaceWrite,
            yolo: true,
            ..Default::default()
        });
        let d = p.evaluate("rm file.txt");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn yolo_still_denies_sudo() {
        let p = defaults::resolve(&ShellPolicyConfig {
            mode: ShellPolicyMode::WorkspaceWrite,
            yolo: true,
            ..Default::default()
        });
        let d = p.evaluate("sudo rm -rf /");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn pipeline_both_checked() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("ls | rm -rf /tmp");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn pipeline_deny_dominates() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("ls | sudo rm /etc/passwd");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn wrapper_bash_c_unwrapped() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("bash -c 'sudo rm -rf /'");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn wrapper_sudo_unwrapped() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("sudo ls");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn subshell_checked() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("(sudo rm /etc/passwd)");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn cmd_substitution_checked() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("echo $(sudo ls)");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn unparseable_fails_closed() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("echo 'unclosed");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn unmatched_fails_closed() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("totally-unknown-binary --flag");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn custom_allow_rule() {
        let p = defaults::resolve(&ShellPolicyConfig {
            mode: ShellPolicyMode::ReadOnly,
            allow: vec![CommandEntry {
                match_str: "my-tool".into(),
                mode: MatchMode::Prefix,
            }],
            ..Default::default()
        });
        let d = p.evaluate("my-tool --check");
        assert_eq!(d.action, PolicyAction::Allow);
    }

    #[test]
    fn custom_deny_overrides_mode_allow() {
        let p = defaults::resolve(&ShellPolicyConfig {
            mode: ShellPolicyMode::WorkspaceWrite,
            deny: vec![CommandEntry {
                match_str: "cargo".into(),
                mode: MatchMode::Prefix,
            }],
            ..Default::default()
        });
        let d = p.evaluate("cargo build");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn args_mode_match() {
        let p = defaults::resolve(&ShellPolicyConfig {
            mode: ShellPolicyMode::Unrestricted,
            ask: vec![CommandEntry {
                match_str: "*:-X POST".into(),
                mode: MatchMode::Args,
            }],
            ..Default::default()
        });
        let d = p.evaluate("curl -X POST http://example.com");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn heredoc_ask_by_default() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("cat <<EOF\nhello\nEOF");
        assert_eq!(d.action, PolicyAction::Ask);
    }

    #[test]
    fn env_wrapper_unwrapped() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("env FOO=bar sudo rm /etc");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn process_substitution_checked() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("diff <(sudo rm /etc/passwd) <(ls)");
        assert_eq!(d.action, PolicyAction::Deny);
    }

    #[test]
    fn quoted_cmd_substitution_checked() {
        let p = policy_for(ShellPolicyMode::WorkspaceWrite);
        let d = p.evaluate("echo \"$(sudo rm -rf /)\"");
        assert_eq!(d.action, PolicyAction::Deny);
    }
}
