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

/// A compact view of the history: a failure that dumps the live reasoning
/// trace is unreadable.
fn shape(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|message| {
            let blocks: Vec<String> = message
                .blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => format!("text({})", text.len()),
                    ContentBlock::Thinking { text, .. } => format!("thinking({})", text.len()),
                    ContentBlock::PartSignature { signature, .. } => {
                        format!("signature({})", signature.len())
                    }
                    _ => "other".to_string(),
                })
                .collect();
            format!("{}[{}]", message.role.as_str(), blocks.join(","))
        })
        .collect::<Vec<_>>()
        .join(" ")
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
        notices[1].contains("agent stopped") && notices[1].contains("byte budget"),
        "a second round at the budget ends the turn: {notices:?}"
    );
}

#[tokio::test]
async fn an_oversized_signature_blob_fails_the_round_and_leaves_no_partial() {
    let dir = tempdir().unwrap();
    // A redacted trace is never published, so the partial must not carry the
    // blob the bound rejected.
    let blob = "x".repeat(MAX_OPAQUE_ROUND_BYTES + 1);
    let round = vec![StreamingEvent::ThinkingRedacted { data: blob }];
    // A provider error is retryable by pattern; one attempt is enough.
    let agent = Agent {
        retry: crate::retry::RetryPolicy {
            max_retries: 0,
            ..crate::retry::RetryPolicy::default()
        },
        ..agent_with(vec![round], dir.path())
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
    let outcome = agent
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
        .await;
    let error = outcome.unwrap_err();
    assert!(
        error.to_string().contains("signatures and redacted traces"),
        "{error}"
    );
    assert!(
        !messages.iter().any(|message| message
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Thinking { redacted: true, .. }))),
        "the rejected blob must not reach the history: {}",
        shape(&messages)
    );
}

#[tokio::test]
async fn a_disabled_byte_cap_drops_the_trace_it_cut_and_ends_the_turn() {
    let dir = tempdir().unwrap();
    // A trace that signs itself after its text holds no signature when the
    // budget cuts it, so it is not replay data. The answer before it keeps
    // the message, so the cut trace is the only block the pop can drop.
    let oversized = vec![
        StreamingEvent::TextDelta("an answer".into()),
        StreamingEvent::ThinkingDelta(noisy_reasoning(MAX_ROUND_BYTES + 1, 5)),
    ];
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

    let assistant: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1, "{}", shape(&messages));
    assert!(
        matches!(
            assistant[0].blocks.as_slice(),
            [ContentBlock::Text { text }] if text == "an answer"
        ),
        "only the cut trace goes: {:?}",
        assistant[0].blocks
    );
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
    assert!(
        !notices[0].contains("no final answer"),
        "a cut trace is not a thinking-only answer: {notices:?}"
    );
}

#[tokio::test]
async fn a_cut_trace_a_provider_signed_before_it_streamed_stays() {
    let dir = tempdir().unwrap();
    // A chat-completions trace declares the field it streams on before its
    // text, so the cut lands inside a part the provider already signed.
    // Replaying that prefix is what the field name asks for, so it stays,
    // and the answer before it keeps the message.
    let oversized = vec![
        StreamingEvent::TextDelta("an answer".into()),
        StreamingEvent::ThinkingSignature("reasoning_content".into()),
        StreamingEvent::ThinkingDelta(noisy_reasoning(MAX_ROUND_BYTES + 1, 7)),
    ];
    let agent = agent_with(vec![oversized], dir.path()).with_auto_continue(
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

    let assistant: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1, "{}", shape(&messages));
    assert!(
        matches!(
            assistant[0].blocks.as_slice(),
            [ContentBlock::Text { text }, ContentBlock::Thinking { signature: Some(field), .. }]
                if text == "an answer" && field == "reasoning_content"
        ),
        "the signed trace stays beside the answer: {}",
        shape(&messages)
    );
}

