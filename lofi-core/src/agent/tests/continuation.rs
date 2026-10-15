#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

#[tokio::test]
async fn max_tokens_stop_continues_the_turn_once() {
    let dir = tempdir().unwrap();
    let truncated = vec![
        StreamingEvent::TextDelta("partial".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::MaxTokens),
        },
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("rest of the answer".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![truncated, final_round], dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let notices: Vec<&Message> = messages
        .iter()
        .filter(|m| m.role == Role::User && m.kind == lofi_types::PromptKind::Notice)
        .collect();
    assert_eq!(notices.len(), 1, "exactly one continuation notice");
    match &notices[0].blocks[0] {
        ContentBlock::Text { text } => {
            assert!(text.contains("token limit"), "nudge text: {text}");
        }
        other => panic!("unexpected block {other:?}"),
    }
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count(),
        2
    );
    let mut saw_notice_prompt = false;
    while let Ok(ev) = rx.try_recv() {
        if matches!(&ev, AgentEvent::Prompt { prompt, kind }
            if *kind == lofi_types::PromptKind::Notice && prompt.contains("token limit"))
        {
            saw_notice_prompt = true;
        }
    }
    assert!(saw_notice_prompt, "live notice prompt emitted");
}

#[tokio::test]
async fn max_tokens_continuation_budget_is_one_per_turn() {
    let dir = tempdir().unwrap();
    let truncated = || {
        vec![
            StreamingEvent::TextDelta("partial".into()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: Some(lofi_types::StopReason::MaxTokens),
            },
        ]
    };
    let agent = agent_with(vec![truncated(), truncated()], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    // Two truncated rounds, but only one continuation notice: the budget
    // caps the loop so a model stuck at the cap still ends the turn.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.kind == lofi_types::PromptKind::Notice)
            .count(),
        1
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count(),
        2
    );
}

#[tokio::test]
async fn loop_recovery_remains_available_after_terminal_recovery() {
    let dir = tempdir().unwrap();
    let pattern = "abcdefghij".repeat(10);
    let truncated = vec![
        StreamingEvent::TextDelta("partial".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::MaxTokens),
        },
    ];
    let looping = vec![StreamingEvent::ThinkingDelta(pattern.repeat(3))];
    let final_round = vec![
        StreamingEvent::TextDelta("final answer".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![truncated, looping, final_round], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];

    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let notices = messages
        .iter()
        .filter(|message| message.kind == PromptKind::Notice)
        .filter_map(|message| match &message.blocks[0] {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 2);
    assert!(notices.iter().any(|text| text.contains("token limit")));
    assert!(notices
        .iter()
        .any(|text| text.contains("different concrete action")));
    assert!(messages.iter().any(|message| {
        matches!(&message.blocks[0], ContentBlock::Text { text } if text == "final answer")
    }));
}

#[tokio::test]
async fn clean_end_turn_does_not_continue() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("final answer".into()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: Some(lofi_types::StopReason::EndTurn),
            },
        ]],
        dir.path(),
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    assert!(
        messages
            .iter()
            .all(|m| m.kind != lofi_types::PromptKind::Notice),
        "clean end_turn must not trigger a continuation"
    );
    assert_eq!(messages.len(), 3);
}

#[tokio::test]
async fn tool_stop_without_a_tool_call_continues_by_default() {
    let dir = tempdir().unwrap();
    let lost = vec![
        StreamingEvent::TextDelta("I will inspect it now.".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        },
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("done".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![lost, final_round], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let notices: Vec<_> = messages
        .iter()
        .filter(|message| message.kind == lofi_types::PromptKind::Notice)
        .collect();
    assert_eq!(notices.len(), 1);
    let ContentBlock::Text { text } = &notices[0].blocks[0] else {
        panic!("expected notice text");
    };
    assert!(text.contains("tool call was not received"), "{text}");
}

#[tokio::test]
async fn tool_stop_recovery_can_be_disabled() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        }]],
        dir.path(),
    )
    .with_auto_continue(lofi_types::AutoContinuePolicy {
        lost_tool_call: false,
        intent: false,
    });
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();
    assert!(messages
        .iter()
        .all(|message| message.kind != lofi_types::PromptKind::Notice));
}

