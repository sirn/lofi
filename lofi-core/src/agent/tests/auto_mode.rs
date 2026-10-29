#![allow(clippy::unwrap_used)]

use super::*;
use crate::agent::auto_mode::{derive_hints, evaluate_command};
use crate::compact::HANDOFF_PREAMBLE;
use lofi_code::AutoModeOutcome;
use lofi_types::PromptKind;

fn typed(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::User,
    }
}

fn notice(text: &str) -> Message {
    Message {
        kind: PromptKind::Notice,
        ..typed(text)
    }
}

fn assistant(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::User,
    }
}

fn tool_result() -> Message {
    Message {
        origin: None,
        role: Role::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "ignored".into(),
            is_error: false,
            images: Vec::new(),
        }],
        kind: PromptKind::User,
    }
}

#[test]
fn hints_carry_the_current_request_even_when_substantive() {
    let hints = derive_hints(&[], Some(("delete the build directory", PromptKind::User)));
    assert_eq!(hints.prompt.as_deref(), Some("delete the build directory"));
    assert_eq!(hints.prior_prompt, None);
    assert_eq!(hints.prior_reply, None);
    assert_eq!(hints.parent_task, None);
}

#[test]
fn hints_keep_the_head_and_tail_of_a_long_request() {
    let long = format!("never push {} now", "a".repeat(900));
    let hints = derive_hints(&[], Some((long.as_str(), PromptKind::User)));
    let prompt = hints.prompt.unwrap();
    assert!(prompt.starts_with("never push "));
    assert!(prompt.ends_with("now"));
    assert!(prompt.contains("[…truncated]"));
}

#[test]
fn a_subagent_task_is_context_not_authorization() {
    let history = [Message {
        kind: PromptKind::Task,
        ..typed("push and comment on PR 12")
    }];
    // The child's first turn: the task is the current prompt.
    let hints = derive_hints(&[], Some(("push and comment on PR 12", PromptKind::Task)));
    assert_eq!(hints.prompt, None);
    assert_eq!(
        hints.parent_task.as_deref(),
        Some("push and comment on PR 12")
    );
    // A resumed child replays the task from its history.
    let hints = derive_hints(&history, None);
    assert_eq!(
        hints.parent_task.as_deref(),
        Some("push and comment on PR 12")
    );
    assert_eq!(hints.prompt, None);
}

#[test]
fn terse_request_carries_the_earlier_request_and_the_agent_question() {
    let history = [
        typed("abandon the last change"),
        assistant("Cancel the \"tidy\" change?"),
    ];
    let hints = derive_hints(&history, Some(("yes", PromptKind::User)));
    assert_eq!(hints.prompt.as_deref(), Some("yes"));
    assert_eq!(
        hints.prior_prompt.as_deref(),
        Some("abandon the last change")
    );
    assert_eq!(
        hints.prior_reply.as_deref(),
        Some("Cancel the \"tidy\" change?")
    );
}

#[test]
fn hints_prefer_the_last_typed_request() {
    let history = [typed("first"), typed("second")];
    let hints = derive_hints(&history, Some(("do that", PromptKind::User)));
    assert_eq!(hints.prior_prompt.as_deref(), Some("second"));
}

#[test]
fn hints_ignore_notices_tool_results_and_compaction_summaries() {
    let history = [
        typed("abandon the last change"),
        notice("job 3 finished"),
        tool_result(),
        typed(&format!("{HANDOFF_PREAMBLE}\n\n[Session Goal]\n- push it")),
    ];
    let hints = derive_hints(&history, Some(("yes", PromptKind::User)));
    assert_eq!(
        hints.prior_prompt.as_deref(),
        Some("abandon the last change")
    );
}

#[test]
fn continue_turn_clears_the_hints() {
    let history = [typed("abandon the last change")];
    assert_eq!(
        derive_hints(&history, None),
        lofi_code::AutoModeHints::default()
    );
}

/// The evaluator answers a one-shot text call; nothing else matters here.
struct ReplyBackend {
    reply: &'static str,
    delay: std::time::Duration,
}

