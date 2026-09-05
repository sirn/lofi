use super::*;

fn last_text(app: &App) -> Option<&str> {
    app.turns.last()?.blocks.iter().rev().find_map(|b| match b {
        Block::Text(t) => Some(t.as_str()),
        _ => None,
    })
}

#[test]
fn agent_notice_prompt_enters_transcript_not_app_notification() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("repeating".into()));
    a.turn_cost = 1.25;
    a.turn_has_round_usage = true;

    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::Notice,
        prompt: "A potential loop was detected".into(),
    });

    assert_eq!(a.turns.len(), 2);
    assert_eq!(a.turns[1].kind, lofi_types::PromptKind::Notice);
    assert_eq!(a.turns[1].prompt, "A potential loop was detected");
    assert!(a.notify.is_none());
    assert_eq!(a.turn_cost, 1.25);
    assert!(a.turn_has_round_usage);
}

#[test]
fn job_and_loop_notices_use_the_same_transcript_prompt() {
    let notice = || AgentEvent::Prompt {
        kind: lofi_types::PromptKind::Notice,
        prompt: "job or loop notice".into(),
    };

    let mut job = app();
    job.apply_event(AgentEvent::RunStart);
    job.apply_event(notice());

    let mut loop_recovery = app();
    loop_recovery.apply_event(AgentEvent::RunStart);
    loop_recovery.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    loop_recovery.apply_event(notice());

    let job_notice = job.turns.last().unwrap();
    let loop_notice = loop_recovery.turns.last().unwrap();
    assert_eq!(job_notice.kind, loop_notice.kind);
    assert_eq!(job_notice.prompt, loop_notice.prompt);
    assert!(job.notify.is_none());
    assert!(loop_recovery.notify.is_none());
}

#[test]
fn standalone_user_shell_keeps_turn_backing_metadata_aligned() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "first".into(),
    });
    a.apply_event(AgentEvent::Text("answer".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 10,
        byte_end: 20,
    });

    a.apply_event(AgentEvent::UserShell {
        command: "pwd".into(),
        output: "/tmp".into(),
        exit_code: Some(0),
        signal: None,
        duration_ms: 1,
        truncated: false,
        cancelled: false,
        exclude_from_context: false,
    });

    assert_eq!(a.turns.len(), 2);
    assert_eq!(a.turn_byte_ranges.len(), 2);
    assert_eq!(a.turn_event_offsets.len(), 2);
    assert!(a.turns[0].blocks.is_empty());

    a.turn_byte_ranges[1] = Some((20, 30));
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "second".into(),
    });
    assert!(a.turns[1].blocks.is_empty());
    assert_eq!(a.turns.len(), a.turn_byte_ranges.len());
    assert_eq!(a.turns.len(), a.turn_event_offsets.len());
}

#[test]
fn user_shell_streams_into_one_turn_and_finalizes_in_place() {
    let mut a = app();
    a.apply_event(AgentEvent::UserShellStart {
        command: "top".into(),
        exclude_from_context: false,
    });
    a.apply_event(AgentEvent::UserShellDelta("first\n".into()));
    a.apply_event(AgentEvent::UserShellDelta("second\n".into()));
    a.apply_event(AgentEvent::UserShell {
        command: "top".into(),
        output: "first\nsecond".into(),
        exit_code: Some(1),
        signal: None,
        duration_ms: 5,
        truncated: false,
        cancelled: false,
        exclude_from_context: false,
    });

    assert_eq!(a.turns.len(), 1);
    assert_eq!(a.turn_byte_ranges.len(), 1);
    assert_eq!(a.turn_event_offsets.len(), 1);
    // Streaming inserts the expansion at `UserShellStart`; finalize must
    // keep the same detail id so the output stays expanded.
    assert!(a
        .expanded_details
        .contains_key(&DetailKey::UserShell(crate::tui::detail_block_id(0, 0))));
    let Some(Block::UserShell {
        id,
        output,
        exit_code,
        duration,
        running,
        ..
    }) = a.turns[0].blocks.first()
    else {
        panic!("expected a user-shell block");
    };
    assert_eq!(*id, crate::tui::detail_block_id(0, 0));
    assert_eq!(output, "first\nsecond");
    assert_eq!(*exit_code, Some(1));
    assert_eq!(*duration, Duration::from_millis(5));
    assert!(!running);
}

#[test]
fn resume_picker_attaches_selected_cursor_to_sink_and_ui() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let old = store.create_cursor(cwd, &"old".into()).unwrap();
    old.append_system("old system").unwrap();
    let selected = store.create_cursor(cwd, &"selected".into()).unwrap();
    selected.append_system("selected system").unwrap();
    let selected_path = selected.path().to_path_buf();
    let file = store
        .list_files_for_cwd(cwd)
        .unwrap()
        .into_iter()
        .find(|file| file.open_snapshot().unwrap().0.path() == selected_path)
        .unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, old);
    a.picker_confirm_inner(PickerState {
        entries: vec![PickerEntry {
            file,
            preview: None,
            details: None,
        }],
        selected: 0,
        generation: 0,
    });

    assert_eq!(a.session.cursor.as_ref().unwrap().path(), selected_path);
    assert_eq!(
        a.session.sink.as_ref().unwrap().cursor().unwrap().path(),
        selected_path
    );
}

