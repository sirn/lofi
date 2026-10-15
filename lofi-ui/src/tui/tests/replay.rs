use super::*;

#[test]
fn thinking_timing_is_restored_from_transcript() {
    let assistant_with_thinking = Message {
        origin: None,
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
            origin: None,
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
            origin: None,
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
            origin: None,
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
            origin: None,
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "exec".to_string(),
                input: serde_json::json!({"code": "return 1"}),
            }],
            kind: PromptKind::default(),
        }),
        msg(Message {
            origin: None,
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
            origin: None,
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
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": format!("[{eid}]")}),
        }],
        kind: PromptKind::default(),
    };
    let exec_result_stub = |id: &str, eid: &str| Message {
        origin: None,
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
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": "return 1"}),
        }],
        kind: PromptKind::default(),
    };
    let exec_result_full = |id: &str, out: &str| Message {
        origin: None,
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
