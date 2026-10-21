#![allow(clippy::doc_markdown)]

use std::sync::Arc;

use lofi_types::{CompactionHook, ContentBlock, Message, PromptKind, Role, SessionEvent};

mod plan;
mod summary;

pub const HANDOFF_PREAMBLE: &str = "This summary captures work done before the most recent messages in this session. Read it to pick up context — this is work already in progress. Continue directly where you left off.";

/// Stubs in the kept tail carry only the event id, so this line is the
/// single place that tells the model how to recover them.
const RESULT_RECALL_INSTRUCTION: &str = "Cleared tool results and exec source show only an event id. Recover the original with lofi.result(eventId).";

#[derive(Debug, Clone)]
pub struct Compaction {
    pub summary: String,
    pub kept_messages: Vec<Message>,
    pub summarized_count: usize,
    pub represented_count: usize,
    pub kept_count: usize,
    pub first_kept_event_id: Option<String>,
    pub summarized_range: Option<[String; 2]>,
}

#[derive(Clone, Default)]
pub struct CompactOptions {
    /// Soft token budget (chars/4) for the kept tail. When the most recent
    /// turn alone exceeds it, the cut is pushed back to a completed
    /// tool-cycle boundary so the oversized turn is partly summarized too.
    /// 0 disables the budget guard.
    pub max_kept_tokens: usize,
    pub edit: lofi_types::EditConfig,
    pub hooks: Vec<Arc<dyn CompactionHook>>,
}

#[must_use]
pub fn compact(events: &[SessionEvent], opts: &CompactOptions) -> Option<Compaction> {
    let plan::CompactionPlan {
        live,
        cut: plan,
        previous_summary,
        previously_summarized,
        native_by_parent,
    } = plan::prepare(events, opts)?;

    let prefix_messages: Vec<&Message> = live[..plan.summarized]
        .iter()
        .map(|lm| &lm.message)
        .collect();
    let summary = summary::summarize(
        &prefix_messages,
        &native_by_parent,
        &opts.hooks,
        previous_summary.as_deref(),
    );

    let kept_count = live.len().saturating_sub(plan.summarized);
    let kept_live = &live[plan.summarized..];
    let kept_pairs: Vec<(&str, &Message)> = kept_live
        .iter()
        .map(|message| (message.event_id.as_str(), &message.message))
        .collect();
    let kept_messages: Vec<Message> = crate::context_edit::edit_tail_refs(&kept_pairs, &opts.edit);

    let summarized_range: Option<[String; 2]> = if plan.summarized > 0 {
        let first = live[0].event_id.clone();
        let last = live[plan.summarized - 1].event_id.clone();
        Some([first, last])
    } else {
        None
    };

    Some(Compaction {
        summary,
        kept_messages,
        summarized_count: plan.summarized,
        represented_count: previously_summarized.saturating_add(plan.summarized),
        kept_count,
        first_kept_event_id: plan.first_kept_event_id,
        summarized_range,
    })
}

#[must_use]
pub fn compacted_history(compaction: &Compaction) -> Vec<Message> {
    let mut out = Vec::with_capacity(compaction.kept_messages.len() + 1);
    if !compaction.summary.is_empty() {
        out.push(Message {
            origin: None,
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: compaction.summary.clone(),
            }],
            kind: PromptKind::default(),
        });
    }
    out.extend(compaction.kept_messages.iter().cloned());
    out
}