#[tokio::test]
async fn a_cut_answer_over_the_hard_cap_stops_for_compaction() {
    let dir = tempdir().unwrap();
    // The kept answer alone overflows the cap, so the turn must not ask for a
    // brief answer that carries it again: the UI compacts first.
    let oversized = vec![StreamingEvent::TextDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        11,
    ))];
    let mut agent = agent_with(vec![oversized], dir.path());
    agent.model.context_window = Some(100_000);
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

    let assistant: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1, "{}", shape(&messages));
    let mut saw_pressure = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::ContextPressure { .. }) {
            saw_pressure = true;
        }
    }
    assert!(saw_pressure, "a cut over the cap must compact");
}

#[tokio::test]
async fn a_cut_round_bills_the_prompt_the_next_request_carries() {
    let dir = tempdir().unwrap();
    // The trace this round keeps is dropped, so the history stays tiny: the
    // injected system prompt alone is over the cap. The next request carries
    // it, so the turn must compact instead of asking again.
    let cut = vec![StreamingEvent::ThinkingDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        23,
    ))];
    let mut agent = agent_with(vec![cut], dir.path());
    agent.model.context_window = Some(100_000);
    agent.system_prompt = "x".repeat(400_000);
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

    let assistant = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .count();
    assert_eq!(assistant, 0, "the cut kept no answer: {}", shape(&messages));
    let mut pressure = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ContextPressure { resume, .. } = event {
            pressure = Some(resume);
        }
    }
    assert_eq!(
        pressure,
        Some(true),
        "a prompt over the cap must compact before the next request"
    );
}

#[tokio::test]
async fn a_cut_answer_over_the_hard_cap_stops_with_the_byte_cap_off() {
    let dir = tempdir().unwrap();
    // With the byte cap off no continuation is asked, so the stop the budget
    // returns is the last plan of the turn: it must not skip the cap, or the
    // next prompt carries an answer the window cannot hold.
    let oversized = vec![StreamingEvent::TextDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        21,
    ))];
    let mut agent = agent_with(vec![oversized, final_round()], dir.path()).with_auto_continue(
        lofi_types::AutoContinuePolicy {
            byte_cap: false,
            ..lofi_types::AutoContinuePolicy::default()
        },
    );
    agent.model.context_window = Some(100_000);
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

    assert!(
        !messages
            .iter()
            .any(|message| message.blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if text == "final answer")
            )),
        "no round may carry the answer again: {}",
        shape(&messages)
    );
    let mut saw_pressure = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ContextPressure { resume, .. } = event {
            saw_pressure = Some(resume);
        }
    }
    assert_eq!(
        saw_pressure,
        Some(false),
        "a stopped cut must compact without asking the model again"
    );
}

#[tokio::test]
async fn a_second_cut_over_the_hard_cap_stops_for_compaction() {
    let dir = tempdir().unwrap();
    // The first cut spent the continuation and kept no answer, so the second
    // cut returns the stop plan. The answer that cut kept is over the cap and
    // still must not go out: the next request would carry it.
    let trace_only = vec![StreamingEvent::ThinkingDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        22,
    ))];
    let oversized = vec![StreamingEvent::TextDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        23,
    ))];
    let mut agent = agent_with(vec![trace_only, oversized, final_round()], dir.path());
    agent.model.context_window = Some(100_000);
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

    assert!(
        !messages
            .iter()
            .any(|message| message.blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if text == "final answer")
            )),
        "no round may carry the answer again: {}",
        shape(&messages)
    );
    let mut saw_pressure = None;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ContextPressure { resume, .. } = event {
            saw_pressure = Some(resume);
        }
    }
    assert_eq!(
        saw_pressure,
        Some(false),
        "a stopped cut must compact without asking the model again"
    );
}

