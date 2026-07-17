//! Auto-mode: LLM-based pre-approval of shell-policy `ask` decisions.
//!
//! When a command evaluates to `ask` (or unmatched `default`), the auto-mode
//! callback sends the command to a small LLM with a safety-evaluation prompt.
//! If the model returns `{"decision":"allow"}`, the command runs without
//! prompting the user. Any other response, a timeout, or a failure falls back
//! to the normal confirmation flow (user dialog or block in headless mode).
//!
//! The prompt is intentionally minimal: it gives the model the command, the
//! workspace cwd, and asks for a JSON decision. The model is instructed to
//! `allow` safe, non-destructive commands and `ask` for anything that could
//! modify the filesystem in unexpected ways, access the network, or escalate
//! privileges.

/// The system/user prompt template for auto-mode evaluation.
///
/// Placeholders: `{COMMAND}`, `{CWD}`.
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

Respond with the JSON object only, no markdown fences, no explanation outside the JSON."#;

/// Build the prompt text for a command evaluation.
#[must_use]
pub fn build_prompt(command: &str, cwd: &str) -> String {
    AUTO_MODE_PROMPT
        .replace("{COMMAND}", command)
        .replace("{CWD}", cwd)
}

/// The parsed decision from the LLM's response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoModeDecision {
    /// `true` when the command is safe to auto-approve.
    pub allow: bool,
    /// Optional reason from the model (for logging/diagnostics).
    pub reason: String,
}

/// Parse the LLM response text into a decision.
///
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

/// Extract the first `{...}` JSON object from the text, handling fenced
/// code blocks.
fn extract_json_object(text: &str) -> Option<String> {
    // Try fenced code block first.
    let fenced = text
        .find("```")
        .and_then(|start| {
            let rest = &text[start + 3..];
            // Skip optional language tag on the first line.
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
        let p = build_prompt("ls -la", "/home/user/project");
        assert!(p.contains("ls -la"));
        assert!(p.contains("/home/user/project"));
        assert!(!p.contains("{COMMAND}"));
        assert!(!p.contains("{CWD}"));
    }
}
