use super::*;

// into "previous summary". Returns the leaf id of the pre-compaction range.
fn seed_compacted_turn(path: &std::path::Path) {
    let mut old: Vec<SessionEvent> = (0..5)
        .map(|i| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&format!("old {i}"))),
        })
        .collect();
    test_append_events(path, &mut old, None).unwrap();
    test_append_compaction(
        path,
        &[],
        None,
        "previous summary",
        &[old[0].id.clone(), old[4].id.clone()],
        store::CompactionCounts {
            summarized: old.len(),
            represented: old.len(),
            kept: 0,
        },
    )
    .unwrap();
}

// Append the silent continuation that triggered the regression: two large
// assistant messages followed by a TurnEnd reporting 113k input tokens.
fn seed_post_compact_continuation(path: &std::path::Path) {
    let large = "continued work ".repeat(2_000);
    let mut continuation = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&large)),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&large)),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage {
                    input_tokens: 113_000,
                    ..Usage::default()
                },
                stop_reason: None,
            },
        },
    ];
    test_append_events(path, &mut continuation, None).unwrap();
}

#[test]
fn resumed_compaction_restores_summarized_message_count() {
    // Exact regression: a hard compact kept no tail, its silent continuation
    // produced only two messages, and the session was then resumed with -c.
    // The marker's summarized count must survive resume so /compact does not
    // treat the restored summary as a single message.
    let dir = tempfile::tempdir().unwrap();
    let session_store = store::SessionStore::new(dir.path().join("sessions"));
    let path = session_store
        .create_cursor(std::path::Path::new("/tmp/resumed-compact"), &"p/m".into())
        .unwrap()
        .path()
        .to_path_buf();
    seed_compacted_turn(&path);
    seed_post_compact_continuation(&path);

    let resumed = store::SessionCursor::open(path.clone()).unwrap();
    let index = resumed.snapshot().unwrap().index;
    let mut config = lofi_types::CompactionConfig::default();
    config.auto.max_context_tokens = Some(100_000);
    let mut a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        ServiceTier::Auto,
        0,
        config,
        String::new(),
    );
    a.session.cursor = Some(resumed.clone());
    a.lifecycle.restore_history(&resumed, &index).unwrap();
    restore_compaction_from_index(&mut a, &resumed, &index);

    assert_eq!(a.lifecycle.history_stats().messages, 3);
    assert_eq!(a.status_usage.unwrap().input_tokens, 113_000);
    a.apply_event(AgentEvent::RoundUsage {
        cost: 0.0,
        usage: Usage {
            input_tokens: 113_000,
            ..Usage::default()
        },
    });
    a.maybe_auto_compact();
    assert!(
        a.compacted,
        "resume must evaluate the next settled state and soft compaction must not reuse the hard cooldown"
    );

    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let marker = events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            SessionEventKind::Compaction {
                summarized,
                represented,
                ..
            } => Some((*summarized, *represented)),
            _ => None,
        })
        .unwrap();
    assert_eq!(marker, (2, 7));
}

