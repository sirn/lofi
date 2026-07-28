#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;
use async_trait::async_trait;
use futures::{stream, StreamExt};
use lofi_types::{Api, SessionEventKind, Usage};
use tempfile::tempdir;

struct MockProvider {
    rounds: std::sync::Mutex<Vec<Vec<StreamingEvent>>>,
}

struct PendingAfterRoundProvider {
    first: std::sync::Mutex<Option<Vec<StreamingEvent>>>,
}

#[async_trait]
impl Provider for PendingAfterRoundProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        let first = self.first.lock().unwrap().take();
        match first {
            Some(events) => Ok(Box::pin(stream::iter(events.into_iter().map(Ok)))),
            None => std::future::pending().await,
        }
    }
}

struct TerminalThenPendingProvider;

#[async_trait]
impl Provider for TerminalThenPendingProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        Ok(Box::pin(
            stream::iter([
                Ok(StreamingEvent::TextDelta("done".to_string())),
                Ok(StreamingEvent::Done(Usage::default())),
            ])
            .chain(stream::pending()),
        ))
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        let mut rounds = self.rounds.lock().unwrap();
        let evs = if rounds.is_empty() {
            Vec::new()
        } else {
            rounds.remove(0)
        };
        Ok(Box::pin(stream::iter(evs.into_iter().map(Ok))))
    }
}

fn model() -> Model {
    Model {
        id: "m".to_string(),
        name: "m".to_string(),
        provider: "p".to_string(),
        api: Api::OpenAiCompletions,
        reasoning: false,
        thinking: lofi_types::ThinkingLevel::Off,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    }
}

fn agent_with(rounds: Vec<Vec<StreamingEvent>>, root: &std::path::Path) -> Agent {
    Agent {
        provider: Arc::new(MockProvider {
            rounds: std::sync::Mutex::new(rounds),
        }),
        model: model(),
        root: root.to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-agent-test"),
        retry: crate::retry::RetryPolicy::default(),
        system_prompt: "sys".to_string(),
        max_output_tokens: None,
        reserved_context_tokens: 0,
        bash_env: lofi_code::BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm_tx: None,
        confirm_counter: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        auto_mode: None,
        skills_dir: None,
    }
}

