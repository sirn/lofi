const AUTO_MODE_PROMPT: &str = r#"You are a shell-command safety evaluator for a coding agent.
The agent is working in the following directory: {CWD}

Evaluate whether the following shell command is safe to execute automatically
without user confirmation.

Command:
```
{COMMAND}
```

Respond with ONLY a JSON object:
{"decision":"allow","reason":"<brief reason>"}
or
{"decision":"ask","reason":"<brief reason>"}

Guidelines:
- "allow" = the command is read-only or a routine workspace operation (build,
  test, lint, format, list files, git status/diff/log, etc.) that cannot
  cause unintended data loss, privilege escalation, or network egress to
  untrusted endpoints.
- "ask" = the command could modify or delete files, push commits, access the
  network for non-routine purposes, escalate privileges, or has side effects
  the user should review.
- When in doubt, return "ask".
{SKILLS_RULE}Respond with the JSON object only, no markdown fences, no explanation outside the JSON."#;

/// Build the evaluator prompt. `skill_dirs` are user-installed skill roots
/// (workspace `.lofi/skills`, config `skills/`); when non-empty the prompt
/// marks commands executing their scripts as pre-vetted.
#[must_use]
pub fn build_prompt(command: &str, cwd: &str, skill_dirs: &[&std::path::Path]) -> String {
    let skills_rule = if skill_dirs.is_empty() {
        String::new()
    } else {
        let dirs = skill_dirs
            .iter()
            .map(|d| format!("  - {}", d.display()))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "- The user has installed and vetted skill scripts under:\n{dirs}\n\
             \x20 Commands that execute scripts from these directories are pre-vetted:\n\
             \x20 evaluate the arguments, not the script itself. Allow unless the\n\
             \x20 arguments are destructive, write outside the workspace, or\n\
             \x20 escalate privileges. Passing a skill path as an argument to\n\
             \x20 another operation is not pre-vetted.\n"
        )
    };
    AUTO_MODE_PROMPT
        .replace("{SKILLS_RULE}", &skills_rule)
        .replace("{COMMAND}", command)
        .replace("{CWD}", cwd)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeDecision {
    pub allow: bool,
    pub reason: String,
}

/// Accepts a bare JSON object or a fenced code block containing JSON.
/// Returns `None` when the response cannot be parsed, so the caller falls
/// back to the confirmation flow.
#[must_use]
pub fn parse_decision(text: &str) -> Option<AutoModeDecision> {
    let body = extract_json_object(text)?;
    let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
    let decision = parsed.get("decision")?.as_str()?;
    let allow = match decision {
        "allow" => true,
        "ask" => false,
        _ => return None,
    };
    let reason = parsed
        .get("reason")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(AutoModeDecision { allow, reason })
}

fn extract_json_object(text: &str) -> Option<String> {
    let fenced = text.find("```").and_then(|start| {
        let rest = &text[start + 3..];
        let rest = rest.find('\n').map_or(rest, |nl| &rest[nl + 1..]);
        let end = rest.find("```")?;
        Some(rest[..end].trim().to_string())
    });
    let body = fenced.as_deref().unwrap_or(text);
    let start = body.find('{')?;
    let end = body.rfind('}')?;
    if end <= start {
        return None;
    }
    Some(body[start..=end].to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn parse_allow() {
        let d = parse_decision(r#"{"decision":"allow","reason":"read-only"}"#).unwrap();
        assert!(d.allow);
        assert_eq!(d.reason, "read-only");
    }

    #[test]
    fn parse_ask() {
        let d = parse_decision(r#"{"decision":"ask","reason":"destructive"}"#).unwrap();
        assert!(!d.allow);
        assert_eq!(d.reason, "destructive");
    }

    #[test]
    fn parse_fenced_json() {
        let text = "```json\n{\"decision\":\"allow\",\"reason\":\"ok\"}\n```";
        let d = parse_decision(text).unwrap();
        assert!(d.allow);
    }

    #[test]
    fn parse_with_surrounding_text() {
        let text = "Here is my decision:\n{\"decision\":\"ask\",\"reason\":\"rm\"}\nDone.";
        let d = parse_decision(text).unwrap();
        assert!(!d.allow);
        assert_eq!(d.reason, "rm");
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(parse_decision("not json").is_none());
        assert!(parse_decision("{}").is_none());
        assert!(parse_decision("{\"decision\":\"maybe\"}").is_none());
    }

    #[test]
    fn parse_missing_reason_defaults_empty() {
        let d = parse_decision(r#"{"decision":"allow"}"#).unwrap();
        assert!(d.allow);
        assert!(d.reason.is_empty());
    }

    #[test]
    fn build_prompt_substitutes_placeholders() {
        let p = build_prompt("ls -la", "/home/user/project", &[]);
        assert!(p.contains("ls -la"));
        assert!(p.contains("/home/user/project"));
        assert!(!p.contains("{COMMAND}"));
        assert!(!p.contains("{CWD}"));
        assert!(!p.contains("{SKILLS_RULE}"));
    }

    #[test]
    fn build_prompt_without_skill_dirs_omits_rule() {
        let p = build_prompt("ls", "/repo", &[]);
        assert!(!p.contains("pre-vetted"));
    }

    #[test]
    fn build_prompt_with_skill_dirs_marks_scripts_pre_vetted() {
        let dirs: [&std::path::Path; 2] = [
            std::path::Path::new("/home/u/.config/lofi/skills"),
            std::path::Path::new("/repo/.lofi/skills"),
        ];
        let p = build_prompt("ls", "/repo", &dirs);
        assert!(p.contains("- /home/u/.config/lofi/skills"));
        assert!(p.contains("- /repo/.lofi/skills"));
        assert!(p.contains("pre-vetted"));
        // The rendered rule is line-wrapped; assert on substrings that
        // survive the wrap.
        assert!(p.contains("Passing a skill path as an argument to"));
        assert!(p.contains("not pre-vetted"));
        assert!(!p.contains("{SKILLS_RULE}"));
    }
}
