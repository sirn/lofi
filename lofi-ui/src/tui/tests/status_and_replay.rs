use super::*;

#[test]
fn turn_failed_wraps_error_below_header() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::TurnFailed {
            label: "openai/gpt-4o · medium".to_string(),
            elapsed: Duration::from_secs(12),
            error: "the provider returned a 503 with a very long service-unavailable message that must wrap".to_string(),
        }],
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 40,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // Line 0 is the status header: model, level, duration only — the error
    // must not appear inline there (it used to, and got clipped).
    let header: String = rls[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        header.contains("Failed in"),
        "header missing status: {header}"
    );
    assert!(
        header.contains("openai/gpt-4o"),
        "header missing label: {header}"
    );
    assert!(
        !header.contains("503"),
        "header must not carry the error inline: {header}"
    );
    let body: String = rls[1..]
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        body.contains("503"),
        "wrapped error missing from body: {body}"
    );
    for rl in &rls[1..] {
        assert!(
            rl.line.width() <= 40,
            "body line overflows: {}",
            rl.line.width()
        );
    }
}

#[test]
fn turn_failed_dedups_after_fatal_error_block() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    let msg = "stream interrupted by upstream gateway";
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![
            Block::Error(msg.to_string()),
            Block::TurnFailed {
                label: "openai/gpt-4o · medium".to_string(),
                elapsed: Duration::from_secs(3),
                error: format!("provider error: {msg}"),
            },
        ],
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // The provider emits the error twice on a stream failure — as a fatal
    // `✗` line and again in the TurnFailed marker. The dedup must collapse
    // them so the message appears exactly once across the rendered turn.
    let all: String = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        all.matches("stream interrupted").count(),
        1,
        "dedup failed: {all}"
    );
}

#[test]
fn exec_result_wraps_long_lines_instead_of_truncating() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.bash('echo t')".to_string(),
        label: Some("echo".to_string()),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "bash".to_string(),
        args: "echo t".to_string(),
    });
    let token = "abcdefghijklmnopqrstuvwxyz1234567890abcdefghij";
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 0,
        result: serde_json::json!({ "ok": true, "output": token, "code": 0 }).to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    a.expanded_details.insert(
        DetailKey::NativeTool {
            parent: 0,
            id: 0,
        },
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 28,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    for rl in &rls {
        assert!(
            rl.line.width() <= 28,
            "row overflows 28: {}",
            rl.line.width()
        );
    }
    let all = rls
        .iter()
        .map(|rl| {
            rl.line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        all.contains("hij"),
        "result tail clipped (no wrapping): {all}"
    );
}

#[test]
fn footer_shows_model_and_thinking() {
    let a = App::new(
        "openai/gpt-5.6-sol".to_string(),
        ThinkingLevel::XHigh,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("gpt-5.6-sol"));
    assert!(r.contains(":xhigh"));
}

#[test]
fn footer_hides_thinking_when_off() {
    let a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Off,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert_eq!(r, "openai/gpt-4o");
}

#[test]
fn footer_shows_ctx_after_usage() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.0,
        usage: Usage {
            input_tokens: 40_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    let r: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("context 40k/200k"), "footer: {r}");
}

#[test]
fn thinking_timing_is_restored_from_transcript() {
    let assistant_with_thinking = Message {
        role: Role::Assistant,
        blocks: vec![
            ContentBlock::Thinking {
                text: "hm".to_string(),
                signature: None,
                redacted: false,
            },
            ContentBlock::Text {
                text: "ok".to_string(),
            },
        ],
        kind: PromptKind::default(),
    };
    let events = sev_chain([
        msg(user("hi")),
        msg(assistant_with_thinking),
        SessionEventKind::ThinkingTiming { elapsed_ms: 1234 },
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let thinking = turns[0]
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Thinking(t) => Some(t),
            _ => None,
        })
        .expect("thinking block");
    assert_eq!(thinking.elapsed, Some(Duration::from_millis(1234)));
}

#[test]
fn turns_from_events_round_trip() {
    let events = sev_chain([
        msg(user("hello")),
        msg(assistant("hi there")),
        msg(user("again")),
        msg(assistant("yep")),
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].prompt, "hello");
    assert_eq!(turns[1].prompt, "again");
    assert!(matches!(turns[0].blocks[0], Block::Text(_)));
}