#[test]
fn resumed_user_shell_and_final_turn_are_file_backed_shells() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let cursor = store
        .create_cursor(std::path::Path::new("/workspace"), &"p/m".into())
        .unwrap();
    let kinds = [
        msg(user("first")),
        msg(assistant("first answer")),
        SessionEventKind::TurnEnd {
            model: "p/m".into(),
            elapsed_ms: 1,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
        SessionEventKind::UserShell {
            command: "pwd".into(),
            output: "/workspace".into(),
            exit_code: Some(0),
            signal: None,
            duration_ms: 1,
            truncated: false,
            cancelled: false,
            exclude_from_context: false,
        },
        msg(user("second")),
        msg(assistant("final answer")),
        SessionEventKind::TurnEnd {
            model: "p/m".into(),
            elapsed_ms: 1,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let mut events: Vec<_> = kinds
        .into_iter()
        .map(|kind| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind,
        })
        .collect();
    cursor.append_events(&mut events).unwrap();
    let snapshot = cursor.snapshot().unwrap();

    let mut a = app();
    a.session.cursor = Some(cursor.clone());
    a.restore_indexed_session(
        &cursor,
        &snapshot.index,
        snapshot.file_size,
        snapshot.history_start,
        snapshot.contiguous,
    )
    .unwrap();

    assert_eq!(a.turns.len(), 3);
    assert_eq!(a.turns.len(), a.turn_byte_ranges.len());
    assert_eq!(a.turns.len(), a.turn_event_offsets.len());
    assert!(a.turns.iter().all(|turn| turn.blocks.is_empty()));
    let final_turn = a.materialize_turn(2);
    assert!(final_turn
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "final answer")));
}

#[test]
fn text_deltas_accumulate_into_one_text_block() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text("hel".to_string()));
    a.apply_event(AgentEvent::Text("lo".to_string()));
    assert_eq!(last_text(&a), Some("hello"));
    assert_eq!(a.turns[0].blocks.len(), 1);
}

#[test]
fn thinking_deltas_accumulate_into_their_own_block() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Thinking("hm".to_string()));
    a.apply_event(AgentEvent::Text("hi".to_string()));
    a.apply_event(AgentEvent::Thinking("more".to_string()));
    let blocks = &a.turns[0].blocks;
    assert!(matches!(blocks[0], Block::Thinking(_)));
    assert!(matches!(blocks[1], Block::Text(_)));
    assert!(matches!(blocks[2], Block::Thinking(_)));
    assert_eq!(blocks.len(), 3);
}

#[test]
fn tool_start_then_text_starts_new_text_block() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text("first".to_string()));
    a.apply_event(AgentEvent::ToolStart {
        id: "t1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::Text("second".to_string()));
    let blocks = &a.turns[0].blocks;
    assert_eq!(blocks.len(), 3);
    assert!(matches!(blocks[0], Block::Text(_)));
    assert!(matches!(blocks[1], Block::Tool(_)));
    assert!(matches!(blocks[2], Block::Text(_)));
}

#[test]
fn tool_input_and_end_land_under_matching_id() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "t1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolStart {
        id: "t2".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "t1".to_string(),
        code: "code-1".to_string(),
        label: None,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "t2".to_string(),
        result: "r2".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "t1".to_string(),
        result: "r1".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let blocks = &a.turns[0].blocks;
    let Block::Tool(t1) = &blocks[0] else {
        unreachable!()
    };
    let Block::Tool(t2) = &blocks[1] else {
        unreachable!()
    };
    assert_eq!(t1.input, "code-1");
    assert_eq!(t1.result.as_deref(), Some("r1"));
    assert!(t1.done);
    assert_eq!(t2.result.as_deref(), Some("r2"));
    assert!(t2.done);
}