#[tokio::test]
async fn clean_stop_intent_continues_only_when_enabled() {
    let dir = tempdir().unwrap();
    let intent = vec![
        StreamingEvent::TextDelta("I found the likely cause.\n\nNext, I'll run the tests.".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("done".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![intent, final_round], dir.path()).with_auto_continue(
        lofi_types::AutoContinuePolicy {
            lost_tool_call: true,
            intent: true,
        },
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        messages
            .iter()
            .filter(|message| message.kind == lofi_types::PromptKind::Notice)
            .count(),
        1
    );
}

#[test]
fn intent_recovery_is_disabled_by_default() {
    assert!(!lofi_types::AutoContinuePolicy::default().intent);
    let message = Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: "I'll run the tests next.".into(),
        }],
        kind: lofi_types::PromptKind::User,
    };
    assert!(assistant_announces_tool_intent(&message));
}

#[test]
fn intent_detector_is_bounded_to_immediate_tool_actions() {
    let message = |text: &str| Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text { text: text.into() }],
        kind: lofi_types::PromptKind::User,
    };
    for positive in [
        "Let me inspect the repository.",
        "I’ll run the tests next.",
        "I am going to check the logs.",
        "I found it.\n\nNext, I'll load the ticket.",
        "I'll verify that now.",
    ] {
        assert!(
            assistant_announces_tool_intent(&message(positive)),
            "{positive}"
        );
    }
    for negative in [
        "Let me know if you need anything else.",
        "You can run the tests.",
        "I can inspect it if you want.",
        "I will not change that.",
        "The next step is to run the tests.",
        "I inspected the repository and fixed the issue.",
    ] {
        assert!(
            !assistant_announces_tool_intent(&message(negative)),
            "{negative}"
        );
    }
}

#[tokio::test]
async fn truncation_and_lost_tool_recovery_share_one_budget() {
    let dir = tempdir().unwrap();
    let truncated = vec![StreamingEvent::Done {
        usage: Usage::default(),
        stop_reason: Some(lofi_types::StopReason::MaxTokens),
    }];
    let lost_tool = vec![StreamingEvent::Done {
        usage: Usage::default(),
        stop_reason: Some(lofi_types::StopReason::ToolUse),
    }];
    let agent = agent_with(vec![truncated, lost_tool], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.kind == lofi_types::PromptKind::Notice)
            .count(),
        1
    );
}

#[tokio::test]
async fn max_tokens_truncated_tool_call_is_closed_and_valid_calls_still_run() {
    let dir = tempdir().unwrap();
    let cut_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t_cut".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t_cut".into(),
            delta: r#"{"code":"#.into(),
        },
        StreamingEvent::ToolUseStart {
            id: "t_ok".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t_ok".into(),
            delta: r#"{"code":"return 2"}"#.into(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::MaxTokens),
        },
    ];
    let recovered = vec![
        StreamingEvent::TextDelta("recovered".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![cut_round, recovered], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    // The cut call was never executed: its result is a synthetic error
    // naming the truncation. The valid call in the same round ran.
    let tool_message = messages.iter().find(|m| m.role == Role::Tool).unwrap();
    assert_eq!(tool_message.blocks.len(), 2);
    let ContentBlock::ToolResult {
        tool_use_id: cut_id,
        content: cut_content,
        is_error: cut_error,
        ..
    } = &tool_message.blocks[0]
    else {
        panic!("expected cut tool result");
    };
    assert_eq!(cut_id, "t_cut");
    assert_eq!(cut_error, &true);
    assert!(cut_content.contains("token limit"), "{cut_content}");
    let ContentBlock::ToolResult {
        tool_use_id: ok_id,
        is_error: ok_error,
        ..
    } = &tool_message.blocks[1]
    else {
        panic!("expected valid tool result");
    };
    assert_eq!(ok_id, "t_ok");
    assert_eq!(ok_error, &false);
    // Results mirror the assistant message's declared order and the turn
    // recovered in the next round.
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count(),
        2
    );
}