#[test]
fn selected_replay_preserves_prompt_when_compaction_parent_was_filtered_out() {
    let events = vec![
        SessionEvent {
            id: "prompt".into(),
            parent_id: Some("older".into()),
            kind: msg(user("current prompt")),
        },
        SessionEvent {
            id: "reply".into(),
            parent_id: Some("prompt".into()),
            kind: msg(assistant("current reply")),
        },
        SessionEvent {
            id: "marker".into(),
            parent_id: Some("filtered-checkpoint-copy".into()),
            kind: SessionEventKind::Compaction {
                summary: "summary".into(),
                first_kept_entry_id: "filtered-checkpoint-copy".into(),
                summarized_range: ["a".into(), "b".into()],
                checkpointed_tail: true,
                summarized: 1,
                represented: 1,
                kept: 1,
            },
        },
        SessionEvent {
            id: "continued".into(),
            parent_id: Some("marker".into()),
            kind: msg(assistant("continued reply")),
        },
    ];

    let turns = turns_from_selected_session_events(&events);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].prompt, "current prompt");
    let rendered = turns[0]
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("current reply"));
    assert!(rendered.contains("continued reply"));
}

#[test]
fn checkpointed_tail_is_hidden_from_ui_but_used_for_model_resume() {
    let events = sev_chain([
        msg(user("old prompt")),
        msg(assistant("old reply")),
        msg(user("kept prompt")),
        msg(assistant("kept reply")),
        msg(user("kept prompt")),
        msg(assistant("kept reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: "e4".to_string(),
            summarized_range: ["e0".to_string(), "e1".to_string()],
            checkpointed_tail: true,
            summarized: 2,
            represented: 2,
            kept: 2,
        },
        msg(assistant("continued")),
    ]);

    let turns = turns_from_session_events(&events);
    assert_eq!(
        turns.len(),
        2,
        "checkpoint copies must not duplicate UI turns"
    );
    assert_eq!(turns[0].prompt, "old prompt");
    assert_eq!(turns[1].prompt, "kept prompt");

    let offsets: Vec<u64> = (0..events.len()).map(|i| i as u64 * 10).collect();
    assert_eq!(
        turn_byte_ranges_from_events(&events, &offsets, 80),
        vec![Some((0, 20)), Some((20, 80))]
    );

    let messages = messages_from_events(&events);
    assert_eq!(messages.len(), 4);
    assert_eq!(user_text(&messages[0]), "SUMMARY");
    assert_eq!(user_text(&messages[1]), "kept prompt");
    assert_eq!(user_text(&messages[2]), "kept reply");
    assert_eq!(user_text(&messages[3]), "continued");
}

#[test]
fn turns_from_events_links_tool_results() {
    let messages = vec![
        user("run it"),
        Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "ok".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"cmd": "ls"}),
                },
            ],
            kind: PromptKind::default(),
        },
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "file.txt".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        },
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
            kind: PromptKind::default(),
        },
    ];
    let events: Vec<SessionEvent> = sev_chain(messages.into_iter().map(msg));
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let blocks = &turns[0].blocks;
    assert!(matches!(blocks[0], Block::Text(_)));
    let Block::Tool(t) = &blocks[1] else {
        unreachable!()
    };
    assert_eq!(t.result.as_deref(), Some("file.txt"));
    assert!(t.done);
    assert!(matches!(blocks[2], Block::Text(_)));
}

