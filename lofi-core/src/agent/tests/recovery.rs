#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

#[tokio::test]
async fn forward_progress_resets_the_recovery_round_budget() {
    let dir = tempdir().unwrap();
    let productive_rounds = agent_run::MAX_RECOVERY_ROUNDS + 10;
    let mut rounds: Vec<_> = (0..productive_rounds).map(productive_tool_round).collect();
    rounds.push(final_round());
    let agent = agent_with(rounds, dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(8192);
    let mut messages = Vec::new();
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".into(), PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let recovery_notices = messages
        .iter()
        .filter(|message| {
            message.kind == PromptKind::Notice
                && message.blocks.iter().any(|block| {
                    matches!(
                        block,
                        ContentBlock::Text { text } if text.contains("recovery rounds")
                    )
                })
        })
        .count();
    let final_answer = messages.iter().rev().find(|message| {
        message.role == Role::Assistant
            && message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if text == "final answer"))
    });
    assert!(
        final_answer.is_some(),
        "turn must run past the round limit and reach the scripted final round"
    );
    assert!(
        recovery_notices == 0,
        "long productive turns must not trip the recovery-round limit"
    );
}

#[tokio::test]
async fn run_resets_the_recovery_round_budget_after_progress() {
    let dir = tempdir().unwrap();
    let productive_rounds = agent_run::MAX_RECOVERY_ROUNDS + 10;
    let mut rounds: Vec<_> = (0..productive_rounds).map(productive_tool_round).collect();
    rounds.push(final_round());
    let agent = agent_with(rounds, dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(8192);
    agent.run("go".into(), tx).await.unwrap();

    let mut recovery_notices = 0;
    let mut reached_final = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::Prompt { prompt, kind }
                if kind == PromptKind::Notice && prompt.contains("recovery rounds") =>
            {
                recovery_notices += 1;
            }
            AgentEvent::Text(text) if text == "final answer" => reached_final = true,
            _ => {}
        }
    }
    assert!(
        reached_final,
        "run must run past the round limit and stream the scripted final round"
    );
    assert!(
        recovery_notices == 0,
        "long productive turns must not trip the recovery-round limit"
    );
}

#[tokio::test]
async fn repeated_thinking_notifies_agent_and_allows_one_recovery() {
    let dir = tempdir().unwrap();
    let pattern = "abcdefghij".repeat(10);
    let looping = vec![StreamingEvent::ThinkingDelta(pattern.repeat(3))];
    let final_round = vec![
        StreamingEvent::TextDelta("changed approach".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![looping, final_round], dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
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

    let notices: Vec<_> = messages
        .iter()
        .filter(|message| message.kind == lofi_types::PromptKind::Notice)
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(messages[1].role, Role::User);
    assert_eq!(messages[2].role, Role::Assistant);
    let ContentBlock::Text { text } = &notices[0].blocks[0] else {
        panic!("expected notice text");
    };
    assert!(text.contains("different concrete action"), "{text}");
    let mut saw_loop_prompt = false;
    let mut saw_transient_notice = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::Prompt { prompt, kind }
                if kind == lofi_types::PromptKind::Notice
                    && prompt.contains("A potential loop was detected") =>
            {
                saw_loop_prompt = true;
            }
            AgentEvent::Notice(text) if text.contains("loop") => {
                saw_transient_notice = true;
            }
            _ => {}
        }
    }
    assert!(saw_loop_prompt);
    assert!(!saw_transient_notice);
}

#[tokio::test]
async fn repeated_thinking_stops_after_failed_recovery() {
    let dir = tempdir().unwrap();
    let pattern = "abcdefghij".repeat(10);
    let looping = || vec![StreamingEvent::ThinkingDelta(pattern.repeat(3))];
    let agent = agent_with(vec![looping(), looping(), looping()], dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
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

    let notices = messages
        .iter()
        .filter(|message| message.kind == lofi_types::PromptKind::Notice)
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 2);
    assert!(notices.iter().any(|message|
        matches!(&message.blocks[0], ContentBlock::Text { text } if text.contains("different concrete action"))
    ));
    assert!(notices.iter().any(|message|
        matches!(&message.blocks[0], ContentBlock::Text { text } if text.contains("recovery failed"))
    ));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        0
    );
    let mut saw_stop_prompt = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::Prompt { prompt, kind }
            if kind == lofi_types::PromptKind::Notice && prompt.contains("recovery failed"))
        {
            saw_stop_prompt = true;
        }
    }
    assert!(saw_stop_prompt);
}

