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

/// Reasoning that holds no repeated pattern, so the loop guard leaves it to
/// the byte budget. A constant filler reads as a loop after 100 bytes.
fn noisy_reasoning(len: usize, salt: u64) -> String {
    let mut state = 0x2545_f491_4f6c_dd1d_u64 ^ salt;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            char::from(b'a' + ((state >> 33) % 26) as u8)
        })
        .collect()
}

#[tokio::test]
async fn a_round_at_its_reply_budget_keeps_its_answer_and_asks_once_for_less() {
    let dir = tempdir().unwrap();
    let budget = MAX_ROUND_BYTES;
    // The answer the model had begun, then reasoning that fills the budget:
    // a thinking: high model spends the reply on it before it says anything.
    let capped_round = vec![
        StreamingEvent::TextDelta("partial answer".into()),
        StreamingEvent::ThinkingDelta(noisy_reasoning(budget / 2, 1)),
        StreamingEvent::ThinkingDelta(noisy_reasoning(budget / 2 + 1, 2)),
    ];
    let brief_round = vec![
        StreamingEvent::TextDelta("brief answer".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![capped_round, brief_round], dir.path());
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
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

    let answers: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    assert_eq!(answers.len(), 2, "{messages:?}");
    // The answer so far stays; the reasoning that filled the budget does not:
    // an incomplete trace carries no signature, and a provider rejects a part
    // it did not sign.
    assert_eq!(
        answers[0].blocks,
        vec![ContentBlock::Text {
            text: "partial answer".into()
        }],
        "{:?}",
        answers[0].blocks
    );
    assert_eq!(
        answers[1].blocks,
        vec![ContentBlock::Text {
            text: "brief answer".into()
        }]
    );

    let notices: Vec<&Message> = messages
        .iter()
        .filter(|message| message.kind == PromptKind::Notice)
        .collect();
    assert_eq!(notices.len(), 1, "{messages:?}");
    let ContentBlock::Text { text } = &notices[0].blocks[0] else {
        panic!("expected notice text");
    };
    assert!(text.contains("reply size limit"), "{text}");

    let mut saw_budget_notice = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::Notice(text) = event {
            if text.contains("byte budget") {
                saw_budget_notice = true;
            }
        }
    }
    assert!(saw_budget_notice, "the round is reported, not failed");
}

#[tokio::test]
async fn a_second_round_at_the_reply_budget_stops_the_turn() {
    let dir = tempdir().unwrap();
    let oversized = |salt| {
        vec![StreamingEvent::ThinkingDelta(noisy_reasoning(
            MAX_ROUND_BYTES + 1,
            salt,
        ))]
    };
    let agent = agent_with(vec![oversized(3), oversized(4)], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
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

    // Both rounds filled the budget inside their reasoning, so neither kept a
    // message: an empty assistant message is not a valid request.
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Assistant)
            .count(),
        0,
        "{messages:?}"
    );
    let notices: Vec<String> = messages
        .iter()
        .filter(|message| message.kind == PromptKind::Notice)
        .filter_map(|message| match &message.blocks[0] {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert!(notices[0].contains("reply size limit"), "{notices:?}");
    assert!(
        notices[1].contains("byte budget again"),
        "a second round at the budget ends the turn: {notices:?}"
    );
}

#[tokio::test]
async fn a_disabled_byte_cap_ends_the_turn_at_the_first_cut() {
    let dir = tempdir().unwrap();
    let oversized = vec![StreamingEvent::ThinkingDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        5,
    ))];
    let never_asked = vec![
        StreamingEvent::TextDelta("should not run".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![oversized, never_asked], dir.path()).with_auto_continue(
        lofi_types::AutoContinuePolicy {
            byte_cap: false,
            ..lofi_types::AutoContinuePolicy::default()
        },
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
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

    assert!(messages
        .iter()
        .all(|message| message.role != Role::Assistant));
    let notices: Vec<String> = messages
        .iter()
        .filter(|message| message.kind == PromptKind::Notice)
        .filter_map(|message| match &message.blocks[0] {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("byte budget"), "{notices:?}");
    assert!(
        !notices[0].contains("reply size limit"),
        "a disabled cap asks for nothing: {notices:?}"
    );
}