#[tokio::test]
async fn truncation_notice_is_persisted_and_replays_as_notice_turn() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let truncated = vec![
        StreamingEvent::TextDelta("partial".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::MaxTokens),
        },
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("rest".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![truncated, final_round], dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    let cursor = crate::session::store::SessionCursor::new(path.clone(), None);
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            Some(&cursor),
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let events = cursor.load_tree_events().unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::Message(message)
            if message.kind == lofi_types::PromptKind::Notice
    )));
    // Replay starts a notice-kind turn for it, never a user turn.
    let mut kinds = Vec::new();
    crate::session::replay::replay_selected_session_events(&events, |ev| {
        if let AgentEvent::Prompt { kind, .. } = ev {
            kinds.push(kind);
        }
    });
    assert_eq!(
        kinds,
        vec![lofi_types::PromptKind::User, lofi_types::PromptKind::Notice]
    );

    // The turn ended on the final round's clean stop. That reason is
    // emitted live, persisted in the transcript marker, and re-emitted by
    // replay — the evidence base for stop-reason-driven decisions.
    let mut live_stop = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::TurnEnd { stop_reason, .. } = ev {
            live_stop.push(stop_reason);
        }
    }
    assert_eq!(live_stop, vec![Some(lofi_types::StopReason::EndTurn)]);
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::TurnEnd {
            stop_reason: Some(lofi_types::StopReason::EndTurn),
            ..
        }
    )));
    let mut replayed_stop = Vec::new();
    crate::session::replay::replay_selected_session_events(&events, |ev| {
        if let AgentEvent::TurnEnd { stop_reason, .. } = ev {
            replayed_stop.push(stop_reason);
        }
    });
    assert_eq!(replayed_stop, vec![Some(lofi_types::StopReason::EndTurn)]);
}

#[tokio::test]
async fn cancel_preempts_the_truncation_continuation() {
    let dir = tempdir().unwrap();
    let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
    let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
    let first_round = vec![
        StreamingEvent::TextDelta("partial".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::MaxTokens),
        },
    ];
    let agent = Agent {
        provider: Arc::new(PendingAfterRoundProvider {
            first: std::sync::Mutex::new(Some(first_round)),
        }),
        ..agent_with(Vec::new(), dir.path())
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    {
        let run = agent.run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            Some(&cursor),
            RunControls {
                cancel: Some(cancel.clone()),
                preempt: None,
            },
        );
        tokio::pin!(run);

        // Interrupt once the truncation prompt went out, while the
        // continuation round is still streaming.
        loop {
            let event = tokio::select! {
                result = run.as_mut() => panic!("run settled before cancellation: {result:?}"),
                event = rx.recv() => event.unwrap_or_else(|| panic!("run event channel closed")),
            };
            if matches!(event, AgentEvent::Prompt { ref prompt, kind }
                if kind == PromptKind::Notice && prompt.contains("token limit"))
            {
                break;
            }
        }
        cancel.store(true, Ordering::Relaxed);
        run.as_mut().await.unwrap();
    }

    // The interrupt wins over the continuation: the turn records as
    // cancelled. The nudge still lands in the transcript, matching the
    // existing policy that cancelled turns stay durable and in context.
    let events = cursor.load_events().unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(&event.kind, SessionEventKind::TurnCancelled { .. })));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::Message(message) if message.kind == lofi_types::PromptKind::Notice
    )));
}

#[tokio::test]
async fn run_continuation_persists_completed_round_before_next_round_settles() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let tool_input = serde_json::json!({ "code": "return 1" }).to_string();
    let first_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: tool_input,
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = Agent {
        provider: Arc::new(PendingAfterRoundProvider {
            first: std::sync::Mutex::new(Some(first_round)),
        }),
        ..agent_with(Vec::new(), dir.path())
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    let cursor = crate::session::store::SessionCursor::new(path.clone(), None);
    let run = agent.run_turn(
        &mut messages,
        TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
        tx,
        Some(&cursor),
        RunControls {
            cancel: None,
            preempt: None,
        },
    );
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), run)
            .await
            .is_err(),
        "second provider round should still be pending"
    );

    let events = cursor.load_tree_events().unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::Message(message) if message.role == Role::Tool
    )));
}