#[async_trait]
impl crate::backend::Backend for ReplyBackend {
    fn name(&self) -> &'static str {
        "test"
    }

    fn selection(&self) -> crate::backend::ModelSelection {
        crate::backend::ModelSelection {
            model: "reply".into(),
            thinking: ThinkingLevel::Off,
            service_tier: ServiceTier::Auto,
        }
    }

    fn choice(&self) -> lofi_types::ModelChoice {
        lofi_types::ModelChoice {
            provider: "test".into(),
            id: "reply".into(),
            name: "reply".into(),
            thinking_levels: Vec::new(),
            service_tiers: Vec::new(),
            supports_image: false,
            context_window: None,
        }
    }

    fn system_prompt(&self) -> &'static str {
        ""
    }

    fn tool_executor(&self) -> ToolExecutor {
        panic!("the evaluator never executes tools")
    }

    fn with_selection(&self, _query: &str) -> Result<crate::backend::BackendRef> {
        panic!("not used")
    }

    fn with_tool_executor(&self, _executor: ToolExecutor) -> crate::backend::BackendRef {
        panic!("not used")
    }

    fn with_system_prompt(&self, _prompt: String) -> crate::backend::BackendRef {
        panic!("not used")
    }

    fn fork(&self, _executor: ToolExecutor) -> crate::backend::BackendRef {
        panic!("not used")
    }

    fn subagents(&self) -> crate::SubagentRegistry {
        panic!("not used")
    }

    fn subagent_hook(&self) -> Option<lofi_code::SubagentFn> {
        None
    }

    async fn run_turn(
        &self,
        _messages: &mut Vec<Message>,
        _request: TurnRequest,
        _events: tokio::sync::mpsc::Sender<AgentEvent>,
        _cursor: Option<&crate::session::store::SessionCursor>,
        _controls: RunControls,
    ) -> Result<crate::session::recorder::TurnOutcome> {
        panic!("not used")
    }

    fn open_turn(&self) -> Result<Box<dyn crate::agent::turn_session::ExchangeDriver>> {
        panic!("not used")
    }

    fn turn_policy(&self) -> crate::agent::turn_session::TurnPolicy {
        crate::agent::turn_session::TurnPolicy {
            auto_continue: lofi_types::AutoContinuePolicy::default(),
            retry: crate::retry::RetryPolicy::default(),
            hard_cap: None,
        }
    }

    async fn complete(
        &self,
        _request: crate::backend::CompletionRequest,
    ) -> Result<crate::backend::Completion> {
        tokio::time::sleep(self.delay).await;
        Ok(crate::backend::Completion {
            text: self.reply.to_string(),
            usage: Usage::default(),
            cost: None,
        })
    }
}

/// Long enough that no reply in these tests reaches it.
const EVALUATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

async fn evaluate(reply: &'static str, hints: lofi_code::AutoModeHints) -> AutoModeOutcome {
    let evaluator: crate::backend::BackendRef = Arc::new(ReplyBackend {
        reply,
        delay: std::time::Duration::ZERO,
    });
    evaluate_command(
        &evaluator,
        "jj abandon",
        std::path::Path::new("/repo"),
        None,
        EVALUATION_TIMEOUT,
        &[],
        &hints,
    )
    .await
}

#[tokio::test]
async fn low_risk_runs_the_command() {
    let outcome = evaluate(
        r#"{"risk":"low","reason":"drops an uncommitted change"}"#,
        lofi_code::AutoModeHints::default(),
    )
    .await;
    assert_eq!(
        outcome,
        AutoModeOutcome::Allow {
            reason: "drops an uncommitted change".into()
        }
    );
}

#[tokio::test]
async fn confirm_risk_defers_to_the_user() {
    let outcome = evaluate(
        r#"{"risk":"confirm","reason":"remote state"}"#,
        lofi_code::AutoModeHints::default(),
    )
    .await;
    assert_eq!(
        outcome,
        AutoModeOutcome::Ask {
            reason: "remote state".into()
        }
    );
}

#[tokio::test]
async fn block_risk_still_defers_to_the_user() {
    let outcome = evaluate(
        r#"{"risk":"block","reason":"exfiltrates secrets"}"#,
        lofi_code::AutoModeHints::default(),
    )
    .await;
    assert_eq!(
        outcome,
        AutoModeOutcome::HighRisk {
            reason: "exfiltrates secrets".into()
        }
    );
}

#[tokio::test]
async fn unparsable_reply_falls_back_to_the_dialog() {
    let outcome = evaluate("I cannot decide", lofi_code::AutoModeHints::default()).await;
    assert!(matches!(outcome, AutoModeOutcome::Failed { .. }));
}

