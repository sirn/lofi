#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

#[tokio::test]
async fn run_uses_the_centralized_retry_session() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![
            vec![StreamingEvent::Error("HTTP 500 transient".into())],
            final_round(),
        ],
        dir.path(),
    )
    .with_retry(crate::retry::RetryPolicy {
        max_retries: 1,
        base_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);

    agent.run("go".to_string(), tx).await.unwrap();

    let mut retry_starts = Vec::new();
    let mut reached_final = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::RetryStart { attempt, .. } => retry_starts.push(attempt),
            AgentEvent::Text(text) if text == "final answer" => reached_final = true,
            _ => {}
        }
    }
    assert_eq!(retry_starts, vec![1]);
    assert!(reached_final);
}

#[tokio::test]
async fn run_continuation_retries_transient_provider_errors() {
    let dir = tempdir().unwrap();
    let round1 = vec![StreamingEvent::Error("HTTP 429 Too Many Requests".into())];
    let round2 = vec![
        StreamingEvent::TextDelta("recovered".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent =
        agent_with(vec![round1, round2], dir.path()).with_retry(crate::retry::RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            ..Default::default()
        });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let mut messages = vec![user_msg("go")];
    let result = agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".to_string(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await;
    assert!(result.is_ok(), "should recover: {result:?}");
    let mut got_start = false;
    let mut got_end_success = false;
    let mut got_transcript_error = false;
    while let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        match ev {
            AgentEvent::RetryStart { attempt, .. } => {
                assert_eq!(attempt, 1);
                got_start = true;
            }
            AgentEvent::RetryEnd {
                success, attempt, ..
            } => {
                assert_eq!(attempt, 1);
                if success {
                    got_end_success = true;
                }
            }
            AgentEvent::Error(_) => got_transcript_error = true,
            _ => {}
        }
    }
    assert!(got_start, "expected a RetryStart event");
    assert!(got_end_success, "expected a RetryEnd(success) event");
    assert!(
        !got_transcript_error,
        "a recovered transient error must not enter the transcript"
    );
}

#[tokio::test]
async fn successful_provider_round_resets_retry_attempt_count() {
    let dir = tempdir().unwrap();
    let tool_input = serde_json::json!({ "code": "return 1" }).to_string();
    let tool_round = vec![
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
    let final_round = vec![
        StreamingEvent::TextDelta("recovered again".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = agent_with(
        vec![
            vec![StreamingEvent::Error("HTTP 500 first".into())],
            tool_round,
            vec![StreamingEvent::Error("HTTP 500 second".into())],
            final_round,
        ],
        dir.path(),
    )
    .with_retry(crate::retry::RetryPolicy {
        max_retries: 3,
        base_delay: Duration::from_millis(1),
        ..Default::default()
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".to_string(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let mut starts = Vec::new();
    let mut successful_ends = Vec::new();
    while let Some(event) = rx.recv().await {
        match event {
            AgentEvent::RetryStart { attempt, .. } => starts.push(attempt),
            AgentEvent::RetryEnd {
                success: true,
                attempt,
                ..
            } => successful_ends.push(attempt),
            _ => {}
        }
    }
    assert_eq!(starts, vec![1, 1]);
    assert_eq!(successful_ends, vec![1, 1]);
}

#[tokio::test]
async fn provider_progress_resets_retry_attempt_count_before_a_later_failure() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![
            vec![StreamingEvent::Error("HTTP 500 before progress".into())],
            vec![
                StreamingEvent::TextDelta("partial response".into()),
                StreamingEvent::Error("HTTP 500 after progress".into()),
            ],
            vec![
                StreamingEvent::TextDelta("recovered".into()),
                StreamingEvent::Done {
                    usage: Usage::default(),
                    stop_reason: None,
                },
            ],
        ],
        dir.path(),
    )
    .with_retry(crate::retry::RetryPolicy {
        max_retries: 1,
        base_delay: Duration::from_millis(1),
        ..Default::default()
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let mut messages = vec![user_msg("go")];

    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".to_string(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let mut starts = Vec::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::RetryStart { attempt, .. } = event {
            starts.push(attempt);
        }
    }
    assert_eq!(starts, vec![1, 1]);
}

#[tokio::test]
async fn provider_error_events_do_not_reset_retry_attempt_count() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![
            vec![StreamingEvent::Error("HTTP 500 first".into())],
            vec![StreamingEvent::Error("HTTP 500 second".into())],
            vec![
                StreamingEvent::TextDelta("must not be reached".into()),
                StreamingEvent::Done {
                    usage: Usage::default(),
                    stop_reason: None,
                },
            ],
        ],
        dir.path(),
    )
    .with_retry(crate::retry::RetryPolicy {
        max_retries: 1,
        base_delay: Duration::from_millis(1),
        ..Default::default()
    });
    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let mut messages = vec![user_msg("go")];

    let result = agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".to_string(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await;

    assert!(result.is_err());
    let mut starts = Vec::new();
    while let Some(event) = rx.recv().await {
        if let AgentEvent::RetryStart { attempt, .. } = event {
            starts.push(attempt);
        }
    }
    assert_eq!(starts, vec![1]);
}

#[tokio::test]
async fn run_continuation_does_not_retry_non_transient_errors() {
    let dir = tempdir().unwrap();
    let round1 = vec![StreamingEvent::Error("401 Unauthorized".into())];
    let round2 = vec![
        StreamingEvent::TextDelta("should-not-happen".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent =
        agent_with(vec![round1, round2], dir.path()).with_retry(crate::retry::RetryPolicy {
            max_retries: 3,
            base_delay: Duration::from_millis(1),
            ..Default::default()
        });
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    let mut messages = vec![user_msg("go")];
    let result = agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("go".to_string(), lofi_types::PromptKind::User),
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await;
    assert!(result.is_err(), "non-retryable errors should propagate");
}

#[tokio::test]
async fn run_exits_when_receiver_dropped() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("hi".to_string()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: None,
            },
        ]],
        dir.path(),
    );
    let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
    drop(rx);
    // The run should exit gracefully (Ok) rather than hang or surface an
    // error: a dropped receiver is a cancellation, not a provider fault.
    let result =
        tokio::time::timeout(Duration::from_secs(2), agent.run("hi".to_string(), tx)).await;
    let inner = result.unwrap(); // timeout => run hung
    assert!(inner.is_ok(), "run should exit gracefully, got {inner:?}");
}