#[test]
fn turns_from_events_restores_timings() {
    let events = sev_chain([
        msg(user("run it")),
        msg(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "exec".to_string(),
                input: serde_json::json!({"code": "return 1"}),
            }],
            kind: PromptKind::default(),
        }),
        msg(Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "1".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        }),
        msg(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::ToolTiming {
            tool_call_id: "t1".into(),
            elapsed_ms: 7,
        },
        SessionEventKind::TurnEnd {
            model: "proxy/gemini-3-flash · medium".into(),
            elapsed_ms: 2000,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let blocks = &turns[0].blocks;
    let Block::Tool(t) = &blocks[0] else {
        unreachable!()
    };
    assert_eq!(t.elapsed, Some(Duration::from_millis(7)));
    match blocks.last() {
        Some(Block::TurnEnd { label, elapsed }) => {
            assert_eq!(label, "proxy/gemini-3-flash:medium");
            assert_eq!(*elapsed, Duration::from_secs(2));
        }
        other => panic!("expected TurnEnd, got {other:?}"),
    }
}

#[test]
fn messages_from_events_excludes_failed_turn_branch() {
    // Build a tree: root chain [user1, assistant1, TurnEnd1], then a
    // failed turn chained linearly off TurnEnd1: [user2, assistant2,
    // TurnFailed]. The TurnFailed marker is the active leaf. The active
    // path INCLUDES the failed turn's messages (so the UI can render
    // them), but `messages_from_events` must EXCLUDE them from the
    // agent's history via the TurnFailed boundary — the model resumes
    // from the checkpoint (TurnEnd1), not the failed partial content.
    use lofi_core::session::store::active_path_from_leaf;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"meta\",\"version\":1,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n",
    )
    .unwrap();

    // First (successful) turn: two messages + a TurnEnd, chained from
    // the root so append_events assigns linear ids.
    let mut t1_events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("hi")),
        SessionEventKind::Message(assistant("hello")),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 10,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut t1_events, None).unwrap();
    let checkpoint = t1_events.last().unwrap().id.clone();

    let mut t2_events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("oops")),
        SessionEventKind::Message(assistant("partial")),
        SessionEventKind::TurnFailed {
            model: "m".into(),
            elapsed_ms: 5,
            error: "boom".into(),
            cost: 0.01,
            usage: Usage::default(),
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut t2_events, Some(&checkpoint)).unwrap();

    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let path_idx = active_path_from_leaf(&events);
    assert_eq!(path_idx.len(), 6, "active path includes failed turn's msgs");
    assert!(
        matches!(&events[path_idx[0]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "hi"))
    );
    assert!(
        matches!(&events[path_idx[1]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant)
    );
    assert!(matches!(
        &events[path_idx[2]].kind,
        SessionEventKind::TurnEnd { .. }
    ));
    assert!(
        matches!(&events[path_idx[3]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "oops"))
    );
    assert!(
        matches!(&events[path_idx[4]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant)
    );
    assert!(matches!(
        &events[path_idx[5]].kind,
        SessionEventKind::TurnFailed { .. }
    ));

    let msgs = messages_from_events(&events);
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
}

#[test]
fn messages_from_events_prepends_compaction_summary() {
    use lofi_core::session::store;
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"meta\",\"version\":1,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n",
    )
    .unwrap();

    let mut events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("old")),
        SessionEventKind::Message(user("kept-prompt")),
        SessionEventKind::Message(assistant("kept-reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: String::new(), // patched after append
            summarized_range: [String::new(), String::new()],
            checkpointed_tail: false,
            summarized: 1,
            represented: 1,
            kept: 2,
        },
        SessionEventKind::Message(assistant("continued")),
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut events, None).unwrap();
    let mut events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let kept_prompt_id = events
        .iter()
        .find(|e| matches!(&e.kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "kept-prompt")))
        .unwrap()
        .id
        .clone();
    for e in &mut events {
        if let SessionEventKind::Compaction {
            first_kept_entry_id,
            ..
        } = &mut e.kind
        {
            *first_kept_entry_id = kept_prompt_id.clone();
        }
    }

    let msgs = messages_from_events(&events);
    assert_eq!(msgs.len(), 4);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");
    assert_eq!(user_text(&msgs[1]), "kept-prompt");
    assert_eq!(msgs[2].role, Role::Assistant);
    assert_eq!(msgs[3].role, Role::Assistant);
}