fn user_msg(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

#[test]
fn cap_tool_result_keeps_short_unchanged() {
    assert_eq!(cap_tool_result("hello"), "hello");
}

#[test]
fn cap_tool_result_truncates_long_with_marker() {
    let big = "x".repeat(MAX_TOOL_RESULT_BYTES + 1000);
    let capped = cap_tool_result(&big);
    assert!(capped.len() < big.len());
    assert!(capped.contains("[output truncated:"));
    assert!(capped.contains(&format!("{} bytes total", big.len())));
    assert!(capped.starts_with("xxxx"));
}

#[test]
fn cap_tool_result_respects_char_boundary() {
    // Fill to just past the cap with multibyte chars so the boundary cut
    // would land mid-char if unhandled.
    let unit = "é"; // 2 bytes
    let n = MAX_TOOL_RESULT_BYTES / 2 + 50;
    let big = unit.repeat(n);
    let capped = cap_tool_result(&big);
    assert!(capped.contains("[output truncated:"));
}

#[test]
fn cap_exec_result_uses_larger_outer_limit() {
    // The exec-level cap is larger than the per-tool cap, so a payload
    // bigger than MAX_TOOL_RESULT_BYTES but under MAX_EXEC_RESULT_BYTES is
    // preserved by cap_exec_result but would be truncated by cap_tool_result.
    let mid = "x".repeat(MAX_TOOL_RESULT_BYTES + 1000);
    assert!(mid.len() < MAX_EXEC_RESULT_BYTES);
    assert_eq!(cap_exec_result(&mid), mid);
    assert_ne!(cap_tool_result(&mid), mid);
    let huge = "x".repeat(MAX_EXEC_RESULT_BYTES + 5000);
    let capped = cap_exec_result(&huge);
    assert!(capped.contains("[output truncated:"));
    assert!(capped.contains(&format!("{} bytes total", huge.len())));
}

#[test]
fn exec_schema_uses_code_contract() {
    let schema = exec_tool_schema();
    assert_eq!(schema.name, lofi_code::EXEC_TOOL_NAME);
    assert_eq!(schema.description, lofi_code::EXEC_TOOL_DESCRIPTION);
    assert_eq!(schema.input_schema, lofi_code::exec_tool_input_schema());
}

#[test]
fn parse_exec_input_extracts_fields() {
    let input = serde_json::json!({
        "code": "return 1",
        "strings": { "a": "x", "b": 2 },
        "display": { "hint": "row" }
    });
    let (code, strings, display) = parse_exec_input(&input);
    assert_eq!(code, "return 1");
    assert_eq!(strings.get("a").unwrap(), "x");
    assert_eq!(strings.get("b").unwrap(), "2");
    assert_eq!(display["hint"], "row");
}

#[test]
fn parse_exec_input_missing_code_is_empty() {
    let (code, strings, display) = parse_exec_input(&serde_json::Value::Null);
    assert!(code.is_empty());
    assert!(strings.is_empty());
    assert!(display.is_null());
}

#[test]
fn parse_exec_input_trims_surrounding_newlines() {
    let input = serde_json::json!({ "code": "\nreturn 1\n" });
    let (code, _, _) = parse_exec_input(&input);
    assert_eq!(code, "return 1");
}

#[test]
fn extract_code_prefix_streams_and_trims_leading_newline() {
    let cases = [
        (r#"{"code":"\nle"#, "le"),
        (r#"{"code":"\nlet x"#, "let x"),
        (r#"{"code":"\nlet x\nlet y"#, "let x\nlet y"),
        (r#"{"display":"z","code":"\nlet x"#, "let x"),
    ];
    for (raw, want) in cases {
        assert_eq!(extract_code_prefix(raw), want, "raw: {raw}");
    }
}

#[test]
fn code_prefix_decoder_handles_split_escape_incrementally() {
    let mut decoder = CodePrefixDecoder::default();
    assert_eq!(decoder.update(r#"{"code":"\nlet x\u"#), "let x");
    assert_eq!(decoder.update(r#"{"code":"\nlet x\u0061"}"#), "let xa");
}

#[test]
fn initial_history_with_and_without_system() {
    let h = initial_history("sys", "hi");
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].role, Role::System);
    let h = initial_history("", "hi");
    assert_eq!(h.len(), 1);
    assert_eq!(h[0].role, Role::User);
}

#[tokio::test]
async fn run_once_text_only_finishes() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("hello".to_string()),
            StreamingEvent::Done(Usage::default()),
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
async fn run_once_tool_call_executes_and_appends_result() {
    let dir = tempdir().unwrap();
    let tool_input = serde_json::json!({ "code": "return 1+1" }).to_string();
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
        StreamingEvent::Done(Usage::default()),
    ];
    let round2 = vec![
        StreamingEvent::TextDelta("done".to_string()),
        StreamingEvent::Done(Usage::default()),
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
    assert!(content.contains('2'), "content was {content}");

    let finished = agent.run_once(&mut messages).await.unwrap();
    assert!(finished);
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

#[tokio::test]
async fn run_continuation_persists_completed_round_before_next_round_settles() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":2,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
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
        StreamingEvent::Done(Usage::default()),
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
    let run = agent.run_continuation(
        &mut messages,
        "go".into(),
        tx,
        Some(&cursor),
        false,
        None,
        None,
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
            StreamingEvent::Done(Usage {
                input_tokens,
                ..Usage::default()
            }),
        ]
    };
    let agent = Agent {
        provider: Arc::new(MockProvider {
            rounds: std::sync::Mutex::new(vec![tool_round(10), tool_round(500)]),
        }),
        model: {
            let mut m = model();
            m.context_window = Some(100);
            m
        },
        root: dir.path().to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-agent-test"),
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
        skills_dir: None,
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = vec![user_msg("go")];
    agent
        .run_continuation(&mut messages, String::new(), tx, None, false, None, None)
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
        StreamingEvent::Done(Usage::default()),
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

#[tokio::test]
async fn run_retries_transient_provider_errors() {
    let dir = tempdir().unwrap();
    let round1 = vec![StreamingEvent::Error("HTTP 429 Too Many Requests".into())];
    let round2 = vec![
        StreamingEvent::TextDelta("recovered".into()),
        StreamingEvent::Done(Usage::default()),
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
        .run_continuation(&mut messages, "go".to_string(), tx, None, false, None, None)
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
        StreamingEvent::Done(Usage::default()),
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("recovered again".into()),
        StreamingEvent::Done(Usage::default()),
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
        .run_continuation(&mut messages, "go".to_string(), tx, None, false, None, None)
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
async fn run_does_not_retry_non_transient_errors() {
    let dir = tempdir().unwrap();
    let round1 = vec![StreamingEvent::Error("401 Unauthorized".into())];
    let round2 = vec![
        StreamingEvent::TextDelta("should-not-happen".into()),
        StreamingEvent::Done(Usage::default()),
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
        .run_continuation(&mut messages, "go".to_string(), tx, None, false, None, None)
        .await;
    assert!(result.is_err(), "non-retryable errors should propagate");
}

#[tokio::test]
async fn run_exits_when_receiver_dropped() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![vec![
            StreamingEvent::TextDelta("hi".to_string()),
            StreamingEvent::Done(Usage::default()),
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

use indexmap::IndexMap;
use lofi_types::{
    AgentConfig, ApiTypeMapping, CompactionConfig, Config, ModelConfig, PricingConvention,
    PricingFieldMappings, ProviderConfig, ThinkingLevel,
};

fn mc() -> ModelConfig {
    ModelConfig {
        name: None,
        api_type: None,
        reasoning: None,
        supports_image: None,
        context_window: None,
        max_tokens: None,
        thinking_levels: vec![
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ],
        thinking_level: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    }
}

fn mc_levels(levels: &[ThinkingLevel]) -> ModelConfig {
    let mut m = mc();
    m.thinking_levels = levels.to_vec();
    m
}

fn mc_default(level: ThinkingLevel) -> ModelConfig {
    let mut m = mc();
    m.thinking_level = Some(level);
    m
}

fn models(entries: &[(&str, ModelConfig)]) -> IndexMap<String, ModelConfig> {
    let mut map = IndexMap::new();
    for (k, v) in entries {
        map.insert((*k).to_string(), v.clone());
    }
    map
}

fn provider(
    api: Api,
    key: Option<&str>,
    models: IndexMap<String, ModelConfig>,
    thinking_level: Option<ThinkingLevel>,
) -> ProviderConfig {
    let mut mappings = IndexMap::new();
    mappings.insert(
        api.as_str().to_string(),
        ApiTypeMapping {
            path: None,
            pricing_field_mappings: None,
        },
    );
    ProviderConfig {
        api_type: Some(api),
        api_types: mappings,
        base_url: Some("https://api.example.com".to_string()),
        pricing_convention: PricingConvention::PerToken,
        pricing_field_mappings: PricingFieldMappings::default(),
        env_name: None,
        api_key: key.map(str::to_string),
        headers: None,
        models,
        auto_models: None,
        no_auth: false,
        thinking_level,
        thinking_levels: Vec::new(),
    }
}

fn build(providers: IndexMap<String, ProviderConfig>) -> (Config, ModelRegistry) {
    let cfg = Config {
        agent: AgentConfig::default(),
        compaction: CompactionConfig::default(),
        bash: lofi_types::BashConfig::default(),
        shell_policy: lofi_types::ShellPolicyConfig::default(),
        retry: lofi_types::RetryConfig::default(),
        default_provider: None,
        default_model: None,
        providers,
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    (cfg, reg)
}

fn one_provider(p: ProviderConfig) -> IndexMap<String, ProviderConfig> {
    let mut m = IndexMap::new();
    m.insert("openai".to_string(), p);
    m
}

#[test]
fn select_model_explicit_qualified() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(m.provider, "openai");
    assert_eq!(level, ThinkingLevel::Medium);
}

#[test]
fn select_model_explicit_level_suffix() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:xhigh")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(level, ThinkingLevel::XHigh);
}

#[test]
fn select_model_off_level_always_allowed() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_levels(&[]))]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:off")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(level, ThinkingLevel::Off);
}

#[test]
fn select_model_rejects_bare_model() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("gpt-4o")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_rejects_unknown_level() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o:bogus")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_rejects_level_not_declared() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[(
            "gpt-4o",
            mc_levels(&[ThinkingLevel::Low, ThinkingLevel::Medium]),
        )]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o:high")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_first_available_when_no_query() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let (cfg, reg) = build(providers);
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "openai");
    assert_eq!(m.id, "gpt-4o");
}

#[test]
fn select_model_uses_default_model_when_no_query() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        agent: AgentConfig::default(),
        compaction: CompactionConfig::default(),
        bash: lofi_types::BashConfig::default(),
        shell_policy: lofi_types::ShellPolicyConfig::default(),
        retry: lofi_types::RetryConfig::default(),
        default_provider: None,
        default_model: Some("anthropic/claude".to_string()),
        providers,
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.id, "claude");
}

#[test]
fn select_model_uses_default_provider_when_no_query() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        agent: AgentConfig::default(),
        compaction: CompactionConfig::default(),
        bash: lofi_types::BashConfig::default(),
        shell_policy: lofi_types::ShellPolicyConfig::default(),
        retry: lofi_types::RetryConfig::default(),
        default_provider: Some("anthropic".to_string()),
        default_model: None,
        providers,
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.id, "claude");
}

#[test]
fn select_model_default_model_overrides_default_provider() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        agent: AgentConfig::default(),
        compaction: CompactionConfig::default(),
        bash: lofi_types::BashConfig::default(),
        shell_policy: lofi_types::ShellPolicyConfig::default(),
        retry: lofi_types::RetryConfig::default(),
        default_provider: Some("anthropic".to_string()),
        default_model: Some("openai/gpt-4o".to_string()),
        providers,
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "openai");
    assert_eq!(m.id, "gpt-4o");
}