fn text_round(text: &'static str) -> Vec<StreamingEvent> {
    vec![
        StreamingEvent::TextDelta(text.to_string()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ]
}

async fn run_prompt(agent: &Agent, messages: &mut Vec<Message>, prompt: &str) {
    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    agent
        .run_turn(
            messages,
            TurnRequest::prompt(prompt.to_string(), PromptKind::User),
            tx,
            None,
            RunControls::default(),
        )
        .await
        .unwrap();
}

fn stored_hints(agent: &Agent) -> lofi_code::AutoModeHints {
    agent.auto_mode_hints().lock().unwrap().clone()
}

#[test]
fn a_child_gets_its_own_context_and_no_parent_evaluator() {
    let dir = tempdir().unwrap();
    let mut agent = agent_with(Vec::new(), dir.path());
    let parent_hints = agent.auto_mode_hints().clone();
    agent = agent.with_auto_mode(Arc::new(|_| {
        Box::pin(std::future::ready(lofi_code::AutoModeOutcome::Allow {
            reason: String::new(),
        }))
    }));

    let support = crate::agent::subagent::SubagentSupport::new(
        crate::models::ModelRegistry::default(),
        lofi_types::Config::default(),
        dir.path().to_path_buf(),
    );
    let mut executor = agent.tool_executor();
    crate::agent::subagent::isolate_auto_mode(&mut executor, &support);
    agent = agent.with_tool_executor(executor);

    assert!(!Arc::ptr_eq(&parent_hints, agent.auto_mode_hints()));
    assert!(
        !agent.has_auto_mode(),
        "the child must not inherit the evaluator"
    );
}

#[test]
fn a_child_inherits_the_user_request_of_the_parent_session() {
    let dir = tempdir().unwrap();
    let agent = agent_with(Vec::new(), dir.path());
    let backend: &dyn crate::backend::Backend = &agent;
    let support = crate::agent::subagent::SubagentSupport::new(
        crate::models::ModelRegistry::default(),
        lofi_types::Config::default(),
        dir.path().to_path_buf(),
    );

    // What the parent's turn loop stores for the current request.
    *agent.auto_mode_hints().lock().unwrap() = lofi_code::AutoModeHints {
        prompt: Some("ship the 1.2 release".to_string()),
        ..lofi_code::AutoModeHints::default()
    };
    let executor = crate::agent::subagent::child_executor(
        backend,
        &support,
        crate::agent::subagent::inherited_request(backend),
    );
    let child_hints = executor.auto_mode_hints.lock().unwrap().clone();
    assert_eq!(
        child_hints.root_request.as_deref(),
        Some("ship the 1.2 release")
    );

    // A nested child inherits the root request, not its parent's task.
    *agent.auto_mode_hints().lock().unwrap() = lofi_code::AutoModeHints {
        root_request: Some("ship the 1.2 release".to_string()),
        parent_task: Some("push the tag".to_string()),
        ..lofi_code::AutoModeHints::default()
    };
    assert_eq!(
        crate::agent::subagent::inherited_request(backend).as_deref(),
        Some("ship the 1.2 release")
    );
}

#[tokio::test]
async fn a_child_turn_keeps_the_inherited_request() {
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("done")], dir.path());
    let support = crate::agent::subagent::SubagentSupport::new(
        crate::models::ModelRegistry::default(),
        lofi_types::Config::default(),
        dir.path().to_path_buf(),
    );
    let backend: &dyn crate::backend::Backend = &agent;
    let executor = crate::agent::subagent::child_executor(
        backend,
        &support,
        Some("ship the 1.2 release".to_string()),
    );
    let agent = agent.with_tool_executor(executor);
    let mut messages = Vec::new();

    let (tx, _rx) = tokio::sync::mpsc::channel(64);
    agent
        .run_turn(
            &mut messages,
            TurnRequest::prompt("push the tag".to_string(), PromptKind::Task),
            tx,
            None,
            RunControls::default(),
        )
        .await
        .unwrap();

    let hints = stored_hints(&agent);
    assert_eq!(hints.parent_task.as_deref(), Some("push the tag"));
    assert_eq!(hints.prompt, None);
    assert_eq!(
        hints.root_request.as_deref(),
        Some("ship the 1.2 release"),
        "the inherited request must survive the turn's derivation"
    );
}