#[tokio::test]
async fn a_cut_round_bills_the_tool_result_it_keeps() {
    let dir = tempdir().unwrap();
    // The round's call completes and runs before the budget cuts its trace,
    // so its result joins the next request: the cap must read the fill that
    // result raises, not the fill of the round before it.
    let big = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".into(),
            delta: serde_json::json!({
                "code": "return \"x\".repeat(200_000)",
                "description": "run code"
            })
            .to_string(),
        },
        StreamingEvent::ToolUseEnd { id: "t1".into() },
        StreamingEvent::ThinkingDelta(noisy_reasoning(MAX_ROUND_BYTES + 1, 24)),
    ];
    let mut agent = agent_with(vec![big, final_round()], dir.path());
    // The default reserve leaves a 10k cap: the call's result alone is over.
    agent.model.context_window = Some(30_000);
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

    let results: Vec<&ContentBlock> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.blocks.iter())
        .collect();
    assert_eq!(results.len(), 1, "{}", shape(&messages));
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = results[0]
    else {
        panic!("expected a tool result");
    };
    assert!(!is_error, "{}", shape(&messages));
    // The result is billed in chars over four, so it alone outgrows the cap.
    assert!(content.len() > 40_000, "{} bytes", content.len());
    assert!(
        !messages
            .iter()
            .any(|message| message.blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if text == "final answer")
            )),
        "the result fills the window, so no round may send it: {}",
        shape(&messages)
    );
    let mut saw_pressure = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::ContextPressure { .. }) {
            saw_pressure = true;
        }
    }
    assert!(saw_pressure, "the kept result must reach the cap");
}

#[tokio::test]
async fn a_byte_cut_ends_the_turn_even_when_loop_recovery_would_continue() {
    let dir = tempdir().unwrap();
    // Five identical calls trip the loop guard, and the fifth round is cut by
    // the budget too. The budget stop is final: a loop repair asks the model
    // again, and the second-cut rule never gets to stop that request.
    let same_call = |salt: Option<u64>| {
        let mut round = vec![
            StreamingEvent::ToolUseStart {
                id: "t1".into(),
                name: "exec".into(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "t1".into(),
                delta: serde_json::json!({ "code": "return 1", "description": "run code" })
                    .to_string(),
            },
            StreamingEvent::ToolUseEnd { id: "t1".into() },
        ];
        if let Some(salt) = salt {
            round.push(StreamingEvent::ThinkingDelta(noisy_reasoning(
                MAX_ROUND_BYTES + 1,
                salt,
            )));
        }
        round.push(StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        });
        round
    };
    let mut rounds: Vec<_> = (0..4).map(|_| same_call(None)).collect();
    rounds.push(same_call(Some(25)));
    rounds.push(final_round());
    let agent = agent_with(rounds, dir.path()).with_auto_continue(lofi_types::AutoContinuePolicy {
        byte_cap: false,
        ..lofi_types::AutoContinuePolicy::default()
    });
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

    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| message.blocks.iter())
            .count(),
        5,
        "the cut round ends the turn: {}",
        shape(&messages)
    );
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
}

