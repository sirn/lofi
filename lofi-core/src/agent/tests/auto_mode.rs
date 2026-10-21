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
    let hints = derive_hints(&[], Some("delete the build directory"));
    assert_eq!(hints.prompt.as_deref(), Some("delete the build directory"));
    assert_eq!(hints.prior_prompt, None);
    assert_eq!(hints.prior_reply, None);
}

#[test]
fn hints_cap_the_request_to_its_tail() {
    let long = format!("{} now", "a".repeat(400));
    let hints = derive_hints(&[], Some(&long));
    let prompt = hints.prompt.unwrap();
    assert!(prompt.starts_with("[…truncated] "));
    assert!(prompt.ends_with("now"));
}

#[test]
fn terse_request_carries_the_earlier_request_and_the_agent_question() {
    let history = [
        typed("abandon the last change"),
        assistant("Cancel the \"tidy\" change?"),
    ];
    let hints = derive_hints(&history, Some("yes"));
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
    let hints = derive_hints(&history, Some("do that"));
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
    let hints = derive_hints(&history, Some("yes"));
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
struct ReplyBackend(&'static str);

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
        Ok(crate::backend::Completion {
            text: self.0.to_string(),
            usage: Usage::default(),
            cost: None,
        })
    }
}

async fn evaluate(reply: &'static str, hints: lofi_code::AutoModeHints) -> AutoModeOutcome {
    let evaluator: crate::backend::BackendRef = Arc::new(ReplyBackend(reply));
    evaluate_command(
        &evaluator,
        "jj abandon",
        std::path::Path::new("/repo"),
        None,
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