#[tokio::test]
async fn cancelled_run_persists_partial_output_as_aborted_turn() {
    let dir = tempdir().unwrap();
    let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
    let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
    let tool_input = serde_json::json!({ "code": "return 1" }).to_string();
    let first_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: tool_input,
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = Agent {
        provider: Arc::new(CompletedRoundThenPartialProvider {
            first: std::sync::Mutex::new(Some(first_round)),
        }),
        ..agent_with(Vec::new(), dir.path())
    };
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    {
        let run = agent.run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), lofi_types::PromptKind::User),
            tx,
            Some(&cursor),
            RunControls {
                cancel: Some(cancel.clone()),
                preempt: None,
            },
        );
        tokio::pin!(run);

        loop {
            let event = tokio::select! {
                result = run.as_mut() => panic!("run settled before cancellation: {result:?}"),
                event = rx.recv() => event.unwrap_or_else(|| panic!("run event channel closed")),
            };
            if matches!(event, AgentEvent::Text(ref text) if text == "partial answer") {
                break;
            }
        }
        cancel.store(true, Ordering::Relaxed);
        run.as_mut().await.unwrap();
    }

    let mut saw_cancelled = false;
    let mut committed = None;
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::TurnCancelled { .. } => {
                saw_cancelled = true;
            }
            AgentEvent::TurnCommitted {
                byte_start,
                byte_end,
            } => committed = Some((byte_start, byte_end)),
            _ => {}
        }
    }
    assert!(saw_cancelled);
    assert!(committed.is_some());

    let events = cursor.load_events().unwrap();
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::Message(message) if message.role == Role::Tool
    )));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        SessionEventKind::Message(message)
            if message.role == Role::Assistant
                && message.blocks.iter().any(|block| matches!(
                    block,
                    ContentBlock::Text { text } if text == "partial answer"
                ))
    )));
    assert!(events
        .iter()
        .any(|event| matches!(&event.kind, SessionEventKind::TurnCancelled { .. })));
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[2].role, Role::Tool);
    assert!(matches!(
        &messages[3].blocks[..],
        [ContentBlock::Text { text }] if text == "partial answer"
    ));
}