#[tokio::test]
async fn a_capped_tool_round_cuts_the_call_and_a_second_cut_stops_the_turn() {
    let dir = tempdir().unwrap();
    // The round writes a tool call that the budget cuts in half: running it
    // would send the model arguments it never wrote.
    let cut_round = |salt| {
        vec![
            StreamingEvent::ToolUseStart {
                id: "t1".into(),
                name: "exec".into(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "t1".into(),
                delta: format!(
                    "{{\"code\":\"{}\"",
                    noisy_reasoning(MAX_ROUND_BYTES + 1, salt)
                ),
            },
        ]
    };
    let agent = agent_with(vec![cut_round(7), cut_round(8)], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    // Both calls are answered as cut instead of executed with arguments the
    // model never wrote. The first round runs on; the second cut ends it.
    let tool_results: Vec<&ContentBlock> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.blocks.iter())
        .collect();
    assert_eq!(tool_results.len(), 2, "{messages:?}");
    for result in tool_results {
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = result
        else {
            panic!("expected a tool result");
        };
        assert!(is_error, "{content}");
        assert!(content.contains("cut off"), "{content}");
        assert!(content.contains("byte budget"), "{content}");
    }
    let notices: Vec<String> = messages
        .iter()
        .filter(|message| message.kind == PromptKind::Notice)
        .filter_map(|message| match &message.blocks[0] {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("agent stopped"), "{notices:?}");
}

#[tokio::test]
async fn a_complete_trace_before_the_cut_stays_with_its_signature() {
    let dir = tempdir().unwrap();
    // Google signs a thought with the part signature that follows it. The
    // round was cut later, in its text, so the thought is complete replay data.
    let round = vec![
        StreamingEvent::ThinkingDelta("weigh the options".into()),
        StreamingEvent::PartSignature {
            provider: "p".into(),
            model: "m".into(),
            format: lofi_types::PartSignatureFormat::Google,
            target: None,
            signature: "c2lnbmF0dXJl".into(),
        },
        StreamingEvent::TextDelta(noisy_reasoning(MAX_ROUND_BYTES + 1, 9)),
    ];
    let mut agent = agent_with(vec![round], dir.path());
    agent.model.api = lofi_types::Api::GoogleGenerativeAi;
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    let Some(assistant) = messages
        .iter()
        .find(|message| message.role == Role::Assistant)
    else {
        panic!("the cut round is kept: {messages:?}");
    };
    assert!(
        matches!(
            assistant.blocks.as_slice(),
            [
                ContentBlock::Thinking { text, .. },
                ContentBlock::PartSignature { .. },
                ContentBlock::Text { .. },
            ] if text == "weigh the options"
        ),
        "{:?}",
        assistant.blocks
    );
}

#[tokio::test]
async fn a_disabled_byte_cap_stops_at_a_cut_tool_round() {
    let dir = tempdir().unwrap();
    // A tool round the budget cuts: with the cap off, the turn reports the cut
    // and ends instead of running the next round.
    let cut_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".into(),
            delta: format!(
                "{{\"code\":\"{}\"",
                noisy_reasoning(MAX_ROUND_BYTES + 1, 11)
            ),
        },
    ];
    let agent = agent_with(vec![cut_round, final_round()], dir.path()).with_auto_continue(
        lofi_types::AutoContinuePolicy {
            byte_cap: false,
            ..lofi_types::AutoContinuePolicy::default()
        },
    );
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    // The second round never ran, so only the cut round is answered.
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .flat_map(|message| message.blocks.iter())
            .count(),
        1,
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
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("byte budget"), "{notices:?}");
}

#[tokio::test]
async fn a_terminal_cut_after_a_tool_cut_ends_the_turn() {
    let dir = tempdir().unwrap();
    // The first cut runs the next round; the second cut of the turn stops it,
    // even when the round that hits it is the answer rather than a tool call.
    let cut_tool_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".into(),
            delta: format!(
                "{{\"code\":\"{}\"",
                noisy_reasoning(MAX_ROUND_BYTES + 1, 12)
            ),
        },
    ];
    let cut_answer = vec![StreamingEvent::TextDelta(noisy_reasoning(
        MAX_ROUND_BYTES + 1,
        13,
    ))];
    let agent = agent_with(vec![cut_tool_round, cut_answer, final_round()], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    let notices: Vec<String> = messages
        .iter()
        .filter(|message| message.kind == PromptKind::Notice)
        .filter_map(|message| match &message.blocks[0] {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("agent stopped"), "{notices:?}");
    assert!(
        !messages
            .iter()
            .any(|message| message.blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if text == "final answer")
            )),
        "the third round must not run: {messages:?}"
    );
}

#[tokio::test]
async fn a_cut_round_that_kept_only_reasoning_adds_no_message() {
    let dir = tempdir().unwrap();
    // The budget fills after a complete signed trace, so the cut leaves only
    // reasoning: an orphan trace must not enter the next request.
    let round = vec![
        StreamingEvent::ThinkingDelta("weigh the options".into()),
        StreamingEvent::PartSignature {
            provider: "p".into(),
            model: "m".into(),
            format: lofi_types::PartSignatureFormat::Google,
            target: None,
            signature: "signed".into(),
        },
        StreamingEvent::ThinkingDelta(noisy_reasoning(MAX_ROUND_BYTES + 1, 14)),
    ];
    let agent = agent_with(vec![round, final_round()], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    let assistant: Vec<&Message> = messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .collect();
    assert_eq!(assistant.len(), 1, "{messages:?}");
    assert!(
        assistant[0]
            .blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text == "final answer")),
        "only the answer round is kept: {messages:?}"
    );
}