#[tokio::test]
async fn run_turn_stores_the_request_for_the_evaluator() {
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("done")], dir.path());
    let mut messages = Vec::new();

    run_prompt(&agent, &mut messages, "abandon the last change").await;

    let hints = stored_hints(&agent);
    assert_eq!(hints.prompt.as_deref(), Some("abandon the last change"));
    assert_eq!(hints.prior_prompt, None);
}

#[tokio::test]
async fn terse_follow_up_turn_carries_the_earlier_request_and_reply() {
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![text_round("Drop the last change?"), text_round("done")],
        dir.path(),
    );
    let mut messages = Vec::new();

    run_prompt(&agent, &mut messages, "drop the last change").await;
    run_prompt(&agent, &mut messages, "yes").await;

    let hints = stored_hints(&agent);
    assert_eq!(hints.prompt.as_deref(), Some("yes"));
    assert_eq!(hints.prior_prompt.as_deref(), Some("drop the last change"));
    assert_eq!(hints.prior_reply.as_deref(), Some("Drop the last change?"));
}

#[tokio::test]
async fn the_configured_timeout_reaches_the_evaluator() {
    let evaluator: crate::backend::BackendRef = Arc::new(ReplyBackend {
        reply: r#"{"risk":"low","reason":"too slow"}"#,
        delay: std::time::Duration::from_secs(30),
    });
    let cfg = lofi_types::AutoModeConfig {
        enable: true,
        model: Some("test/reply".into()),
        provider: None,
        decision_model: None,
        max_tokens: None,
        timeout_ms: Some(20),
    };
    let hints = crate::agent::auto_mode::hint_handle();
    let evaluate = crate::agent::auto_mode::build_auto_mode(
        evaluator,
        &cfg,
        std::path::Path::new("/repo"),
        None,
        &hints,
    );

    assert_eq!(
        evaluate("jj abandon".to_string()).await,
        AutoModeOutcome::Failed {
            reason: "evaluation timed out after 20 ms".into()
        }
    );
}