#[tokio::test]
async fn run_continuation_force_stops_at_hard_cap() {
    let dir = tempdir().unwrap();
    let tool_input = serde_json::json!({ "code": "return 1" }).to_string();
    let tool_round = |input_tokens| {
        vec![
            StreamingEvent::ToolUseStart {
                id: "t1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "t1".to_string(),
                delta: tool_input.clone(),
            },
            StreamingEvent::ToolUseEnd {
                id: "t1".to_string(),
            },
            StreamingEvent::Done {
                usage: Usage {
                    input_tokens,
                    ..Usage::default()
                },
                stop_reason: None,
            },
        ]
    };
    let agent = Agent {
        exec_worker: std::sync::Arc::new(std::sync::OnceLock::new()),
        provider: Arc::new(MockProvider::new(vec![tool_round(10), tool_round(500)])),
        model: {
            let mut m = model();
            m.context_window = Some(100);
            m
        },
        root: dir.path().to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-agent-test"),
        tmp_lease: None,
        retry: crate::retry::RetryPolicy::default(),
        system_prompt: "sys".to_string(),
        max_output_tokens: None,
        reserved_context_tokens: 20, // hard cap = 100 - 20 = 80
        bash_env: lofi_code::BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm_tx: None,
        confirm_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        auto_mode: None,
        policy_override: lofi_code::policy::PolicyOverride::default(),
        skills_dir: None,
        jobs: lofi_code::tools::JobRegistry::new(),
        truncate: lofi_code::TruncatedCap::default(),
        image: lofi_types::ImageConfig::default(),
        auto_continue: lofi_types::AutoContinuePolicy::default(),
        subagent_support: None,
        subagents: SubagentRegistry::default(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt(String::new(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let mut saw_pressure = false;
    let mut saw_turn_end = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::ContextPressure { .. } => saw_pressure = true,
            AgentEvent::TurnEnd { .. } => saw_turn_end = true,
            _ => {}
        }
    }
    assert!(saw_pressure, "expected a ContextPressure event");
    assert!(!saw_turn_end, "hard-cap stop must not emit TurnEnd");
    assert_eq!(messages.last().unwrap().role, Role::Tool);
}

#[tokio::test]
async fn run_continuation_image_byte_pressure_stops_before_send() {
    let dir = tempdir().unwrap();
    // A vision model so the image is NOT stripped by the non-vision guard and
    // its bytes actually count toward the request payload budget.
    let provider = Arc::new(MockProvider::new(vec![vec![
        StreamingEvent::TextDelta("unreachable".to_string()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ]]));
    let agent = Agent {
        exec_worker: std::sync::Arc::new(std::sync::OnceLock::new()),
        provider: provider.clone(),
        model: {
            let mut m = model();
            m.supports_image = true;
            m
        },
        root: dir.path().to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-agent-test"),
        tmp_lease: None,
        retry: crate::retry::RetryPolicy::default(),
        system_prompt: "sys".to_string(),
        max_output_tokens: None,
        reserved_context_tokens: 0, // token cap disabled; byte cap is the trigger
        bash_env: lofi_code::BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm_tx: None,
        confirm_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        auto_mode: None,
        policy_override: lofi_code::policy::PolicyOverride::default(),
        skills_dir: None,
        jobs: lofi_code::tools::JobRegistry::new(),
        truncate: lofi_code::TruncatedCap::default(),
        image: lofi_types::ImageConfig::default(),
        auto_continue: lofi_types::AutoContinuePolicy::default(),
        subagent_support: None,
        subagents: SubagentRegistry::default(),
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    // 7 MiB raw -> ~9.3 MiB base64, over the 8 MiB request image budget.
    let mut messages = vec![Message {
        origin: None,
        role: Role::User,
        blocks: vec![
            ContentBlock::Text {
                text: "look".to_string(),
            },
            ContentBlock::Image {
                bytes: vec![0u8; 7 * 1024 * 1024],
                media_type: "image/jpeg".to_string(),
            },
        ],
        kind: PromptKind::default(),
    }];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt(String::new(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let mut saw_pressure = false;
    let mut saw_failed = false;
    let mut saw_notice = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::ContextPressure { .. } => saw_pressure = true,
            AgentEvent::TurnFailed { .. } => saw_failed = true,
            AgentEvent::Notice(n) if n.contains("compacting to fit") => saw_notice = true,
            _ => {}
        }
    }
    assert!(
        saw_pressure,
        "byte pressure must surface as ContextPressure"
    );
    assert!(!saw_failed, "byte pressure must not fail the turn");
    assert!(saw_notice, "byte pressure must explain the stop");
    // The provider was never called: its single round is still queued.
    assert_eq!(provider.rounds.lock().unwrap().len(), 1);
    // The durable history keeps the original image untouched.
    assert!(messages
        .iter()
        .flat_map(|m| m.blocks.iter())
        .any(|b| matches!(b, ContentBlock::Image { .. })));
}

#[tokio::test]
async fn run_once_tool_error_marks_result_error() {
    let dir = tempdir().unwrap();
    let tool_input =
        serde_json::json!({ "code": "await lofi.read('../escape'); return 1;" }).to_string();
    let round1 = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: tool_input,
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = agent_with(vec![round1], dir.path());
    let mut messages = vec![user_msg("go")];
    let finished = agent.run_once(&mut messages).await.unwrap();
    assert!(!finished);
    assert_eq!(messages[2].role, Role::Tool);
    let ContentBlock::ToolResult { is_error, .. } = &messages[2].blocks[0] else {
        panic!("expected tool_result");
    };
    assert!(*is_error);
}