#[test]
#[allow(clippy::too_many_lines)]
fn round_commit_releases_hidden_exec_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("round-commit.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"p/m\"}\n",
    )
    .unwrap();
    let cursor = store::SessionCursor::new(path, None);
    let durable_result = "{\"value\":\"durable outer result\"}";
    let mut events = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(user("go")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "e1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "code": "return 1" }),
                }],
                kind: PromptKind::default(),
            }),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "e1".into(),
                    content: durable_result.into(),
                    is_error: false,
                    images: Vec::new(),
                }],
                kind: PromptKind::default(),
            }),
        },
    ];
    let (start, end) = cursor.append_events(&mut events).unwrap();

    let mut a = app();
    a.session.cursor = Some(cursor);
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Thinking("still visible thought".into()));
    a.apply_event(AgentEvent::Text("still visible text".into()));
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".into(),
        name: "exec".into(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".into(),
        code: "return 1".into(),
        label: None,
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".into(),
        id: 0,
        name: "bash".into(),
        args: "printf native".into(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".into(),
        id: 0,
        result: "native result stays resident".into(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".into(),
        result: durable_result.into(),
        is_error: false,
        elapsed_ms: 1,
    });
    let block_count = a.turns[0].blocks.len();

    a.apply_event(AgentEvent::RoundCommitted {
        byte_start: start,
        byte_end: end,
    });

    assert_eq!(a.turns[0].prompt, "go");
    assert_eq!(a.turns[0].blocks.len(), block_count);
    assert!(
        matches!(&a.turns[0].blocks[0], Block::Thinking(thinking) if thinking.text == "still visible thought")
    );
    assert!(matches!(&a.turns[0].blocks[1], Block::Text(text) if text == "still visible text"));
    let Block::Tool(tool) = &a.turns[0].blocks[2] else {
        panic!("exec block preserved")
    };
    assert!(tool.result_committed);
    assert!(
        tool.result.is_none(),
        "collapsed hidden duplicate is released"
    );
    assert_eq!(
        tool.native[0].result.as_deref(),
        Some("native result stays resident")
    );
}

#[test]
fn turn_committed_extends_existing_range_across_silent_continuation() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });
    a.apply_event(AgentEvent::TurnContinue);
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 250,
        byte_end: 300,
    });
    assert_eq!(a.turn_byte_ranges, vec![Some((100, 300))]);
}

#[test]
fn run_finished_keeps_latest_persisted_turn_visible_until_next_prompt() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("visible response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });

    a.run_finished();

    assert!(a.turns[0]
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "visible response")));
    assert_eq!(a.turn_byte_ranges, vec![Some((100, 200))]);
}

#[test]
fn next_turn_freezes_previous_response_atomically_with_new_prompt() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "previous prompt".into(),
    });
    a.apply_event(AgentEvent::Text("previous response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });
    a.run_finished();

    assert!(a.turns[0]
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "previous response")));

    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "new prompt".into(),
    });
    assert_eq!(a.turns.len(), 2);
    assert!(a.turns[0].blocks.is_empty());
    assert_eq!(a.turns[1].prompt, "new prompt");
}

#[test]
fn settled_first_turn_remains_visible_from_committed_cursor_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let cursor = store
        .create_cursor(Path::new("/tmp/settled-first-turn"), &"p/m".into())
        .unwrap();
    let mut events = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::System,
                blocks: vec![ContentBlock::Text {
                    text: "system".into(),
                }],
                kind: PromptKind::default(),
            }),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(user("go")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(assistant("visible response")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        },
    ];
    let (start, end) = cursor.append_events(&mut events).unwrap();

    let mut a = app();
    a.session.cursor = Some(cursor);
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("visible response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: start,
        byte_end: end,
    });
    a.run_finished();

    assert!(a.turns[0]
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "visible response")));
}

#[test]
fn run_finished_keeps_blocks_for_hard_cap_continuation() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("partial response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });
    a.context_pressure = true;

    a.run_finished();

    assert!(!a.turns[0].blocks.is_empty());
}

#[test]
fn turn_end_updates_usage() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.0,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    assert_eq!(a.total_in, 10);
    assert_eq!(a.total_out, 20);
}

#[test]
fn round_usage_updates_totals_per_round() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        prompt: "p".into(),
        kind: lofi_types::PromptKind::User,
    });
    a.apply_event(AgentEvent::RoundUsage {
        cost: 0.01,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert_eq!(a.total_in, 100);
    assert_eq!(a.total_out, 50);
    assert_eq!(a.turn_cost, 0.01);
    assert!(a.turn_has_round_usage);
    assert!((a.cost + a.turn_cost - 0.01).abs() < 1e-9);

    a.apply_event(AgentEvent::RoundUsage {
        cost: 0.03,
        usage: Usage {
            input_tokens: 200,
            output_tokens: 80,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert_eq!(a.total_in, 300);
    assert_eq!(a.total_out, 130);
    assert!((a.turn_cost - 0.03).abs() < 1e-9);
    assert!((a.cost + a.turn_cost - 0.03).abs() < 1e-9);

    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.03,
        usage: Usage {
            input_tokens: 200,
            output_tokens: 80,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    assert!((a.cost - 0.03).abs() < 1e-9);
    assert_eq!(a.turn_cost, 0.0);
    assert!(!a.turn_has_round_usage);
    assert_eq!(a.total_in, 300);
    assert_eq!(a.total_out, 130);
}

#[test]
fn turn_end_folds_bundled_totals_on_resume_path() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        prompt: "p".into(),
        kind: lofi_types::PromptKind::User,
    });
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.05,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
        stop_reason: None,
    });
    assert!((a.cost - 0.05).abs() < 1e-9);
    assert_eq!(a.total_in, 10);
    assert_eq!(a.total_out, 20);
    assert_eq!(a.turn_cost, 0.0);
    assert!(!a.turn_has_round_usage);
}