fn user_text(m: &Message) -> String {
    m.blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::plan::MIN_SUMMARIZED;
    use super::*;
    use lofi_types::{NativeToolRecord, SessionEventKind};

    fn user(t: &str) -> Message {
        Message {
            origin: None,
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: t.into() }],
            kind: PromptKind::default(),
        }
    }
    fn assistant(t: &str) -> Message {
        Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: t.into() }],
            kind: PromptKind::default(),
        }
    }
    fn exec_call(id: &str, code: &str) -> Message {
        Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: "exec".into(),
                input: serde_json::json!({ "code": code }),
            }],
            kind: PromptKind::default(),
        }
    }
    fn exec_result(id: &str, value: &str) -> Message {
        Message {
            origin: None,
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: serde_json::json!({ "value": value, "logs": [] }).to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        }
    }
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
    fn events_of(msgs: &[Message]) -> Vec<SessionEvent> {
        let mut out = Vec::with_capacity(msgs.len());
        for message in msgs {
            push_event(&mut out, SessionEventKind::Message(message.clone()));
        }
        out
    }

    fn push_event(events: &mut Vec<SessionEvent>, kind: SessionEventKind) {
        events.push(SessionEvent {
            id: format!("e{}", events.len()),
            parent_id: events.last().map(|event| event.id.clone()),
            kind,
        });
    }

    #[test]
    fn compaction_output_and_repeated_merge_match_fixtures() {
        let messages = [
            user("Please fix src/auth.rs."),
            exec_call("t1", "lofi.edit"),
            exec_result("t1", "ok"),
            assistant("Fixed src/auth.rs."),
            assistant("Tests passed."),
            user("Keep the public API unchanged."),
            assistant("Added tests."),
        ];
        let mut events = events_of(&messages);
        push_event(
            &mut events,
            SessionEventKind::NativeTool(native("t1", "edit", "src/auth.rs")),
        );
        let first = compact(&events, &CompactOptions::default()).unwrap();
        assert_eq!(
            first.summary,
            include_str!("compact/fixtures/first.txt").trim_end()
        );
        assert_eq!(first.summarized_count, 5);
        assert_eq!(first.represented_count, 5);
        assert_eq!(first.kept_count, 2);
        assert_eq!(first.first_kept_event_id.as_deref(), Some("e5"));
        assert_eq!(first.summarized_range, Some(["e0".into(), "e4".into()]));
        assert_eq!(
            serde_json::to_value(&first.kept_messages).unwrap(),
            serde_json::to_value(&messages[5..]).unwrap()
        );
        push_event(
            &mut events,
            SessionEventKind::Compaction {
                summary: first.summary,
                first_kept_entry_id: "e5".into(),
                summarized_range: ["e0".into(), "e4".into()],
                checkpointed_tail: false,
                summarized: 5,
                represented: 5,
                kept: 2,
            },
        );
        push_event(&mut events, SessionEventKind::Message(user("Next task")));
        push_event(&mut events, SessionEventKind::Message(assistant("Ready")));
        let second = compact(&events, &CompactOptions::default()).unwrap();
        assert_eq!(
            second.summary,
            include_str!("compact/fixtures/repeated.txt").trim_end()
        );
        assert_eq!(second.summarized_count, 2);
        assert_eq!(second.represented_count, 7);
        assert_eq!(second.kept_count, 2);
        assert_eq!(second.first_kept_event_id.as_deref(), Some("e9"));
        assert_eq!(second.summarized_range, Some(["e5".into(), "e6".into()]));
        assert_eq!(
            serde_json::to_value(&second.kept_messages).unwrap(),
            serde_json::to_value([user("Next task"), assistant("Ready")]).unwrap()
        );
    }

    #[test]
    fn compact_returns_none_for_too_few() {
        let events = events_of(&[user("hi"), assistant("hello")]);
        assert!(compact(&events, &CompactOptions::default()).is_none());
    }

    #[test]
    fn compact_counts_system_boundary_without_summarizing_it() {
        let events = events_of(&[
            Message {
                origin: None,
                role: Role::System,
                blocks: vec![ContentBlock::Text {
                    text: "instructions".into(),
                }],
                kind: PromptKind::default(),
            },
            user("prompt one"),
            assistant("answer one"),
            user("prompt two"),
            assistant("answer two"),
            user("latest prompt"),
            assistant("latest answer"),
        ]);

        let compacted = compact(&events, &CompactOptions::default())
            .expect("the system boundary contributes to the history guard");

        assert_eq!(compacted.summarized_count, 5);
        assert!(!compacted.summary.contains("instructions"));
        assert_eq!(
            compacted.kept_messages.first().map(user_text).as_deref(),
            Some("latest prompt")
        );
    }

    #[test]
    fn previous_compaction_restores_summarized_message_count() {
        let mut events = events_of(&[
            user("old prompt"),
            assistant("old response"),
            assistant("old follow-up"),
            assistant("old result"),
            assistant("old conclusion"),
        ]);
        events.push(SessionEvent {
            id: "marker".to_string(),
            parent_id: Some("e4".to_string()),
            kind: SessionEventKind::Compaction {
                summary: "previous summary".to_string(),
                first_kept_entry_id: String::new(),
                summarized_range: ["e0".to_string(), "e4".to_string()],
                checkpointed_tail: true,
                summarized: 5,
                represented: 5,
                kept: 0,
            },
        });
        events.push(SessionEvent {
            id: "e6".to_string(),
            parent_id: Some("marker".to_string()),
            kind: SessionEventKind::Message(assistant("continued work")),
        });
        events.push(SessionEvent {
            id: "e7".to_string(),
            parent_id: Some("e6".to_string()),
            kind: SessionEventKind::Message(assistant("continued result")),
        });

        let compacted = compact(&events, &CompactOptions::default())
            .expect("restored summarized count must satisfy the history guard");
        assert_eq!(compacted.summarized_count, 2);
        assert_eq!(compacted.represented_count, 7);
        assert_eq!(compacted.kept_count, 0);
    }

    #[test]
    fn compact_excludes_failed_turn_without_dropping_the_next_success() {
        let mut events = events_of(&[
            user("baseline prompt one"),
            assistant("baseline answer one"),
            user("baseline prompt two"),
            assistant("baseline answer two"),
            assistant("baseline conclusion"),
        ]);
        push_event(
            &mut events,
            SessionEventKind::TurnEnd {
                model: "m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: lofi_types::Usage::default(),
                stop_reason: None,
            },
        );
        push_event(
            &mut events,
            SessionEventKind::Message(user("failed prompt")),
        );
        push_event(
            &mut events,
            SessionEventKind::Message(assistant("failed partial")),
        );
        push_event(
            &mut events,
            SessionEventKind::TurnFailed {
                model: "m".into(),
                elapsed_ms: 1,
                error: "boom".into(),
                cost: 0.0,
                usage: lofi_types::Usage::default(),
                keep_context: false,
            },
        );
        push_event(
            &mut events,
            SessionEventKind::Message(user("successful prompt")),
        );
        push_event(
            &mut events,
            SessionEventKind::Message(assistant("successful answer")),
        );
        push_event(
            &mut events,
            SessionEventKind::TurnEnd {
                model: "m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: lofi_types::Usage::default(),
                stop_reason: None,
            },
        );
        push_event(
            &mut events,
            SessionEventKind::Message(user("latest prompt")),
        );
        push_event(
            &mut events,
            SessionEventKind::Message(assistant("latest answer")),
        );
        push_event(
            &mut events,
            SessionEventKind::TurnEnd {
                model: "m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: lofi_types::Usage::default(),
                stop_reason: None,
            },
        );

        let compacted = compact(&events, &CompactOptions::default()).expect("some compaction");

        assert!(compacted.summary.contains("successful prompt"));
        assert!(compacted.summary.contains("successful answer"));
        assert!(!compacted.summary.contains("failed prompt"));
        assert!(!compacted.summary.contains("failed partial"));
        assert_eq!(
            compacted.kept_messages.first().map(user_text).as_deref(),
            Some("latest prompt")
        );
    }

    #[test]
    fn compact_refuses_when_too_little_to_fold() {
        let events = events_of(&[
            user("hi"),
            assistant("hello"),
            user("again"),
            assistant("sure"),
        ]);
        assert!(compact(&events, &CompactOptions::default()).is_none());
    }

    #[test]
    fn compact_keeps_last_turn_and_summarizes_prefix() {
        let msgs = [
            user("Please add a login form to auth.ts"),
            assistant("I'll edit auth.ts to add the form."),
            exec_call("t1", "lofi.edit"),
            exec_result("t1", "ok"),
            assistant("Done."),
            user("Now add tests please"),
            assistant("Adding tests."),
        ];
        let events = events_of(&msgs);
        let c = compact(&events, &CompactOptions::default()).expect("some compaction");
        assert!(c.summarized_count >= 1);
        assert!(c.summary.starts_with(HANDOFF_PREAMBLE));
        assert!(c.summary.contains(RESULT_RECALL_INSTRUCTION));
        assert!(c.summary.contains("[Session Goal]"));
        assert!(c.summary.contains("login form"));
        assert_eq!(
            c.kept_messages.first().map(user_text).as_deref(),
            Some("Now add tests please")
        );
    }

    #[test]
    fn compact_drops_image_blocks() {
        // The byte-pressure recovery depends on this: an oversized image
        // payload stops before send, the UI force-compacts, and the continued
        // request must be small. That only holds because compaction drops
        // `Image` blocks from the summarized prefix. If compaction ever
        // starts keeping images, the recovery loops forever — this test pins
        // the contract.
        let with_image = Message {
            origin: None,
            role: Role::User,
            kind: PromptKind::default(),
            blocks: vec![
                ContentBlock::Text {
                    text: "look at this".into(),
                },
                ContentBlock::Image {
                    bytes: vec![0u8; 16],
                    media_type: "image/jpeg".into(),
                },
            ],
        };
        let msgs = [
            with_image,
            assistant("I see it."),
            exec_call("t1", "return 1"),
            exec_result("t1", "1"),
            assistant("Done."),
            user("next"),
            assistant("ok"),
        ];
        let events = events_of(&msgs);
        let c = compact(&events, &CompactOptions::default()).expect("some compaction");
        let history = compacted_history(&c);
        assert!(
            !history
                .iter()
                .flat_map(|m| m.blocks.iter())
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "compacted history must not carry Image blocks"
        );
    }

    #[test]
    fn compact_files_and_changes_from_native_tools() {
        let mut events = events_of(&[
            user("edit auth.ts"),
            exec_call("t1", "lofi.edit"),
            exec_result("t1", "ok"),
            assistant("done"),
            user("thanks"),
            assistant("ok"),
            user("also add a test"),
            assistant("sure"),
        ]);
        events.push(SessionEvent {
            id: "n1".to_string(),
            parent_id: Some("e7".to_string()),
            kind: SessionEventKind::NativeTool(native("t1", "edit", "auth.ts")),
        });
        let c = compact(&events, &CompactOptions::default()).expect("some compaction");
        assert!(c.summary.contains("[Files And Changes]"));
        assert!(c.summary.contains("Modified: auth.ts"));
    }

    #[test]
    fn summary_stays_within_token_budget() {
        // Build a conversation with many turns so the brief transcript
        // would be large without the token budget.
        let big = "x".repeat(2000);
        let mut msgs = Vec::new();
        msgs.push(user("do a big task"));
        for i in 0..40 {
            msgs.push(exec_call(&format!("t{i}"), "lofi.read"));
            msgs.push(exec_result(&format!("t{i}"), &big));
            msgs.push(assistant(&"step done with lots of text ".repeat(5)));
        }
        msgs.push(user("now summarize"));
        msgs.push(assistant("done"));
        let events = events_of(&msgs);
        let c = compact(&events, &CompactOptions::default()).expect("should compact");
        let summary_chars = c.summary.len();
        assert!(
            summary_chars < 20_000,
            "summary is {summary_chars} chars, should be under ~16k"
        );
    }

    #[test]
    fn re_compact_after_compact_all_ignores_summarized_messages() {
        let mut events = events_of(&[user("OLD MESSAGE MUST STAY SUMMARIZED"), assistant("old")]);
        events.push(SessionEvent {
            id: "c1".to_string(),
            parent_id: Some("e1".to_string()),
            kind: SessionEventKind::Compaction {
                summary: format!("{HANDOFF_PREAMBLE}\n\n[prior summary]"),
                first_kept_entry_id: String::new(),
                summarized_range: ["e0".to_string(), "e1".to_string()],
                checkpointed_tail: false,
                summarized: 2,
                represented: 2,
                kept: 0,
            },
        });
        let new_messages = [
            user("new one"),
            assistant("reply one"),
            user("new two"),
            assistant("reply two"),
            user("new three"),
            assistant("reply three"),
            user("new four"),
            assistant("reply four"),
        ];
        let mut parent = "c1".to_string();
        for (i, message) in new_messages.into_iter().enumerate() {
            let id = format!("n{i}");
            events.push(SessionEvent {
                id: id.clone(),
                parent_id: Some(parent),
                kind: SessionEventKind::Message(message),
            });
            parent = id;
        }

        let c = compact(&events, &CompactOptions::default()).expect("compaction should succeed");
        assert!(c.summary.contains("[prior summary]"));
        assert!(!c.summary.contains("OLD MESSAGE MUST STAY SUMMARIZED"));
    }

    #[test]
    fn re_compact_edits_old_kept_tail_from_disk() {
        let big_result = "x".repeat(10_000);
        let mut events = events_of(&[
            user("do task"),
            exec_call("t1", "lofi.read"),
            exec_result("t1", &big_result),
            assistant("done"),
            user("now continue"),
            exec_call("t2", "lofi.read"),
            exec_result("t2", &big_result),
            assistant("ok"),
        ]);
        events.push(SessionEvent {
            id: "c1".to_string(),
            parent_id: Some("e7".to_string()),
            kind: SessionEventKind::Compaction {
                summary: format!("{HANDOFF_PREAMBLE}\n\n[prior summary]"),
                first_kept_entry_id: "e4".to_string(),
                summarized_range: ["e0".to_string(), "e3".to_string()],
                checkpointed_tail: false,
                summarized: 4,
                represented: 4,
                kept: 4,
            },
        });
        events.push(SessionEvent {
            id: "e8".to_string(),
            parent_id: Some("c1".to_string()),
            kind: SessionEventKind::Message(user("what did you do?")),
        });
        events.push(SessionEvent {
            id: "e9".to_string(),
            parent_id: Some("e8".to_string()),
            kind: SessionEventKind::Message(assistant("I read a file")),
        });
        events.push(SessionEvent {
            id: "e10".to_string(),
            parent_id: Some("e9".to_string()),
            kind: SessionEventKind::Message(user("ok now add tests")),
        });
        events.push(SessionEvent {
            id: "e11".to_string(),
            parent_id: Some("e10".to_string()),
            kind: SessionEventKind::Message(exec_call("t3", "lofi.write")),
        });
        events.push(SessionEvent {
            id: "e12".to_string(),
            parent_id: Some("e11".to_string()),
            kind: SessionEventKind::Message(exec_result("t3", &big_result)),
        });
        events.push(SessionEvent {
            id: "e13".to_string(),
            parent_id: Some("e12".to_string()),
            kind: SessionEventKind::Message(assistant("Done adding tests")),
        });

        let opts = CompactOptions {
            edit: lofi_types::EditConfig {
                enabled: true,
                keep_results: 1,
                keep_thinking: 0,
                keep_calls: 1,
            },
            ..Default::default()
        };

        let c = compact(&events, &opts).expect("compaction should succeed");

        assert!(c.summary.contains("[prior summary]"));
        assert!(
            !c.summary.contains(&"x".repeat(100)),
            "summary should not contain the big result from the old kept tail"
        );

        assert!(
            c.summary.contains("cleared") || !c.summary.contains(&big_result),
            "old kept tail results should be stubbed, not carried verbatim into summary"
        );
    }

    #[test]
    fn compact_single_prompt_oversized_turn() {
        // One user prompt followed by 6 exec tool cycles (12 messages).
        // Each exec result is 20k chars → ~30k tokens total, well over the
        // 2k-token budget. The oversized-turn guard must split at a
        // completed tool-cycle so the leading messages are summarized.
        let big = "x".repeat(20_000);
        let msgs = [
            user("do a big task"),
            exec_call("t1", "lofi.read big1"),
            exec_result("t1", &big),
            exec_call("t2", "lofi.read big2"),
            exec_result("t2", &big),
            exec_call("t3", "lofi.read big3"),
            exec_result("t3", &big),
            exec_call("t4", "lofi.read big4"),
            exec_result("t4", &big),
            exec_call("t5", "lofi.read big5"),
            exec_result("t5", &big),
            exec_call("t6", "lofi.read big6"),
            exec_result("t6", "ok"),
        ];
        let events = events_of(&msgs);
        let opts = CompactOptions {
            max_kept_tokens: 2_000,
            ..Default::default()
        };
        let c = compact(&events, &opts).expect("should compact single oversized turn");
        assert!(
            c.summarized_count >= MIN_SUMMARIZED,
            "summarized {} should be >= {MIN_SUMMARIZED}",
            c.summarized_count
        );
        assert!(c.summary.contains("do a big task"));
        assert!(!c.kept_messages.is_empty(), "should keep a tail");
    }
}
