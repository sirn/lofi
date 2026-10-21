#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

#[tokio::test]
async fn user_prompt_is_persisted_before_first_provider_round() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let agent = Agent {
        provider: Arc::new(PendingAfterRoundProvider {
            first: std::sync::Mutex::new(None),
        }),
        ..agent_with(Vec::new(), dir.path())
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    let cursor = crate::session::store::SessionCursor::new(path.clone(), None);
    let run = agent.run_turn(
        &mut messages,
        TurnRequest::prompt("keep this".into(), lofi_types::PromptKind::User),
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
        "first provider round should still be pending"
    );

    let events = cursor.load_tree_events().unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::Message(message)
                if message.role == Role::User
                    && message.blocks.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text } if text == "keep this"
                    ))
        )),
        "settled user prompt must be on disk before the first stream"
    );
    assert!(!events.iter().any(|event| matches!(
        event.kind,
        SessionEventKind::TurnEnd { .. }
            | SessionEventKind::TurnFailed { .. }
            | SessionEventKind::TurnCancelled { .. }
    )));
}

#[tokio::test]
async fn failed_admission_checkpoint_fails_the_turn_before_the_request() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    // A transcript that cannot take the prompt fails admission. The provider
    // never yields, so a turn that reached it would not return at all.
    let agent = Agent {
        provider: Arc::new(NeverProvider),
        ..agent_with(Vec::new(), dir.path())
    };
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let cursor = crate::session::store::SessionCursor::new(path, None);
    let mut messages = Vec::new();
    let run = agent.run_turn(
        &mut messages,
        TurnRequest::prompt("keep this".into(), lofi_types::PromptKind::User),
        tx,
        Some(&cursor),
        RunControls {
            cancel: None,
            preempt: None,
        },
    );
    let out = tokio::time::timeout(std::time::Duration::from_secs(5), run).await;
    assert!(
        matches!(out, Ok(Err(_))),
        "a prompt the transcript cannot hold must fail the turn"
    );
    assert!(messages.is_empty(), "the prompt never entered the history");
}

#[tokio::test]
async fn startup_notice_uses_the_durable_prompt_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let agent = agent_with(
        vec![vec![StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        }]],
        dir.path(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    let cursor = crate::session::store::SessionCursor::new(path, None);
    agent
        .run_turn(
            &mut messages,
            TurnRequest {
                input: TurnInput::Prompt {
                    text: "continue".into(),
                    kind: lofi_types::PromptKind::User,
                    attachments: Vec::new(),
                },
                notices: vec!["job 7 is stale".into()],
            },
            tx,
            Some(&cursor),
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
            .filter(|message| message.role == Role::User)
            .map(|message| message.kind)
            .collect::<Vec<_>>(),
        [lofi_types::PromptKind::Notice, lofi_types::PromptKind::User]
    );
    let mut prompts = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::Prompt { prompt, kind } = event {
            prompts.push((prompt, kind));
        }
    }
    assert_eq!(
        prompts,
        [
            ("job 7 is stale".into(), lofi_types::PromptKind::Notice),
            ("continue".into(), lofi_types::PromptKind::User),
        ]
    );
    let events = cursor.load_tree_events().unwrap();
    let persisted = events
        .iter()
        .filter_map(|event| match &event.kind {
            SessionEventKind::Message(message) if message.role == Role::User => Some(message.kind),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        persisted,
        [lofi_types::PromptKind::Notice, lofi_types::PromptKind::User]
    );
}

#[tokio::test]
async fn settled_messages_remain_on_disk_after_consumer_drops() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("kept".into()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: None,
            },
        ]],
        dir.path(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
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
    tokio::pin!(run);
    let mut run_done = false;
    let mut saw_text = false;
    while !saw_text {
        tokio::select! {
            biased;
            event = rx.recv() => match event {
                Some(AgentEvent::Text(_)) => saw_text = true,
                None if run_done => panic!("run sent no Text"),
                _ => {}
            },
            result = run.as_mut(), if !run_done => {
                result.unwrap();
                run_done = true;
            }
        }
    }
    drop(rx);
    if !run_done {
        run.await.unwrap();
    }

    let events = cursor.load_tree_events().unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::Message(message)
                if message.role == Role::User
                    && message.blocks.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text } if text == "go"
                    ))
        )),
        "user prompt must survive a dropped consumer"
    );
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::Message(message)
                if message.role == Role::Assistant
                    && message.blocks.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text } if text == "kept"
                    ))
        )),
        "completed assistant message must survive a dropped consumer"
    );
}

#[tokio::test]
async fn completed_final_round_is_on_disk_before_turn_end() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("final answer".into()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: None,
            },
        ]],
        dir.path(),
    );
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
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
    tokio::pin!(run);

    let mut saw_text = false;
    let mut run_done = false;
    loop {
        let event = tokio::select! {
            biased;
            event = rx.recv() => event.unwrap(),
            result = run.as_mut(), if !run_done => {
                result.unwrap();
                run_done = true;
                continue;
            }
        };
        match event {
            AgentEvent::Text(_) => saw_text = true,
            AgentEvent::TurnEnd { .. } => {
                panic!("finished assistant must be durable before TurnEnd");
            }
            AgentEvent::RoundCommitted { .. } if saw_text => break,
            _ => {}
        }
    }

    let events = cursor.load_tree_events().unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::Message(message)
                if message.role == Role::Assistant
                    && message.blocks.iter().any(|block| matches!(
                        block,
                        ContentBlock::Text { text } if text == "final answer"
                    ))
        )),
        "completed final assistant message must be on disk before TurnEnd"
    );
}

#[tokio::test]
async fn repeated_tool_results_notify_after_pairing_and_recover_once() {
    let dir = tempdir().unwrap();
    let tool_round = |id: usize| {
        vec![
            StreamingEvent::ToolUseStart {
                id: format!("t{id}"),
                name: "exec".into(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: format!("t{id}"),
                delta: serde_json::json!({ "code": "return 1" }).to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: format!("t{id}"),
            },
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: Some(lofi_types::StopReason::ToolUse),
            },
        ]
    };
    let mut rounds: Vec<_> = (0..5).map(tool_round).collect();
    rounds.push(vec![
        StreamingEvent::TextDelta("changed approach".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ]);
    let agent = agent_with(rounds, dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(128);
    let mut messages = Vec::new();
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

    let Some(notice_index) = messages
        .iter()
        .position(|message| message.kind == lofi_types::PromptKind::Notice)
    else {
        panic!("missing loop notice");
    };
    assert_eq!(messages[notice_index - 1].role, Role::Tool);
    assert_eq!(messages[notice_index + 1].role, Role::Assistant);
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .count(),
        5
    );
}