#[tokio::test]
async fn thinking_only_stop_continues_without_replaying_the_orphan_round() {
    let dir = tempdir().unwrap();
    let thinking_only = vec![
        StreamingEvent::ThinkingDelta("unfinished reasoning".into()),
        StreamingEvent::ThinkingSignature("reasoning-signature".into()),
        StreamingEvent::TextDelta("   ".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("final answer".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let provider = Arc::new(MockProvider::recording(vec![thinking_only, final_round]));
    let agent = Agent {
        provider: provider.clone(),
        ..agent_with(Vec::new(), dir.path())
    };
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

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].iter().all(|message| {
        !message.blocks.iter().any(
            |block| matches!(block, ContentBlock::Thinking { text, .. } if text == "unfinished reasoning"),
        )
    }));
    assert!(requests[1].iter().any(|message| {
        message.kind == PromptKind::Notice
            && matches!(&message.blocks[0], ContentBlock::Text { text } if text.contains("no final answer"))
    }));
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        1
    );
}

#[tokio::test]
async fn terminal_provider_stop_does_not_retry_thinking_only_response() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(MockProvider::recording(vec![vec![
        StreamingEvent::ThinkingDelta("filtered reasoning".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::Other),
        },
    ]]));
    let agent = Agent {
        provider: provider.clone(),
        ..agent_with(Vec::new(), dir.path())
    };
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

    assert_eq!(provider.requests().len(), 1);
    assert!(messages
        .iter()
        .all(|message| message.role != Role::Assistant));
    assert!(messages.iter().any(|message| {
        matches!(&message.blocks[0], ContentBlock::Text { text } if text.contains("provider ended generation"))
    }));
}

#[tokio::test]
async fn repeated_thinking_only_stop_ends_after_one_continuation() {
    let dir = tempdir().unwrap();
    let thinking_only = || {
        vec![
            StreamingEvent::ThinkingDelta("unfinished reasoning".into()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: Some(lofi_types::StopReason::EndTurn),
            },
        ]
    };
    let agent = agent_with(vec![thinking_only(), thinking_only()], dir.path());
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
            .filter(|message| message.kind == PromptKind::Notice)
            .count(),
        2
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        0
    );
    assert!(messages.iter().any(|message| {
        matches!(&message.blocks[0], ContentBlock::Text { text } if text.contains("continuation also produced reasoning"))
    }));
}

#[test]
fn thinking_only_detection_requires_reasoning_and_no_answer_content() {
    let message = |blocks| Message {
        origin: None,
        role: Role::Assistant,
        blocks,
        kind: PromptKind::User,
    };
    assert!(agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Thinking {
            text: "reasoning".into(),
            signature: None,
            redacted: false,
        }
    ])));
    assert!(agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Thinking {
            text: String::new(),
            signature: Some("encrypted reasoning".into()),
            redacted: false,
        }
    ])));
    assert!(agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Thinking {
            text: "[Reasoning redacted]".into(),
            signature: Some("redacted reasoning".into()),
            redacted: true,
        }
    ])));
    assert!(agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Thinking {
            text: String::new(),
            signature: None,
            redacted: false,
        },
        ContentBlock::PartSignature {
            provider: "google".into(),
            model: "gemini".into(),
            format: lofi_types::PartSignatureFormat::Google,
            signature: "thought signature".into(),
        },
    ])));
    assert!(!agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Text {
            text: String::new(),
        }
    ])));
    assert!(!agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Thinking {
            text: String::new(),
            signature: None,
            redacted: false,
        }
    ])));
    assert!(!agent_run::assistant_is_thinking_only(&message(vec![
        ContentBlock::Thinking {
            text: "reasoning".into(),
            signature: None,
            redacted: false,
        },
        ContentBlock::Text {
            text: "answer".into(),
        },
    ])));
}