#[test]
fn messages_from_events_compact_all_does_not_restore_old_messages() {
    let events = sev_chain([
        msg(user("old prompt")),
        msg(assistant("old reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: String::new(),
            summarized_range: ["e0".to_string(), "e1".to_string()],
            checkpointed_tail: false,
            summarized: 2,
            represented: 2,
            kept: 0,
        },
        msg(assistant("continued")),
    ]);

    let msgs = messages_from_events(&events);
    assert_eq!(msgs.len(), 2);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");
    assert_eq!(msgs[1].role, Role::Assistant);
    assert_eq!(user_text(&msgs[1]), "continued");
}

#[test]
fn messages_from_events_reads_kept_tail_verbatim_on_resume() {
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let exec_call_stub = |id: &str, eid: &str| Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": format!("[{eid}]")}),
        }],
        kind: PromptKind::default(),
    };
    let exec_result_stub = |id: &str, eid: &str| Message {
        role: Role::User,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: format!("[{eid}]"),
            is_error: false,
            images: Vec::new(),
        }],
        kind: PromptKind::default(),
    };
    let exec_call_full = |id: &str| Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": "return 1"}),
        }],
        kind: PromptKind::default(),
    };
    let exec_result_full = |id: &str, out: &str| Message {
        role: Role::User,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: out.to_string(),
            is_error: false,
            images: Vec::new(),
        }],
        kind: PromptKind::default(),
    };

    let events: Vec<SessionEvent> = sev_chain([
        msg(user("old prompt")),
        msg(exec_call_stub("t1", "e1")),
        msg(exec_result_stub("t1", "e2")),
        msg(exec_call_stub("t2", "e3")),
        msg(exec_result_full("t2", "out-2")), // most recent kept-tail result, kept verbatim by edit_tail
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: "e1".to_string(),
            summarized_range: [String::new(), String::new()],
            checkpointed_tail: false,
            summarized: 1,
            represented: 1,
            kept: 4,
        },
        msg(exec_call_full("t3")),
        msg(exec_result_full("t3", "post-compaction-result")),
    ]);

    let msgs = messages_from_events(&events);
    assert_eq!(msgs.len(), 7);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");

    let ContentBlock::ToolResult { content, .. } = &msgs[4].blocks[0] else {
        panic!()
    };
    assert_eq!(content, "out-2");
    let ContentBlock::ToolResult { content, .. } = &msgs[2].blocks[0] else {
        panic!()
    };
    assert_eq!(content, "[e2]");

    let ContentBlock::ToolResult { content, .. } = &msgs[6].blocks[0] else {
        panic!()
    };
    assert_eq!(content, "post-compaction-result");
    let ContentBlock::ToolUse { input, .. } = &msgs[5].blocks[0] else {
        panic!()
    };
    let code = input.get("code").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(code, "return 1");
}

#[test]
fn compact_count_formats() {
    assert_eq!(compact_count(0), "0");
    assert_eq!(compact_count(500), "500");
    assert_eq!(compact_count(119_000), "119k");
    assert_eq!(compact_count(200_000), "200k");
    assert_eq!(compact_count(2_000_000), "2M");
    assert_eq!(compact_count(9_700_000), "9.7M");
}

#[test]
fn fmt_cost_two_decimals() {
    assert_eq!(fmt_cost(0.0), "$0.00");
    assert_eq!(fmt_cost(0.01), "$0.01");
    assert_eq!(fmt_cost(81.0), "$81.00");
    assert_eq!(fmt_cost(81.40), "$81.40");
    assert_eq!(fmt_cost(81.45), "$81.45");
}

#[test]
fn abbreviate_path_progressive() {
    let home = dirs::home_dir().unwrap();
    let p = home.join("Dev/src/proj");
    assert_eq!(abbreviate_path(&p, 100), "~/Dev/src/proj");
    assert_eq!(abbreviate_path(&p, 10), "~/D/s/proj");
    assert_eq!(abbreviate_path(&p, 4), "proj");
    let p2 = home.join("Dev/~sirn/lofi");
    assert_eq!(abbreviate_path(&p2, 12), "~/D/~s/lofi");
}

#[test]
fn footer_and_header_show_cost_and_usage() {
    let mut a = App::new(
        "proxy/deepseek-v4-flash".to_string(),
        ThinkingLevel::Off,
        ServiceTier::Auto,
        200_000,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 18.0,
        usage: Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    let footer: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(footer.contains("↑1M ↓1M"), "footer: {footer}");
    assert!(footer.contains("context 2M/200k"), "footer: {footer}");
    let cost: String = a
        .render_footer_cost()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(cost.contains("$18.00"), "cost: {cost}");
    let header: String = a
        .render_header_line(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(header.contains("lofi"), "header: {header}");
    assert!(
        !header.contains('$'),
        "header should not show cost: {header}"
    );
}

#[test]
fn footer_shows_cache_percentage_without_cumulative_cache_counts() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 1.0,
        usage: Usage {
            input_tokens: 100_000,
            output_tokens: 50_000,
            cache_read_tokens: 800_000,
            cache_write_tokens: 200_000,
        },
        stop_reason: None,
    });
    let footer: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(footer.contains("73% cached"), "footer: {footer}");
    assert!(!footer.contains("cache ↑"), "footer: {footer}");
}