/// A decision endpoint that answers from a canned response and records the
/// commands it saw, so tests can tell an unanswered command from a refused
/// one.
struct FakeDecision {
    answers: std::collections::BTreeMap<String, lofi_types::DecisionAnswer>,
    delay: std::time::Duration,
    calls: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl lofi_providers::Decision for FakeDecision {
    async fn decide(
        &self,
        request: &lofi_providers::DecisionRequest,
    ) -> lofi_error::Result<lofi_providers::DecisionResponse> {
        self.calls
            .lock()
            .unwrap()
            .push(request.state["command"].as_str().unwrap().to_string());
        tokio::time::sleep(self.delay).await;
        Ok(lofi_providers::DecisionResponse {
            model: "fake".to_string(),
            answers: self.answers.clone(),
            usage: lofi_providers::DecisionUsage {
                input_tokens: 100,
                output_tokens: 0,
            },
        })
    }
}

fn choice_answer(
    choice: &str,
    probabilities: &[(&str, f64)],
    confidence: Option<f64>,
) -> lofi_types::DecisionAnswer {
    lofi_types::DecisionAnswer::Choice {
        choice: choice.to_string(),
        probabilities: probabilities
            .iter()
            .map(|(k, v)| ((*k).to_string(), *v))
            .collect(),
        confidence,
    }
}

fn noul_answer(p: f64) -> lofi_types::DecisionAnswer {
    lofi_types::DecisionAnswer::Noul { noul: p }
}

fn decision_gate(
    fake: FakeDecision,
    timeout: std::time::Duration,
) -> (lofi_code::AutoModeFn, std::sync::Arc<FakeDecision>) {
    let fake = std::sync::Arc::new(fake);
    let gate = crate::agent::auto_mode::build_decision_auto_mode(
        fake.clone(),
        "jev-latest",
        timeout,
        std::path::Path::new("/repo"),
        None,
        &crate::agent::auto_mode::hint_handle(),
    );
    (gate, fake)
}

#[tokio::test]
async fn the_decision_gate_allows_a_safe_command() {
    let mut answers = std::collections::BTreeMap::new();
    answers.insert(
        "scope".to_string(),
        choice_answer("workspace", &[("workspace", 1.0)], Some(0.9)),
    );
    answers.insert("destructive".to_string(), noul_answer(0.02));
    answers.insert("exposure".to_string(), noul_answer(0.02));
    answers.insert("authorized".to_string(), noul_answer(0.2));
    answers.insert("recognized".to_string(), noul_answer(0.95));
    let (gate, _) = decision_gate(
        FakeDecision {
            answers,
            delay: std::time::Duration::ZERO,
            calls: std::sync::Mutex::new(Vec::new()),
        },
        std::time::Duration::from_secs(5),
    );
    assert!(matches!(
        gate("cargo test".to_string()).await,
        AutoModeOutcome::Allow { .. }
    ));
}

#[tokio::test]
async fn the_decision_gate_asks_on_a_destructive_command() {
    let mut answers = std::collections::BTreeMap::new();
    answers.insert(
        "scope".to_string(),
        choice_answer("workspace", &[("workspace", 1.0)], Some(0.9)),
    );
    answers.insert("destructive".to_string(), noul_answer(0.95));
    answers.insert("exposure".to_string(), noul_answer(0.05));
    answers.insert("authorized".to_string(), noul_answer(0.1));
    answers.insert("recognized".to_string(), noul_answer(0.95));
    let (gate, _) = decision_gate(
        FakeDecision {
            answers,
            delay: std::time::Duration::ZERO,
            calls: std::sync::Mutex::new(Vec::new()),
        },
        std::time::Duration::from_secs(5),
    );
    assert!(matches!(
        gate("rm -rf build".to_string()).await,
        AutoModeOutcome::Ask { .. }
    ));
}

#[tokio::test]
async fn the_decision_gate_falls_back_to_the_prompt_on_timeout() {
    let (gate, _) = decision_gate(
        FakeDecision {
            answers: std::collections::BTreeMap::new(),
            delay: std::time::Duration::from_secs(30),
            calls: std::sync::Mutex::new(Vec::new()),
        },
        std::time::Duration::from_millis(20),
    );
    assert_eq!(
        gate("jj abandon".to_string()).await,
        AutoModeOutcome::Failed {
            reason: "decision call timed out after 20 ms".into()
        }
    );
}

/// A decision endpoint that always fails, so a transport error can be
/// told apart from an ask.
struct FailingDecision;

#[async_trait]
impl lofi_providers::Decision for FailingDecision {
    async fn decide(
        &self,
        _request: &lofi_providers::DecisionRequest,
    ) -> lofi_error::Result<lofi_providers::DecisionResponse> {
        Err(lofi_error::Error::Provider("unreachable".to_string()))
    }
}

/// A gate that answers from a fixed queue, so a two-stage composition
/// can be tested without a backend.
fn scripted_gate(
    outcomes: Vec<AutoModeOutcome>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) -> lofi_code::AutoModeFn {
    let outcomes = std::sync::Arc::new(std::sync::Mutex::new(outcomes));
    std::sync::Arc::new(move |command: String| {
        let outcomes = outcomes.clone();
        let seen = seen.clone();
        Box::pin(async move {
            seen.lock().unwrap().push(command);
            outcomes.lock().unwrap().remove(0)
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
    })
}

#[tokio::test]
async fn the_chat_stage_sees_only_what_the_decision_gate_refuses() {
    let decision_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let chat_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let staged = crate::agent::auto_mode::chat_stage_after_decision(
        scripted_gate(
            vec![
                AutoModeOutcome::Allow {
                    reason: "scope=workspace".into(),
                },
                AutoModeOutcome::Ask {
                    reason: "scope=read_only known=0.35".into(),
                },
            ],
            decision_seen.clone(),
        ),
        scripted_gate(
            vec![AutoModeOutcome::Ask {
                reason: "not reached".into(),
            }],
            chat_seen.clone(),
        ),
    );
    assert!(matches!(
        staged("cargo test".to_string()).await,
        AutoModeOutcome::Allow { .. }
    ));
    assert_eq!(decision_seen.lock().unwrap().len(), 1);
    assert!(chat_seen.lock().unwrap().is_empty());

    // The refused command reaches the chat stage, whose verdict stands.
    match staged("exa search rust release notes".to_string()).await {
        AutoModeOutcome::Ask { reason } => {
            assert!(reason.contains("known=0.35"), "{reason}");
        }
        other => panic!("the chat verdict must stand: {other:?}"),
    }
    assert_eq!(chat_seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_chat_stage_allows_what_the_decision_gate_asked_on() {
    let staged = crate::agent::auto_mode::chat_stage_after_decision(
        scripted_gate(
            vec![AutoModeOutcome::Ask {
                reason: "scope=read_only known=0.40".into(),
            }],
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
        scripted_gate(
            vec![AutoModeOutcome::Allow {
                reason: "a public search with no payload".into(),
            }],
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
    );
    assert!(matches!(
        staged("exa search rust release notes".to_string()).await,
        AutoModeOutcome::Allow { .. }
    ));
}

#[tokio::test]
async fn the_chat_stage_never_runs_after_a_decision_allow() {
    let chat_seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let staged = crate::agent::auto_mode::chat_stage_after_decision(
        scripted_gate(
            vec![AutoModeOutcome::Allow {
                reason: "scope=read_only".into(),
            }],
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        ),
        scripted_gate(vec![], chat_seen.clone()),
    );
    assert!(matches!(
        staged("uptime".to_string()).await,
        AutoModeOutcome::Allow { .. }
    ));
    assert!(chat_seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_transport_error_fails_the_gate_closed() {
    let gate = crate::agent::auto_mode::build_decision_auto_mode(
        std::sync::Arc::new(FailingDecision),
        "jev-latest",
        std::time::Duration::from_secs(5),
        std::path::Path::new("/repo"),
        None,
        &crate::agent::auto_mode::hint_handle(),
    );
    match gate("jj abandon".to_string()).await {
        AutoModeOutcome::Failed { reason } => {
            assert!(reason.contains("unreachable"), "{reason}");
        }
        other => panic!("a transport error must fail closed: {other:?}"),
    }
}

#[tokio::test]
async fn an_oversized_command_asks_without_a_call() {
    let (gate, fake) = decision_gate(
        FakeDecision {
            answers: std::collections::BTreeMap::new(),
            delay: std::time::Duration::ZERO,
            calls: std::sync::Mutex::new(Vec::new()),
        },
        std::time::Duration::from_secs(5),
    );
    let long = "x".repeat(8 * 1024 + 1);
    let outcome = gate(long).await;
    assert!(matches!(outcome, AutoModeOutcome::Ask { .. }));
    assert!(fake.calls.lock().unwrap().is_empty());
}

fn auto_mode_config(model: &str) -> lofi_types::Config {
    let mut providers = IndexMap::new();
    providers.insert(
        "chat".to_string(),
        lofi_types::ProviderConfig {
            api_type: Some(Api::OpenAiCompletions),
            base_url: Some("https://chat.example.com".to_string()),
            api_key: Some("sk-test".to_string()),
            models: models(&["haiku"]),
            ..provider_default()
        },
    );
    providers.insert(
        "typesafe".to_string(),
        lofi_types::ProviderConfig {
            api_type: Some(Api::SystemOne),
            base_url: Some("https://typesafe.example.com".to_string()),
            api_key: Some("sk-test".to_string()),
            models: models(&["jev-latest"]),
            ..provider_default()
        },
    );
    let mut config = lofi_types::Config {
        providers,
        ..lofi_types::Config::default()
    };
    config.shell_policy.auto_mode = Some(lofi_types::AutoModeConfig {
        enable: true,
        model: Some(model.to_string()),
        provider: None,
        decision_model: None,
        max_tokens: None,
        timeout_ms: None,
    });
    config
}

/// A config whose decision gate is `decision_model`, with model as the
/// chat stage when given.
fn decision_config(decision_model: &str, model: Option<&str>) -> lofi_types::Config {
    let mut config = auto_mode_config(model.unwrap_or("chat/haiku"));
    let auto = config.shell_policy.auto_mode.as_mut().unwrap();
    auto.model = model.map(str::to_string);
    auto.decision_model = Some(decision_model.to_string());
    config
}

fn models(ids: &[&str]) -> IndexMap<String, lofi_types::ModelConfig> {
    ids.iter()
        .map(|id| ((*id).to_string(), serde_json::from_str("{}").unwrap()))
        .collect()
}

fn provider_default() -> lofi_types::ProviderConfig {
    serde_json::from_str("{}").unwrap()
}

async fn configured(model: &str) -> lofi_error::Result<ToolExecutor> {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    crate::agent::auto_mode::configure(executor, &auto_mode_config(model), &[]).await
}

async fn configured_decision(
    decision_model: &str,
    model: Option<&str>,
) -> lofi_error::Result<ToolExecutor> {
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    crate::agent::auto_mode::configure(executor, &decision_config(decision_model, model), &[]).await
}

#[tokio::test]
async fn a_level_on_a_decision_model_is_a_config_error() {
    match configured_decision("typesafe/jev-latest:high", None).await {
        Err(error) => assert!(
            error
                .to_string()
                .contains("no thinking level or service tier"),
            "unexpected error: {error}"
        ),
        Ok(_) => panic!("a decision model query has no thinking level"),
    }
}

#[tokio::test]
async fn a_max_tokens_on_a_decision_model_is_ignored() {
    let mut config = decision_config("typesafe/jev-latest", None);
    config.shell_policy.auto_mode.as_mut().unwrap().max_tokens = Some(1024);
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    match crate::agent::auto_mode::configure(executor, &config, &[]).await {
        Ok(executor) => assert!(executor.auto_mode.is_some()),
        Err(error) => panic!("a decision model must ignore max_tokens: {error}"),
    }
}

#[tokio::test]
async fn a_decision_model_builds_the_gate() {
    match configured_decision("typesafe/jev-latest", None).await {
        Ok(executor) => assert!(executor.auto_mode.is_some()),
        Err(error) => panic!("a systemone model must open as a decision gate: {error}"),
    }
}

#[tokio::test]
async fn a_chat_model_still_builds_the_chat_gate() {
    match configured("chat/haiku").await {
        Ok(executor) => assert!(executor.auto_mode.is_some()),
        Err(error) => panic!("a chat model must open as a chat evaluator: {error}"),
    }
}

#[tokio::test]
async fn a_decision_model_named_as_the_chat_stage_is_a_config_error() {
    let config = auto_mode_config("typesafe/jev-latest");
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    match crate::agent::auto_mode::configure(executor, &config, &[]).await {
        Err(error) => assert!(
            error.to_string().contains("belongs in decision_model"),
            "unexpected error: {error}"
        ),
        Ok(_) => panic!("a decision model cannot be the chat stage"),
    }
}

#[tokio::test]
async fn a_configured_stage_without_either_model_is_a_config_error() {
    let mut config = auto_mode_config("chat/haiku");
    config.shell_policy.auto_mode.as_mut().unwrap().model = None;
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    match crate::agent::auto_mode::configure(executor, &config, &[]).await {
        Err(error) => assert!(
            error.to_string().contains("set model or decision_model"),
            "unexpected error: {error}"
        ),
        Ok(_) => panic!("auto mode needs one of the two models"),
    }
}

#[tokio::test]
async fn a_chat_stage_builds_on_the_decision_gate() {
    let config = decision_config("typesafe/jev-latest", Some("chat/haiku"));
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    match crate::agent::auto_mode::configure(executor, &config, &[]).await {
        Ok(executor) => assert!(executor.auto_mode.is_some()),
        Err(error) => panic!("a chat stage must attach to a decision gate: {error}"),
    }
}

#[tokio::test]
async fn a_chat_stage_without_credentials_is_a_config_error() {
    let mut config = decision_config("typesafe/jev-latest", Some("chat/haiku"));
    config.providers.get_mut("chat").unwrap().api_key = None;
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    match crate::agent::auto_mode::configure(executor, &config, &[]).await {
        Err(error) => assert!(
            error.to_string().contains("no API key"),
            "unexpected error: {error}"
        ),
        Ok(_) => panic!("a chat stage without credentials must not start"),
    }
}

#[tokio::test]
async fn a_decision_provider_without_credentials_is_a_config_error() {
    let mut config = decision_config("typesafe/jev-latest", None);
    config.providers.get_mut("typesafe").unwrap().api_key = None;
    let dir = tempfile::tempdir().unwrap();
    let executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    match crate::agent::auto_mode::configure(executor, &config, &[]).await {
        Err(error) => assert!(
            error.to_string().contains("no api key"),
            "unexpected error: {error}"
        ),
        Ok(_) => panic!("a decision gate without credentials must not start"),
    }
}