#[test]
fn select_model_explicit_query_overrides_defaults() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        agent: AgentConfig::default(),
        compaction: CompactionConfig::default(),
        bash: lofi_types::BashConfig::default(),
        shell_policy: lofi_types::ShellPolicyConfig::default(),
        retry: lofi_types::RetryConfig::default(),
        default_provider: Some("openai".to_string()),
        default_model: Some("openai/gpt-4o".to_string()),
        providers,
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, Some("anthropic/claude")).unwrap();
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.id, "claude");
}

#[test]
fn select_model_no_models_error() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        None,
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, None).unwrap_err();
    assert!(matches!(err, Error::NoModels(_)));
}

#[test]
fn select_model_rejects_keyless_provider_model() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "local".to_string(),
        provider(
            Api::OpenAiCompletions,
            None,
            models(&[("local-model", mc())]),
            None,
        ),
    );
    let (cfg, reg) = build(providers);
    let err = select_model(&reg, &cfg, Some("local/local-model")).unwrap_err();
    // A disabled (keyless) provider now reports as "no models" rather than
    // a config error, so the TUI can launch in no-model mode.
    assert!(matches!(err, Error::NoModels(_)));
}

#[test]
fn select_model_thinking_precedence_model_default() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk"),
        models(&[("gpt-4o", mc_default(ThinkingLevel::High))]),
        None,
    )));
    let (_, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(level, ThinkingLevel::High);
}

