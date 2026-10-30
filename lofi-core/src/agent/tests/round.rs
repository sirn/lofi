#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

#[tokio::test]
async fn run_once_adds_configured_system_to_provider_request() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let agent = Agent {
        provider: provider.clone(),
        ..agent_with(Vec::new(), dir.path())
    };
    let mut messages = vec![user_msg("hi")];

    assert!(agent.run_once(&mut messages).await.unwrap());

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0][0], system_msg("sys"));
    assert_eq!(requests[0][1], user_msg("hi"));
    assert_eq!(messages[0], user_msg("hi"));
}

#[tokio::test]
async fn run_once_ignores_empty_history_system() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let agent = Agent {
        provider: provider.clone(),
        ..agent_with(Vec::new(), dir.path())
    };
    let mut messages = vec![system_msg(""), user_msg("hi")];

    assert!(agent.run_once(&mut messages).await.unwrap());

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests[0][0], system_msg("sys"));
}

#[tokio::test]
async fn run_once_keeps_history_system_in_provider_request() {
    let dir = tempdir().unwrap();
    let provider = Arc::new(RecordingProvider::default());
    let agent = Agent {
        provider: provider.clone(),
        ..agent_with(Vec::new(), dir.path())
    };
    let mut messages = vec![system_msg("restored"), user_msg("hi")];

    assert!(agent.run_once(&mut messages).await.unwrap());

    let requests = provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0][0], system_msg("restored"));
    assert_eq!(
        requests[0]
            .iter()
            .filter(|message| message.role == Role::System)
            .count(),
        1
    );
}

#[tokio::test]
async fn run_once_text_only_finishes() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("hello".to_string()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: None,
            },
        ]],
        dir.path(),
    );
    let mut messages = vec![user_msg("hi")];
    let finished = agent.run_once(&mut messages).await.unwrap();
    assert!(finished);
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, Role::Assistant);
    match &messages[1].blocks[0] {
        ContentBlock::Text { text } => assert_eq!(text, "hello"),
        other => panic!("unexpected block {other:?}"),
    }
}

#[tokio::test]
async fn run_once_stamps_assistant_message_origin() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::ThinkingDelta("summary".to_string()),
            StreamingEvent::ThinkingSignature("encrypted".to_string()),
            StreamingEvent::TextDelta("answer".to_string()),
            StreamingEvent::Done {
                usage: Usage::default(),
                stop_reason: None,
            },
        ]],
        dir.path(),
    );
    let mut messages = vec![user_msg("hi")];

    assert!(agent.run_once(&mut messages).await.unwrap());

    assert_eq!(
        messages[1].origin.as_ref(),
        Some(&lofi_types::ModelOrigin::from(&agent.model))
    );
    assert!(matches!(
        messages[1].blocks[0],
        ContentBlock::Thinking { .. }
    ));
}

#[tokio::test]
async fn run_once_tool_call_executes_and_appends_result() {
    let dir = tempdir().unwrap();
    let round1 = exec_round("return 1+1");
    let round2 = vec![
        StreamingEvent::TextDelta("done".to_string()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = agent_with(vec![round1, round2], dir.path());
    let mut messages = vec![user_msg("go")];

    let finished = agent.run_once(&mut messages).await.unwrap();
    assert!(!finished);
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[2].role, Role::Tool);
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &messages[2].blocks[0]
    else {
        panic!("expected tool_result");
    };
    assert!(!*is_error);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(content).unwrap(),
        serde_json::json!({ "ok": true, "value": 2 })
    );

    let finished = agent.run_once(&mut messages).await.unwrap();
    assert!(finished);
}

#[tokio::test]
async fn run_once_exec_without_return_reports_success_and_logs() {
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![exec_round("print('done')")], dir.path());
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult { content, .. } = &messages[2].blocks[0] else {
        panic!("expected tool_result");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(content).unwrap(),
        serde_json::json!({ "ok": true, "logs": "done\n" })
    );
}

#[tokio::test]
async fn run_once_exec_preserves_explicit_null_return() {
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![exec_round("return null")], dir.path());
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult { content, .. } = &messages[2].blocks[0] else {
        panic!("expected tool_result");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(content).unwrap(),
        serde_json::json!({ "ok": true, "value": null })
    );
}

#[tokio::test]
async fn run_once_exec_without_return_or_logs_reports_success() {
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![exec_round("const done = true")], dir.path());
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult { content, .. } = &messages[2].blocks[0] else {
        panic!("expected tool_result");
    };
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(content).unwrap(),
        serde_json::json!({ "ok": true })
    );
}