#[tokio::test]
async fn a_capped_round_cuts_every_open_tool_start() {
    let dir = tempdir().unwrap();
    // Two calls start and the budget cuts the round inside the second: neither
    // may run, because one never held an argument and the other is still being
    // written.
    let cut_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t0".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseStart {
            id: "t1".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".into(),
            delta: format!(
                "{{\"code\":\"{}\"",
                noisy_reasoning(MAX_ROUND_BYTES + 1, 15)
            ),
        },
    ];
    let agent = agent_with(vec![cut_round, final_round()], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    let tool_results: Vec<&ContentBlock> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.blocks.iter())
        .collect();
    assert_eq!(tool_results.len(), 2, "{messages:?}");
    for result in tool_results {
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = result
        else {
            panic!("expected a tool result");
        };
        assert!(is_error, "{content}");
        assert!(content.contains("cut off"), "{content}");
        assert!(content.contains("byte budget"), "{content}");
    }
}

#[tokio::test]
async fn a_capped_round_on_a_marking_protocol_cuts_a_call_without_its_end() {
    let dir = tempdir().unwrap();
    // The protocol ends each call with its own marker, and the cut lands
    // before the first one. A call whose arguments happen to parse is still
    // unfinished: the provider may replace them when it completes the call.
    let cut_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t0".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t0".into(),
            delta: r#"{"code":"1"}"#.into(),
        },
        StreamingEvent::ToolUseStart {
            id: "t1".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".into(),
            delta: format!(
                "{{\"code\":\"{}\"",
                noisy_reasoning(MAX_ROUND_BYTES + 1, 17)
            ),
        },
    ];
    let mut agent = agent_with(vec![cut_round, final_round()], dir.path());
    agent.model.api = lofi_types::Api::OpenAiResponses;
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    let tool_results: Vec<&ContentBlock> = messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .flat_map(|message| message.blocks.iter())
        .collect();
    assert_eq!(tool_results.len(), 2, "{messages:?}");
    for result in tool_results {
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = result
        else {
            panic!("expected a tool result");
        };
        assert!(is_error, "{content}");
        assert!(content.contains("cut off"), "{content}");
        assert!(content.contains("byte budget"), "{content}");
    }
}

#[tokio::test]
async fn a_cut_round_raises_the_fill_it_never_billed() {
    let dir = tempdir().unwrap();
    // A capped round never reports usage, so the turn has to bill what the
    // next request carries. With a small window the cut must reach the hard cap
    // instead of sending the kept text. The kept answer fits the window, but
    // the history it joins does not fit the cap the turn watches: a 10k window
    // keeps a 7.5k cap, and the prompt alone is 8k tokens.
    let cut_answer = vec![
        StreamingEvent::TextDelta("partial answer".into()),
        StreamingEvent::ThinkingDelta(noisy_reasoning(MAX_ROUND_BYTES + 1, 16)),
    ];
    let mut agent = agent_with(vec![cut_answer, final_round()], dir.path());
    agent.model.context_window = Some(10_000);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg(&"h".repeat(32_000))];
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

    let mut saw_pressure = false;
    while let Ok(event) = rx.try_recv() {
        if matches!(event, AgentEvent::ContextPressure { .. }) {
            saw_pressure = true;
        }
    }
    assert!(saw_pressure, "the cut round raises the fill over the cap");
    assert!(
        !messages
            .iter()
            .any(|message| message.blocks.iter().any(
                |block| matches!(block, ContentBlock::Text { text } if text == "final answer")
            )),
        "the kept text must not be sent to a model without room: {messages:?}"
    );
}
#[tokio::test]
async fn a_signature_does_not_count_toward_the_reply_budget() {
    let dir = tempdir().unwrap();
    // An encrypted reasoning blob bigger than the budget with a small answer:
    // the model never re-reads the blob, so the round is not cut.
    let round = vec![
        StreamingEvent::TextDelta("the answer".into()),
        StreamingEvent::ThinkingSignature("s".repeat(MAX_ROUND_BYTES + 1)),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![round], dir.path());
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
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

    assert!(
        messages
            .iter()
            .all(|message| message.kind != PromptKind::Notice),
        "{messages:?}"
    );
    assert!(messages.iter().any(|message| message
        .blocks
        .iter()
        .any(|block| matches!(block, ContentBlock::Text { text } if text == "the answer"))));
}