#[test]
fn select_model_thinking_precedence_provider_default() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk"),
        models(&[("gpt-4o", mc())]),
        Some(ThinkingLevel::Low),
    )));
    let (_, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(level, ThinkingLevel::Low);
}

#[test]
fn add_usage_bills_cache_tokens_at_their_own_rate() {
    let model = Model {
        id: "m".to_string(),
        name: "m".to_string(),
        provider: "p".to_string(),
        api: Api::OpenAiCompletions,
        reasoning: false,
        thinking: ThinkingLevel::Off,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: Some(1.0),
        output_price: Some(2.0),
        cache_read_price: Some(0.1),
        cache_write_price: Some(0.5),
        per_request_price: None,
    };
    let mut stats = TurnStats::new();
    stats.add_usage(
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 1_000_000,
        },
        &model,
    );
    assert!((stats.cost - 3.70).abs() < 1e-9, "cost was {}", stats.cost);
}

#[test]
fn add_usage_falls_back_to_input_rate_when_cache_prices_unset() {
    let model = Model {
        id: "m".to_string(),
        name: "m".to_string(),
        provider: "p".to_string(),
        api: Api::OpenAiCompletions,
        reasoning: false,
        thinking: ThinkingLevel::Off,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: Some(1.0),
        output_price: Some(2.0),
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    };
    let mut stats = TurnStats::new();
    stats.add_usage(
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 1_000_000,
        },
        &model,
    );
    assert!((stats.cost - 6.0).abs() < 1e-9, "cost was {}", stats.cost);
}