#[test]
fn resumed_compaction_stays_on_its_cursor_when_a_sibling_appends_later() {
    // Regression: resume snapshots branch A as its logical cursor. Another
    // writer then appends branch B to the same physical JSONL file before the
    // resumed app compacts. Compaction must read and checkpoint A, not infer
    // its thread from physical EOF (which now belongs to B).
    let dir = tempfile::tempdir().unwrap();
    let session_store = store::SessionStore::new(dir.path().join("sessions"));
    let path = session_store
        .create_cursor(
            std::path::Path::new("/tmp/resume-branch-compact"),
            &"p/m".into(),
        )
        .unwrap()
        .path()
        .to_path_buf();

    let mut branch_a: Vec<SessionEvent> = (0..4)
        .flat_map(|i| {
            [
                SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: msg(user(&format!("branch A prompt {i}"))),
                },
                SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: msg(assistant(&format!("branch A reply {i}"))),
                },
            ]
        })
        .collect();
    test_append_events(&path, &mut branch_a, None).unwrap();
    let branch_a_leaf = branch_a.last().unwrap().id.clone();

    let resumed = store::SessionCursor::open(path.clone()).unwrap();
    let resumed_index = resumed.snapshot().unwrap().index;
    let mut a = app();
    a.session.cursor = Some(resumed.clone());
    a.lifecycle
        .restore_history(&resumed, &resumed_index)
        .unwrap();

    let root = branch_a[0].id.clone();
    let mut branch_b = [
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(user("WRONG SIBLING PROMPT")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant("WRONG SIBLING REPLY")),
        },
    ];
    test_append_events(&path, &mut branch_b, Some(&root)).unwrap();
    let sibling_leaf = branch_b[1].id.clone();
    assert_eq!(
        store::SessionCursor::open(path.clone()).unwrap().leaf_id(),
        Some(sibling_leaf.clone())
    );

    assert!(a.compact_now(), "branch A has enough history to compact");
    let marker_leaf = a
        .session
        .cursor
        .as_ref()
        .and_then(store::SessionCursor::leaf_id)
        .expect("compaction advances the resumed cursor");
    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let marker_index = events
        .iter()
        .position(|event| event.id == marker_leaf)
        .expect("cursor points at persisted compaction marker");
    let lineage = store::active_path(&events, &marker_leaf);

    assert!(matches!(
        events[marker_index].kind,
        SessionEventKind::Compaction { .. }
    ));
    assert!(
        lineage.iter().any(|&i| events[i].id == branch_a_leaf),
        "compaction must descend from the branch captured by resume"
    );
    assert!(
        lineage.iter().all(|&i| events[i].id != sibling_leaf),
        "later physical-EOF sibling must not leak into resumed compaction"
    );
    let SessionEventKind::Compaction { summary, .. } = &events[marker_index].kind else {
        unreachable!()
    };
    assert!(summary.contains("branch A prompt"));
    assert!(!summary.contains("WRONG SIBLING"));
}

#[test]
fn resume_does_not_restore_usage_measured_before_latest_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let session_store = store::SessionStore::new(dir.path().join("sessions"));
    let path = session_store
        .create_cursor(
            std::path::Path::new("/tmp/stale-compact-usage"),
            &"p/m".into(),
        )
        .unwrap()
        .path()
        .to_path_buf();

    let mut old = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(user("old prompt")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant("old response")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage {
                    input_tokens: 149_000,
                    ..Usage::default()
                },
                stop_reason: None,
            },
        },
    ];
    test_append_events(&path, &mut old, None).unwrap();
    test_append_compaction(
        &path,
        &[],
        None,
        "summary",
        &[old[0].id.clone(), old[1].id.clone()],
        store::CompactionCounts {
            summarized: 2,
            represented: 2,
            kept: 0,
        },
    )
    .unwrap();
    let mut continuation = vec![SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: msg(assistant("partial continuation")),
    }];
    test_append_events(&path, &mut continuation, None).unwrap();

    let resumed = store::SessionCursor::open(path.clone()).unwrap();
    let index = resumed.snapshot().unwrap().index;
    let mut a = app();
    restore_compaction_from_index(&mut a, &resumed, &index);

    assert_eq!(a.status_usage, None);
    assert!(!a.settled_usage_fresh);
    assert!(a.compacted);
}

#[test]
fn working_status_keeps_the_run_model_across_a_mid_run_switch() {
    let mut a = app(); // model_label = "openai/gpt-4o"
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    a.run_model_label = Some(a.session_model());

    a.model_label = "anthropic/claude".into();
    assert_eq!(a.run_label(), "openai/gpt-4o:medium");
    assert_eq!(a.session_model(), "anthropic/claude:medium");

    a.run_finished();
    assert_eq!(a.run_label(), "anthropic/claude:medium");
}