#[test]
fn retry_notice_is_transient_status_and_resets_on_success() {
    let mut a = app();
    push_turn(&mut a);
    let before = a.turns[0].blocks.len();

    a.apply_event(AgentEvent::RetryStart {
        attempt: 2,
        max_attempts: 10,
        delay_ms: 42_000,
        error: "provider error: HTTP 500 Internal Server Error".to_string(),
    });

    assert_eq!(a.retry_badge().as_deref(), Some("Retry: 2 of 10"));
    assert_eq!(
        a.turns[0].blocks.len(),
        before,
        "retry status must not enter the transcript"
    );

    a.apply_event(AgentEvent::RetryEnd {
        success: true,
        attempt: 2,
        final_error: None,
    });
    assert!(a.retry_badge().is_none());
    assert_eq!(a.turns[0].blocks.len(), before);
}

#[test]
fn queue_badge_shows_preview_and_count() {
    let mut a = app();
    assert!(a.queue_badge().is_none());
    a.prompt_queue.push(QueuedPrompt {
        text: "fix the bug".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    let badge = a.queue_badge().expect("badge for one item");
    assert!(badge.contains("Queue: fix the bug"), "badge: {badge}");
    a.prompt_queue.push(QueuedPrompt {
        text: "also add tests".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    let badge = a.queue_badge().expect("badge for two items");
    assert!(badge.contains("Queue: fix the bug (+1)"), "badge: {badge}");
}

#[test]
fn interrupt_run_restores_user_prompts_and_keeps_notices_queued() {
    // A Notice that arrives while a run is live must not enter the editor,
    // but it must survive cancellation for the next agent round.
    let mut a = app();
    a.prompt_queue.push(QueuedPrompt {
        text: "user follow-up".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "job 1786 completed: sleep 1".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    restore_queued_prompts(&mut a);
    assert_eq!(a.input, "user follow-up");
    assert!(!a.input.contains("job 1786"));
    assert_eq!(a.prompt_queue.len(), 1);
    assert_eq!(a.prompt_queue[0].kind, lofi_types::PromptKind::Notice);
    assert!(a.prompt_queue[0].text.contains("job 1786"));
}

#[test]
fn queue_badge_marks_notice_with_hollow_bullet() {
    let mut a = app();
    a.prompt_queue.push(QueuedPrompt {
        text: "job 1786 completed: sleep 1".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let badge = a.queue_badge().expect("badge");
    assert!(
        badge.starts_with("Queue: ▷ "),
        "queued Notice should render with the hollow bullet, got: {badge}"
    );
}

#[test]
fn queue_badge_truncates_long_prompt() {
    let mut a = app();
    let long = "x".repeat(100);
    a.prompt_queue.push(QueuedPrompt {
        text: long,
        kind: lofi_types::PromptKind::User,
    });
    let badge = a.queue_badge().expect("badge");
    assert!(
        badge.ends_with("…"),
        "badge should end with ellipsis: {badge}"
    );
    assert!(
        !badge.contains(&"x".repeat(50)),
        "badge should be truncated: {badge}"
    );
}

#[test]
fn alt_up_restores_queued_prompt_lifo() {
    let mut a = app();
    let mut run = None;
    a.prompt_queue.push(QueuedPrompt {
        text: "first prompt".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "second prompt".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "second prompt");
    assert_eq!(a.prompt_queue.len(), 1);
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "first prompt");
    assert!(a.prompt_queue.is_empty());
}

#[test]
fn alt_up_skips_notices_when_restoring_queue() {
    // Notices are not user input — Alt+Up should never land one in the
    // editor, even if it's newer than the latest User prompt.
    let mut a = app();
    let mut run = None;
    a.prompt_queue.push(QueuedPrompt {
        text: "user text".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.prompt_queue.push(QueuedPrompt {
        text: "job 1786 completed".to_string(),
        kind: lofi_types::PromptKind::Notice,
    });
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "user text");
    // The Notice remains queued — only the User entry was popped.
    assert_eq!(a.prompt_queue.len(), 1);
    assert_eq!(a.prompt_queue[0].kind, lofi_types::PromptKind::Notice);
}

#[test]
fn no_model_submit_surfaces_hint_without_running() {
    let mut a = app();
    a.no_models_hint = Some("set OPENAI_API_KEY".to_string());
    a.input = "hello".to_string();
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ));
    let mut run = None;
    handle_event(&ev, &mut a, None, &mut run);
    assert!(run.is_none(), "no run should be started without a model");
    assert!(a.run.is_none());
    let last = a.turns.last().unwrap();
    assert_eq!(last.prompt, "hello");
    assert!(
        last.blocks
            .iter()
            .any(|b| matches!(b, Block::Error(m) if m == "set OPENAI_API_KEY")),
        "the hint should be attached to the turn"
    );
}