#[tokio::test]
async fn run_once_exec_reports_unawaited_bash_failure() {
    let dir = tempdir().unwrap();
    let agent = Agent {
        shell_policy: lofi_code::policy::defaults::resolve(&lofi_types::ShellPolicyConfig {
            mode: lofi_types::ShellPolicyMode::Unrestricted,
            ..lofi_types::ShellPolicyConfig::default()
        }),
        ..agent_with(vec![exec_round("lofi.bash({ cmd: 'exit 7' })")], dir.path())
    };
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &messages[2].blocks[0]
    else {
        panic!("expected tool_result");
    };
    assert!(*is_error);
    let result = serde_json::from_str::<serde_json::Value>(content).unwrap();
    assert_eq!(result["ok"], serde_json::json!(false));
    assert_eq!(result["errors"][0]["tool"], serde_json::json!("bash"));
    assert_eq!(result["errors"][0]["result"]["code"], serde_json::json!(7));
}

#[tokio::test]
async fn run_once_exec_without_description_is_rejected() {
    let dir = tempdir().unwrap();
    let round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: serde_json::json!({ "code": "return 1" }).to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        },
    ];
    let agent = agent_with(vec![round], dir.path());
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &messages[2].blocks[0]
    else {
        panic!("expected tool_result");
    };
    assert!(*is_error);
    assert_eq!(content, "exec: missing required 'description' argument");
}

#[tokio::test]
async fn run_once_exec_with_blank_description_is_rejected() {
    let dir = tempdir().unwrap();
    let round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: serde_json::json!({
                "code": "return 1",
                "description": "   "
            })
            .to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        },
    ];
    let agent = agent_with(vec![round], dir.path());
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &messages[2].blocks[0]
    else {
        panic!("expected tool_result");
    };
    assert!(*is_error);
    assert_eq!(content, "exec: missing required 'description' argument");
}

#[tokio::test]
async fn run_once_exec_without_code_is_rejected() {
    let dir = tempdir().unwrap();
    let round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: serde_json::json!({ "description": "run code" }).to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        },
    ];
    let agent = agent_with(vec![round], dir.path());
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &messages[2].blocks[0]
    else {
        panic!("expected tool_result");
    };
    assert!(*is_error);
    assert_eq!(content, "exec: missing required 'code' argument");
}

#[tokio::test]
async fn two_exec_calls_pair_each_command_with_its_own_description() {
    // The intent travels on the call's own context, so commands of two
    // calls in one round are never judged against the other call's
    // description.
    let dir = tempdir().unwrap();
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let auto: lofi_code::AutoModeFn = {
        let seen = seen.clone();
        Arc::new(move |query| {
            let intent = query.intent.clone().unwrap_or_default();
            seen.lock().unwrap().push((query.command.clone(), intent));
            Box::pin(async {
                lofi_code::AutoModeOutcome::Allow {
                    reason: "safe".to_string(),
                }
            })
        })
    };
    let two_calls = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: serde_json::json!({
                "code": "return (await lofi.bash({ cmd: 'first-cmd' })).ok",
                "description": "first call"
            })
            .to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::ToolUseStart {
            id: "t2".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t2".to_string(),
            delta: serde_json::json!({
                "code": "return (await lofi.bash({ cmd: 'second-cmd' })).ok",
                "description": "second call"
            })
            .to_string(),
        },
        StreamingEvent::ToolUseEnd {
            id: "t2".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = Agent {
        auto_mode: Some(auto),
        ..agent_with(vec![two_calls], dir.path())
    };
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("first-cmd".to_string(), "first call".to_string()),
            ("second-cmd".to_string(), "second call".to_string()),
        ],
        "each command must be judged with its own call's description"
    );
}

#[tokio::test]
async fn run_once_exec_description_reaches_the_shell_gate() {
    // A shell command inside exec is judged with the call's stated
    // description: the gate reads it as the intent hint.
    let dir = tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(None));
    let auto: lofi_code::AutoModeFn = {
        let seen = seen.clone();
        Arc::new(move |query| {
            *seen.lock().unwrap() = query.intent;
            Box::pin(async {
                lofi_code::AutoModeOutcome::Allow {
                    reason: "safe".to_string(),
                }
            })
        })
    };
    let agent = Agent {
        auto_mode: Some(auto),
        ..agent_with(
            vec![exec_round(
                "return (await lofi.bash({ cmd: 'special-cmd' })).ok",
            )],
            dir.path(),
        )
    };
    let mut messages = vec![user_msg("go")];

    assert!(!agent.run_once(&mut messages).await.unwrap());
    assert_eq!(
        *seen.lock().unwrap(),
        Some("run code".to_string()),
        "the evaluator must see the exec call's description"
    );
}

#[tokio::test]
async fn run_once_stops_at_done_without_polling_stream_again() {
    // A protocol terminal event is sufficient even when the transport keeps
    // the connection open. This is the Responses terminal-stream regression.
    let dir = tempdir().unwrap();
    let provider = Arc::new(TerminalThenPendingProvider);
    let agent = Agent {
        provider,
        ..agent_with(Vec::new(), dir.path())
    };
    let mut messages = vec![user_msg("go")];
    let finished = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        agent.run_once(&mut messages),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(finished);
}
