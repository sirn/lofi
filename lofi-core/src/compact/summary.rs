use std::collections::HashMap;
use std::sync::Arc;

use lofi_types::{
    clip, CompactBlock, CompactionHook, ContentBlock, Message, NativeToolRecord, Role,
};

use super::{user_text, HANDOFF_PREAMBLE, RESULT_RECALL_INSTRUCTION};

mod brief;
mod compress;
mod extract;
mod merge;
mod units;

#[allow(clippy::wildcard_imports)]
use self::{brief::*, compress::*, extract::*, merge::*, units::*};

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