#[test]
fn working_status_is_replaced_in_place_by_done_status() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen_rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        let area = buf.area;
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "show the status transition".to_string(),
    });
    a.apply_event(AgentEvent::Text(
        (0..40)
            .map(|i| format!("finished response line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    a.apply_event(AgentEvent::TurnEnd {
        model: "openai/gpt-4o".into(),
        elapsed_ms: 1_500,
        cost: 0.0,
        usage: Usage::default(),
        stop_reason: None,
    });

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let active = screen_rows(&term);
    let working_y = active
        .iter()
        .position(|row| row.contains("Working for"))
        .expect("working status row");
    assert!(
        active.iter().all(|row| !row.contains("Done in")),
        "terminal status must stay buffered until settlement: {active:#?}"
    );

    a.run_finished();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let settled = screen_rows(&term);
    let done_y = settled
        .iter()
        .position(|row| row.contains("Done in 1.5s with openai/gpt-4o"))
        .expect("done status row");
    assert_eq!(
        done_y, working_y,
        "Done should replace Working on the same physical row"
    );
}

#[test]
fn auto_evaluation_dialog_renders_elapsed_state_and_ask_reason() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let area = buf.area;
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    }

    let mut a = app();
    let (mut req, _response) = confirm_request("rm generated.txt");
    req.reason = std::sync::Arc::new(std::sync::Mutex::new(
        lofi_core::ConfirmReason::AutoEvaluating {
            started_at: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(4))
                .unwrap(),
        },
    ));
    let reason = req.reason.clone();
    a.pending_confirms.push(req);
    let mut term = Terminal::new(TestBackend::new(90, 28)).unwrap();

    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let evaluating = screen(&term);
    assert!(
        evaluating.contains("Auto evaluation for 4s..."),
        "{evaluating}"
    );
    assert!(!evaluating.contains("allow or deny"), "{evaluating}");

    *reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = lofi_core::ConfirmReason::AutoAsk {
        reason: "command deletes a file".to_string(),
    };
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let asking = screen(&term);
    assert!(
        asking.contains("Auto evaluation asks: command deletes a file"),
        "{asking}"
    );
}

#[test]
fn collapsed_cache_round_trip_preserves_native_preview_rendering() {
    use crate::tui::view::blocks::{compact_native_previews, render_turn_lines};
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: "run it".to_string(),
        blocks: vec![Block::Tool(ToolCall {
            detail_id: 0,
            id: "exec-1".to_string(),
            name: "exec".to_string(),
            input: r#"await lofi.bash({ cmd: "printf test" })"#.to_string(),
            label: None,
            native: vec![NativeTool {
                id: 1,
                name: "bash".to_string(),
                args: "printf test".to_string(),
                result: Some(
                    serde_json::json!({
                        "output": "one\ntwo\nthree\nfour",
                        "duration_ms": 10,
                    })
                    .to_string(),
                ),
                preview: None,
                is_error: false,
                done: true,
            }],
            result: Some("{\"value\":null}".to_string()),
            result_availability: ResultAvailability::Available,
            result_committed: true,
            is_error: false,
            done: true,
            elapsed: Some(Duration::from_millis(10)),
        })],
    });
    let render = |app: &App, turn: &Turn| {
        let cx = Cx {
            app,
            theme: app.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, turn)
            .iter()
            .map(|line| {
                line.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let expected = render(&a, &a.turns[0]);
    let mut compact = a.turns[0].clone();
    compact_native_previews(&mut compact);
    let mut cache = CollapsedTurnCache::new();
    cache.insert(0, &compact);
    let restored = cache.get(0).unwrap();
    assert_eq!(render(&a, &restored), expected);
    assert!(restored.blocks.iter().all(|block| match block {
        Block::Tool(tool) => tool.native.iter().all(|native| native.result.is_none()),
        _ => true,
    }));
}
