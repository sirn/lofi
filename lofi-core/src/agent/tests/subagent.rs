#![allow(clippy::await_holding_lock)]
// The lock serializes tests against the process-global LOFI_STATE_HOME, not
// against shared runtime state, so holding it across awaits is the point.

use super::*;
use lofi_code::SubagentOp;
use serde_json::json;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::state::STATE_ENV_LOCK.lock().unwrap()
}

fn support(cwd: &std::path::Path) -> SubagentSupport {
    SubagentSupport::new(
        crate::models::ModelRegistry::default(),
        lofi_types::Config::default(),
        cwd.to_path_buf(),
    )
}

// Point LOFI_STATE_HOME at a temp dir so spawning tests never write to the
// real state directory; restores the previous value on drop.
struct StateEnv {
    prev: Option<std::ffi::OsString>,
    _dir: tempfile::TempDir,
}

impl StateEnv {
    fn new() -> (Self, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let prev = std::env::var_os("LOFI_STATE_HOME");
        std::env::set_var("LOFI_STATE_HOME", dir.path());
        let path = dir.path().to_path_buf();
        (Self { prev, _dir: dir }, path)
    }
}

impl Drop for StateEnv {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => std::env::set_var("LOFI_STATE_HOME", v),
            None => std::env::remove_var("LOFI_STATE_HOME"),
        }
    }
}

fn text_round(text: &str) -> Vec<StreamingEvent> {
    vec![
        StreamingEvent::TextDelta(text.to_string()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ]
}

#[tokio::test]
async fn spawn_returns_id_status_and_result() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("sub result")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "do it" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("sub result"), "{out}");
    assert_eq!(out["id"].as_str().unwrap().len(), 8);
}

#[tokio::test]
async fn hook_is_none_without_support() {
    let _env = env_lock();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![], dir.path());
    assert!(agent.subagent_hook().is_none());
}

#[tokio::test]
async fn resume_appends_to_child_history() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let provider = Arc::new(MockProvider::recording(vec![
        text_round("first"),
        text_round("second"),
    ]));
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(SubagentOp::Run, json!({ "id": id, "prompt": "b" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("second"), "{out}");

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let roles: Vec<_> = requests[1]
        .iter()
        .map(|m| (m.role, first_text(m)))
        .collect();
    let a = roles
        .iter()
        .position(|(r, t)| *r == Role::User && t == "a")
        .unwrap();
    assert_eq!(roles[a + 1], (Role::Assistant, "first".to_string()));
    assert_eq!(roles[a + 2], (Role::User, "b".to_string()));
}

#[tokio::test]
async fn resume_rejects_model_override() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("first")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(
        SubagentOp::Run,
        json!({ "id": id, "prompt": "b", "model": "p/m" }),
    )
    .await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("only apply when spawning"),
        "{out}"
    );
}

#[tokio::test]
async fn unknown_id_reports_failed() {
    let _env = env_lock();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "id": "deadbeef", "prompt": "b" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("unknown subagent id"),
        "{out}"
    );
    assert_eq!(out["id"], json!("deadbeef"));
}

#[tokio::test]
async fn output_limit_truncates_result() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("abcdefghijklmnopqrstuvwxyz")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "a", "output_limit": 5 })).await;
    assert_eq!(out["result"], json!("abcde"), "{out}");
}

#[tokio::test]
async fn spawn_persists_a_transcript() {
    let _env = env_lock();
    let (_state, state_path) = StateEnv::new();

    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("sub result")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();
    let out = hook(SubagentOp::Run, json!({ "prompt": "do it" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");

    let files = find_jsonl(&state_path);
    assert!(
        !files.is_empty(),
        "no transcript under {}",
        state_path.display()
    );
}

#[tokio::test]
async fn spawn_without_persistence_writes_no_transcript() {
    let _env = env_lock();
    let (_state, state_path) = StateEnv::new();

    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("sub result")], dir.path())
        .with_subagent_support(support(dir.path()).with_persistence(false));
    let hook = agent.subagent_hook().unwrap();
    let out = hook(SubagentOp::Run, json!({ "prompt": "do it" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert!(
        find_jsonl(&state_path).is_empty(),
        "transcript written under {}",
        state_path.display()
    );
}

#[tokio::test]
async fn invalid_arg_types_fail_instead_of_defaulting() {
    let _env = env_lock();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("sub result")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "id": 123, "prompt": "a" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("must be a string"),
        "{out}"
    );

    let out = hook(SubagentOp::Run, json!({ "prompt": "a", "timeout_ms": 0 })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("positive integer"),
        "{out}"
    );

    let out = hook(SubagentOp::Run, json!({})).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("prompt is required"),
        "{out}"
    );

    let out = hook(SubagentOp::Run, json!({ "prompt": "   " })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("prompt is required"),
        "{out}"
    );

    let out = hook(SubagentOp::Run, json!({ "id": "", "prompt": "a" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("non-empty string"),
        "{out}"
    );
}

#[tokio::test]
async fn output_limit_counts_bytes_at_char_boundary() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(
        vec![text_round("ab\u{1f600}cd"), text_round("ab\u{1f600}cd")],
        dir.path(),
    )
    .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    // The emoji is 4 bytes: limit 4 would split it, so it drops to "ab".
    let out = hook(SubagentOp::Run, json!({ "prompt": "a", "output_limit": 4 })).await;
    assert_eq!(out["result"].as_str().unwrap(), "ab", "{out}");
    // Limit 6 fits the emoji exactly.
    let out = hook(SubagentOp::Run, json!({ "prompt": "b", "output_limit": 6 })).await;
    assert_eq!(out["result"].as_str().unwrap(), "ab\u{1f600}", "{out}");
}

// The answer depends on the request content, not on a call counter, so the
// tests stay deterministic no matter when the stop fires: a prompt "a" asks
// for a tool call, the follow-up after a tool result pends forever (the run
// can be stopped mid-turn), and anything else completes with plain text.
struct TimeoutRollbackProvider {
    requests: std::sync::Mutex<Vec<Vec<Message>>>,
    tool_round: Vec<StreamingEvent>,
    text_round: Vec<StreamingEvent>,
}

#[async_trait]
impl Provider for TimeoutRollbackProvider {
    async fn stream(
        &self,
        _model: &Model,
        messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        self.requests.lock().unwrap().push(messages.to_vec());
        let last = messages.last();
        let round = match last {
            Some(m) if m.role == Role::Tool => std::future::pending().await,
            Some(m) if m.role == Role::User && first_text(m) == "a" => self.tool_round.clone(),
            _ => self.text_round.clone(),
        };
        Ok(Box::pin(stream::iter(round.into_iter().map(Ok))))
    }
}

#[tokio::test]
async fn timeout_rolls_back_the_unfinished_turn() {
    let _env = env_lock();
    let (_state, state_path) = StateEnv::new();
    let dir = tempdir().unwrap();
    let provider = Arc::new(TimeoutRollbackProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_round: exec_round("return 1"),
        text_round: text_round("resumed"),
    });
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    let hook = agent.subagent_hook().unwrap();

    // The model round after the tool call pends, so the run times out
    // mid-turn. A slow tool round is fine too: the resume works either way.
    let out = hook(SubagentOp::Run, json!({ "prompt": "a", "timeout_ms": 100 })).await;
    assert_eq!(out["status"], json!("timed_out"), "{out}");
    let id = out["id"].as_str().unwrap().to_string();

    let out = hook(
        SubagentOp::Run,
        json!({ "id": id, "prompt": "b", "timeout_ms": 5000 }),
    )
    .await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("resumed"), "{out}");

    // In-memory history: the resumed turn sees neither "a" nor the tool call.
    let requests = provider.requests.lock().unwrap();
    let roles: Vec<_> = requests
        .last()
        .unwrap()
        .iter()
        .map(|m| (m.role, first_text(m)))
        .collect();
    assert_eq!(
        roles,
        vec![
            (Role::System, "sys".to_string()),
            (Role::User, "b".to_string())
        ],
        "{roles:?}"
    );
    drop(requests);

    // Transcript: the selected lineage holds only the resumed turn.
    let files = find_jsonl(&state_path);
    assert_eq!(files.len(), 1, "{files:?}");
    let (_cursor, snapshot) =
        crate::session::store::SessionCursor::open_snapshot(files[0].clone()).unwrap();
    let kinds: Vec<_> = snapshot.index.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            crate::session::store::IndexKind::SystemMessage,
            // The timed-out run's settle marker, at the rollback point.
            crate::session::store::IndexKind::RunSettled,
            crate::session::store::IndexKind::UserPrompt,
            crate::session::store::IndexKind::AssistantMessage,
            crate::session::store::IndexKind::TurnEnd,
            // The resumed run's settle marker ends the lineage.
            crate::session::store::IndexKind::RunSettled,
        ],
        "{kinds:?}"
    );
}

struct PendingProvider;

#[async_trait]
impl Provider for PendingProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        std::future::pending().await
    }
}

// Poll the registry until an entry reaches `want`, returning its id.
async fn wait_for_status(registry: &SubagentRegistry, want: &str) -> String {
    for _ in 0..1000 {
        let hit = registry
            .ids()
            .into_iter()
            .find(|id| registry.status_of(id).as_deref() == Some(want));
        if let Some(id) = hit {
            return id;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no subagent reached status {want}");
}

#[tokio::test]
async fn resume_while_running_is_rejected() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(PendingProvider);
    let registry = agent.subagents();
    let hook = agent.subagent_hook().unwrap();

    let run = tokio::spawn({
        let hook = hook.clone();
        async move { hook(SubagentOp::Run, json!({ "prompt": "a" })).await }
    });
    let id = wait_for_status(&registry, "running").await;
    let out = hook(SubagentOp::Run, json!({ "id": id, "prompt": "b" })).await;
    // A busy id reports `running`, not `failed`: the run is still going and
    // the id stays valid for a later resume.
    assert_eq!(out["status"], json!("running"), "{out}");
    assert_eq!(out["ok"], json!(false), "{out}");
    assert!(out["error"].as_str().unwrap().contains("running"), "{out}");
    assert_eq!(out["id"], json!(id), "{out}");
    run.abort();
}

#[tokio::test]
async fn dropped_hook_future_cancels_and_stays_resumable() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let provider = Arc::new(TimeoutRollbackProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_round: exec_round("return 1"),
        text_round: text_round("resumed"),
    });
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider;
    let registry = agent.subagents();
    let hook = agent.subagent_hook().unwrap();

    // Dropping the hook future (parent turn aborted) must cancel the child:
    // the entry turns terminal and the same id stays resumable.
    let run = tokio::spawn({
        let hook = hook.clone();
        async move { hook(SubagentOp::Run, json!({ "prompt": "a" })).await }
    });
    let id = wait_for_status(&registry, "running").await;
    run.abort();
    let _ = run.await;
    let cancelled = wait_for_status(&registry, "cancelled").await;
    assert_eq!(cancelled, id);

    let out = hook(
        SubagentOp::Run,
        json!({ "id": id, "prompt": "b", "timeout_ms": 5000 }),
    )
    .await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("resumed"), "{out}");
}

#[tokio::test]
async fn registry_evicts_terminal_entries_at_capacity() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let rounds: Vec<_> = (0..67).map(|i| text_round(&format!("r{i}"))).collect();
    let agent = agent_with(rounds, dir.path()).with_subagent_support(support(dir.path()));
    let registry = agent.subagents();
    let hook = agent.subagent_hook().unwrap();

    let mut ids = Vec::new();
    for i in 0..64 {
        let out = hook(SubagentOp::Run, json!({ "prompt": format!("p{i}") })).await;
        assert_eq!(out["status"], json!("completed"), "{out}");
        ids.push(out["id"].as_str().unwrap().to_string());
    }
    assert_eq!(registry.ids().len(), 64);
    // The 65th spawn evicts the least recently used finished entry (p0)
    // instead of failing or dropping a run the guest may still resume.
    let out = hook(SubagentOp::Run, json!({ "prompt": "overflow" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(registry.ids().len(), 64);
    let overflow = out["id"].as_str().unwrap().to_string();

    let out = hook(SubagentOp::Run, json!({ "id": ids[0], "prompt": "again" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("unknown subagent id"),
        "{out}"
    );
    let out = hook(SubagentOp::Run, json!({ "id": ids[63], "prompt": "again" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("r65"), "{out}");
    let out = hook(
        SubagentOp::Run,
        json!({ "id": overflow, "prompt": "again" }),
    )
    .await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("r66"), "{out}");
}

#[tokio::test]
async fn running_entries_hit_the_concurrency_cap() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(PendingProvider);
    let registry = agent.subagents();
    let hook = agent.subagent_hook().unwrap();

    let mut runs = Vec::new();
    for _ in 0..8 {
        runs.push(tokio::spawn({
            let hook = hook.clone();
            async move { hook(SubagentOp::Run, json!({ "prompt": "a" })).await }
        }));
    }
    for _ in 0..1000 {
        if registry.ids().len() == 8 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(registry.ids().len(), 8, "entries never filled");

    let out = hook(SubagentOp::Run, json!({ "prompt": "overflow" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("concurrency cap"),
        "{out}"
    );

    for run in runs {
        run.abort();
    }
}

#[tokio::test]
async fn spawn_applies_the_system_override() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let provider = Arc::new(TimeoutRollbackProvider {
        requests: std::sync::Mutex::new(Vec::new()),
        tool_round: exec_round("return 1"),
        text_round: text_round("done"),
    });
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    let hook = agent.subagent_hook().unwrap();

    let out = hook(
        SubagentOp::Run,
        json!({ "prompt": "b", "system": "custom sys" }),
    )
    .await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    // Scope the guard: it must not live across the second spawn, whose
    // provider call takes the same lock.
    {
        let requests = provider.requests.lock().unwrap();
        let first = &requests[0][0];
        assert_eq!(first.role, Role::System, "{first:?}");
        assert_eq!(first_text(first), "custom sys");
    }

    // Without an override the child keeps the session system prompt.
    let out = hook(SubagentOp::Run, json!({ "prompt": "b" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let requests = provider.requests.lock().unwrap();
    assert_eq!(first_text(&requests[1][0]), "sys");
}

#[tokio::test]
async fn completed_run_without_text_is_a_failure() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round(""), text_round("fine")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    // An empty final answer must not surface as a successful empty result.
    let out = hook(SubagentOp::Run, json!({ "prompt": "a" })).await;
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("without a text result"),
        "{out}"
    );

    // A failed entry stays resumable.
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(SubagentOp::Run, json!({ "id": id, "prompt": "b" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("fine"), "{out}");
}

#[tokio::test]
async fn thinking_override_requires_session_model_support() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent =
        agent_with(vec![text_round("ok")], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    // The mock session model declares no thinking levels.
    let out = hook(
        SubagentOp::Run,
        json!({ "prompt": "a", "thinking": "high" }),
    )
    .await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("does not support thinking"),
        "{out}"
    );
    // `off` is always allowed.
    let out = hook(SubagentOp::Run, json!({ "prompt": "a", "thinking": "off" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
}

// Observable through the sandbox, not the fields: after `without_subagent`
// a guest that calls `lofi.models()` gets the unavailable status.
#[tokio::test]
async fn without_subagent_makes_models_unavailable_in_the_sandbox() {
    let dir = tempdir().unwrap();
    let mut executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    executor.subagent = Some(Arc::new(
        |_, _| -> std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>> {
            Box::pin(async { json!(null) })
        },
    ));
    executor.models = Some(Arc::new(|| json!([])));
    let executor = executor.without_subagent();
    let context = ToolCallContext {
        events: None,
        session: None,
        recall: None,
        result: None,
        cancel: None,
        on_job_acquired: None,
        record_tool: None,
    };
    let out = executor
        .execute(
            lofi_code::EXEC_TOOL_NAME,
            &json!({ "code": "return lofi.models()" }),
            context,
        )
        .await
        .unwrap();
    let ContentBlock::ToolResult { content, .. } = out else {
        panic!("expected a tool result: {out:?}");
    };
    assert!(content.contains("\"status\":\"unavailable\""), "{content}");
}

fn find_jsonl(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(find_jsonl(&path));
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
    out
}

fn first_text(m: &Message) -> String {
    m.blocks
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn settled_run_spend_folds_into_session_totals() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let child = vec![
        StreamingEvent::TextDelta("sub result".to_string()),
        StreamingEvent::Done {
            usage: Usage {
                input_tokens: 11,
                output_tokens: 7,
                ..Usage::default()
            },
            stop_reason: None,
        },
    ];
    let agent = agent_with(vec![child], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "do it" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let registry = agent.subagents();
    let (cost, usage) = registry.take_unreported().unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 7);
    assert!(cost >= 0.0);
    assert!(registry.take_unreported().is_none(), "spend is taken once");
}

#[tokio::test]
async fn background_run_survives_a_per_turn_runtime() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("bg done")], dir.path())
        .with_subagent_support(support(dir.path()));
    // The TUI's turn loop runs on its own worker runtime that dies when
    // the turn ends; recording it as the host runtime must not win over
    // the construction-time capture.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let turn = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                turn.block_on(async {
                    agent
                        .subagents()
                        .set_host_handle(tokio::runtime::Handle::current());
                });
            })
            .join()
            .unwrap();
    });
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("completed"), "{done}");
    assert_eq!(done["result"], json!("bg done"), "{done}");
}

#[tokio::test]
async fn cache_only_spend_is_still_reported() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    // A cache hit with no billable tokens and no cost still counts: the
    // predicate must not wait for another run to carry it.
    let child = vec![
        StreamingEvent::TextDelta("sub result".to_string()),
        StreamingEvent::Done {
            usage: Usage {
                cache_read_tokens: 500,
                ..Usage::default()
            },
            stop_reason: None,
        },
    ];
    let agent = agent_with(vec![child], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "do it" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let (_, usage) = agent.subagents().take_unreported().unwrap();
    assert_eq!(usage.cache_read_tokens, 500);
}

#[tokio::test]
async fn parent_turn_records_and_reports_subagent_spend() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let tool_round = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".into(),
            name: "exec".into(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".into(),
            delta: r#"{"code":"const r = await lofi.subagent({ prompt: 'x' }); return r.status;"}"#
                .into(),
        },
        StreamingEvent::ToolUseInputComplete {
            id: "t1".into(),
            input: r#"{"code":"const r = await lofi.subagent({ prompt: 'x' }); return r.status;"}"#
                .into(),
        },
        StreamingEvent::ToolUseEnd { id: "t1".into() },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::ToolUse),
        },
    ];
    let child_round = vec![
        StreamingEvent::TextDelta("child answer".into()),
        StreamingEvent::Done {
            usage: Usage {
                input_tokens: 13,
                output_tokens: 5,
                ..Usage::default()
            },
            stop_reason: None,
        },
    ];
    let final_round = vec![
        StreamingEvent::TextDelta("done".into()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: Some(lofi_types::StopReason::EndTurn),
        },
    ];
    let agent = agent_with(vec![tool_round, child_round, final_round], dir.path())
        .with_subagent_support(support(dir.path()));
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n",
    )
    .unwrap();
    let cursor = crate::session::store::SessionCursor::new(path, None);
    let (tx, mut rx) = tokio::sync::mpsc::channel(64);
    let mut messages = Vec::new();
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

    let mut reported = None;
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::SubagentUsage { usage, .. } = ev {
            reported = Some(usage);
        }
    }
    let usage = reported.unwrap();
    assert_eq!(usage.input_tokens, 13);
    assert_eq!(usage.output_tokens, 5);
    let events = cursor.load_tree_events().unwrap();
    assert!(
        events.iter().any(|event| matches!(
            &event.kind,
            SessionEventKind::SubagentUsage { usage, .. }
                if usage.input_tokens == 13 && usage.output_tokens == 5
        )),
        "transcript carries the subagent spend"
    );
}

#[tokio::test]
async fn spawn_returns_running_and_wait_collects_the_outcome() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("bg result")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "do it" })).await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["status"], json!("running"), "{out}");
    let id = out["id"].as_str().unwrap().to_string();

    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["ok"], json!(true), "{done}");
    assert_eq!(done["status"], json!("completed"), "{done}");
    assert_eq!(done["result"], json!("bg result"), "{done}");
}

struct BigUsageProvider;

#[async_trait]
impl Provider for BigUsageProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        Ok(Box::pin(stream::iter([
            Ok(StreamingEvent::TextDelta("done".to_string())),
            Ok(StreamingEvent::Done {
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Usage::default()
                },
                stop_reason: None,
            }),
        ])))
    }
}

#[tokio::test]
async fn token_budget_exceeded_fails_the_run() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(BigUsageProvider);
    let hook = agent.subagent_hook().unwrap();

    let out = hook(
        SubagentOp::Spawn,
        json!({ "prompt": "spend", "token_budget": 10 }),
    )
    .await;
    assert_eq!(out["ok"], json!(true), "{out}");
    let id = out["id"].as_str().unwrap().to_string();

    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("failed"), "{done}");
    assert_eq!(
        done["error"],
        json!("token budget exceeded (150 > 10)"),
        "{done}"
    );
}

#[tokio::test]
async fn token_budget_zero_is_rejected() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "x", "token_budget": 0 })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("token_budget"),
        "{out}"
    );
    let out = hook(
        SubagentOp::Spawn,
        json!({ "prompt": "x", "token_budget": 0 }),
    )
    .await;
    assert_eq!(out["ok"], json!(false), "{out}");
}

#[tokio::test]
async fn read_pages_do_not_repeat_events() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("paged")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "page me" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("completed"), "{done}");

    let page1 = hook(SubagentOp::Read, json!({ "id": id, "limit": 2 })).await;
    let events1 = page1["events"].as_array().unwrap();
    assert_eq!(events1.len(), 2, "{page1}");
    let cursor = page1["cursor"].as_u64().unwrap();
    let page2 = hook(SubagentOp::Read, json!({ "id": id, "cursor": cursor })).await;
    let events2 = page2["events"].as_array().unwrap();
    assert!(!events2.is_empty(), "{page2}");
    let first = events2[0]["seq"].as_u64().unwrap();
    assert!(first >= cursor, "page 2 repeats page 1: {page2}");
    assert_eq!(page2["done"], json!(true), "{page2}");
}

#[tokio::test]
async fn wait_after_settle_returns_the_stored_outcome() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("late waiter")], dir.path())
        .with_subagent_support(support(dir.path()));
    let registry = agent.subagents();
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "quick" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    // Poll status until terminal: no waiter is subscribed when the run
    // settles, so the watch channel must retain the terminal state.
    for _ in 0..1000 {
        let status = hook(SubagentOp::Status, json!({ "id": id })).await;
        if status["status"] != json!("running") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(registry.ids().contains(&id));
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("completed"), "{done}");
    assert_eq!(done["result"], json!("late waiter"), "{done}");
}

#[tokio::test]
async fn resume_is_rejected_at_the_concurrency_cap() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![text_round("settled")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    // One settled run to resume later.
    let out = hook(SubagentOp::Run, json!({ "prompt": "first" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let resume_id = out["id"].as_str().unwrap().to_string();

    // Fill the running set with eight pending runs.
    agent.provider = Arc::new(NeverProvider);
    let registry = agent.subagents();
    let mut running = Vec::new();
    for _ in 0..8 {
        let out = hook(SubagentOp::Spawn, json!({ "prompt": "hang" })).await;
        assert_eq!(out["ok"], json!(true), "{out}");
        running.push(out["id"].as_str().unwrap().to_string());
    }
    assert_eq!(registry.ids().len(), 9);

    let out = hook(
        SubagentOp::Run,
        json!({ "id": resume_id, "prompt": "again" }),
    )
    .await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("concurrency cap"),
        "{out}"
    );

    for id in running {
        hook(SubagentOp::Kill, json!({ "id": id })).await;
    }
}

#[tokio::test]
async fn foreground_run_publishes_no_terminal_notice() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("quiet")], dir.path())
        .with_subagent_support(support(dir.path()));
    let mut notices = agent.subagents().subscribe_notices();
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "quiet" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert!(
        notices.try_recv().is_err(),
        "foreground run published a notice"
    );
}

struct CacheUsageProvider;

#[async_trait]
impl Provider for CacheUsageProvider {
    async fn stream(
        &self,
        _model: &Model,
        _messages: &[Message],
        _tools: &[ToolSchema],
    ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
        Ok(Box::pin(stream::iter([
            Ok(StreamingEvent::TextDelta("done".to_string())),
            Ok(StreamingEvent::Done {
                usage: Usage {
                    cache_read_tokens: 100,
                    ..Usage::default()
                },
                stop_reason: None,
            }),
        ])))
    }
}

#[tokio::test]
async fn token_budget_counts_cache_tokens() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(CacheUsageProvider);
    let hook = agent.subagent_hook().unwrap();

    let out = hook(
        SubagentOp::Run,
        json!({ "prompt": "cache", "token_budget": 10 }),
    )
    .await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("token budget exceeded"),
        "{out}"
    );
}

#[tokio::test]
async fn resume_of_a_background_run_publishes_no_notice() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("first"), text_round("second")], dir.path())
        .with_subagent_support(support(dir.path()));
    let mut notices = agent.subagents().subscribe_notices();
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("completed"), "{done}");
    // The background run published its terminal notice; drain it.
    while notices.try_recv().is_ok() {}

    // Resume is always a foreground op: its outcome goes to the caller, so
    // no second notice may be queued as a parent prompt.
    let out = hook(SubagentOp::Run, json!({ "id": id, "prompt": "b" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("second"), "{out}");
    assert!(
        notices.try_recv().is_err(),
        "foreground resume published a notice"
    );
}

#[tokio::test]
async fn multibyte_prompt_at_the_event_cap_does_not_panic() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent =
        agent_with(vec![text_round("ok")], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    // Byte 8192 lands inside the two-byte character.
    let mut prompt = "x".repeat(8191);
    prompt.push('\u{00e9}');
    prompt.push_str(&"y".repeat(64));
    let out = hook(SubagentOp::Run, json!({ "prompt": prompt })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
}

#[tokio::test]
async fn read_rejects_non_string_types_items() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent =
        agent_with(vec![text_round("ok")], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(SubagentOp::Read, json!({ "id": id, "types": ["text", 3] })).await;
    assert_eq!(out["ok"], json!(false), "{out}");
    assert!(out["error"].as_str().unwrap().contains("types"), "{out}");
}

#[tokio::test]
async fn spawn_rejects_resume_and_output_limit_args() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(
        SubagentOp::Spawn,
        json!({ "prompt": "x", "id": "whatever" }),
    )
    .await;
    assert_eq!(out["ok"], json!(false), "{out}");
    assert_eq!(out["status"], json!("failed"), "{out}");
    let out = hook(
        SubagentOp::Spawn,
        json!({ "prompt": "x", "output_limit": 10 }),
    )
    .await;
    assert_eq!(out["ok"], json!(false), "{out}");
}

#[tokio::test]
async fn wait_timeout_reports_running_and_never_cancels() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(NeverProvider);
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "hang" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 50 })).await;
    assert_eq!(out["ok"], json!(true), "{out}");
    assert_eq!(out["status"], json!("running"), "{out}");

    // The run survived the timed-out wait.
    let status = hook(SubagentOp::Status, json!({ "id": id })).await;
    assert_eq!(status["status"], json!("running"), "{status}");

    // Clean up so the test runtime does not drop a live run.
    let killed = hook(SubagentOp::Kill, json!({ "id": id })).await;
    assert_eq!(killed["ok"], json!(true), "{killed}");
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("cancelled"), "{done}");
}

#[tokio::test]
async fn kill_is_idempotent_and_reports_terminal_state() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(NeverProvider);
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "hang" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let first = hook(SubagentOp::Kill, json!({ "id": id, "reason": "test" })).await;
    assert_eq!(first["ok"], json!(true), "{first}");
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("cancelled"), "{done}");
    let second = hook(SubagentOp::Kill, json!({ "id": id })).await;
    assert_eq!(second["ok"], json!(true), "{second}");
    assert_eq!(second["status"], json!("cancelled"), "{second}");
}

#[tokio::test]
async fn read_pages_events_and_reports_gaps() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let agent = agent_with(vec![text_round("page me")], dir.path())
        .with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "do it" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["status"], json!("completed"), "{done}");

    let page = hook(SubagentOp::Read, json!({ "id": id })).await;
    assert_eq!(page["ok"], json!(true), "{page}");
    assert_eq!(page["done"], json!(true), "{page}");
    assert_eq!(page["dropped"], json!(0), "{page}");
    let events = page["events"].as_array().unwrap();
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(types.contains(&"prompt"), "{types:?}");
    assert!(types.contains(&"text"), "{types:?}");
    assert_eq!(types.last(), Some(&"state"), "{types:?}");
    // Sequence numbers are consecutive from 0; the cursor is past the end.
    let seqs: Vec<u64> = events.iter().filter_map(|e| e["seq"].as_u64()).collect();
    assert!(
        seqs.iter().enumerate().all(|(i, seq)| *seq == i as u64),
        "{seqs:?}"
    );
    assert_eq!(page["cursor"].as_u64().unwrap(), seqs.len() as u64);

    // A filtered page advances the cursor over filtered-out events.
    let filtered = hook(SubagentOp::Read, json!({ "id": id, "types": ["state"] })).await;
    let fseq: Vec<u64> = filtered["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert_eq!(fseq.len(), 1, "{filtered}");
    assert_eq!(filtered["cursor"], page["cursor"]);
}

#[tokio::test]
async fn list_filters_state_and_pages_by_creation_order() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent =
        agent_with(vec![text_round("done")], dir.path()).with_subagent_support(support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let first = hook(SubagentOp::Spawn, json!({ "prompt": "one" })).await;
    let first_id = first["id"].as_str().unwrap().to_string();
    let done = hook(
        SubagentOp::Wait,
        json!({ "id": first_id, "timeout_ms": 30_000 }),
    )
    .await;
    assert_eq!(done["status"], json!("completed"), "{done}");

    // The second run never finishes: it stays in the running filter.
    agent.provider = Arc::new(NeverProvider);
    let hook = agent.subagent_hook().unwrap();
    let second = hook(SubagentOp::Spawn, json!({ "prompt": "two" })).await;
    let second_id = second["id"].as_str().unwrap().to_string();

    let all = hook(SubagentOp::List, json!({})).await;
    assert_eq!(all["entries"].as_array().unwrap().len(), 2, "{all}");
    let running = hook(SubagentOp::List, json!({ "state": "running" })).await;
    let rids: Vec<&str> = running["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(rids, vec![second_id.as_str()], "{running}");

    let page1 = hook(SubagentOp::List, json!({ "limit": 1 })).await;
    assert_eq!(page1["entries"].as_array().unwrap().len(), 1, "{page1}");
    let cursor = page1["cursor"].as_str().unwrap().to_string();
    let page2 = hook(SubagentOp::List, json!({ "limit": 1, "cursor": cursor })).await;
    assert_eq!(page2["entries"].as_array().unwrap().len(), 1, "{page2}");
    assert!(page2.get("cursor").is_none(), "{page2}");
    let id1 = page1["entries"][0]["id"].as_str().unwrap();
    let id2 = page2["entries"][0]["id"].as_str().unwrap();
    assert_ne!(id1, id2);

    let _ = hook(SubagentOp::Kill, json!({ "id": second_id })).await;
}

#[tokio::test]
async fn concurrency_cap_rejects_spawn_over_the_running_limit() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(NeverProvider);
    let hook = agent.subagent_hook().unwrap();

    let mut ids = Vec::new();
    for _ in 0..8 {
        let out = hook(SubagentOp::Spawn, json!({ "prompt": "hang" })).await;
        assert_eq!(out["ok"], json!(true), "{out}");
        ids.push(out["id"].as_str().unwrap().to_string());
    }
    let over = hook(SubagentOp::Spawn, json!({ "prompt": "once more" })).await;
    assert_eq!(over["ok"], json!(false), "{over}");
    assert_eq!(over["status"], json!("failed"), "{over}");
    assert!(
        over["error"].as_str().unwrap().contains("concurrency cap"),
        "{over}"
    );

    for id in ids {
        let _ = hook(SubagentOp::Kill, json!({ "id": id })).await;
    }
}

#[tokio::test]
async fn spawned_entry_resumes_through_the_foreground_op() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let provider = Arc::new(MockProvider::recording(vec![
        text_round("first"),
        text_round("second"),
    ]));
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Spawn, json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let done = hook(SubagentOp::Wait, json!({ "id": id, "timeout_ms": 30_000 })).await;
    assert_eq!(done["result"], json!("first"), "{done}");

    let resumed = hook(SubagentOp::Run, json!({ "id": id, "prompt": "b" })).await;
    assert_eq!(resumed["status"], json!("completed"), "{resumed}");
    assert_eq!(resumed["result"], json!("second"), "{resumed}");
    assert_eq!(provider.requests().len(), 2);
}

// Restart rehydration: entries are restored from transcripts so a run can be
// resumed after the process that spawned it exited. Each test builds a fresh
// registry over the same state dir to stand in for the restart.

fn user_event(text: &str) -> lofi_types::SessionEvent {
    lofi_types::SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: lofi_types::SessionEventKind::Message(Message {
            origin: None,
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            kind: PromptKind::default(),
        }),
    }
}

#[tokio::test]
async fn rehydrated_run_resumes_with_replayed_history() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let provider = Arc::new(MockProvider::recording(vec![
        text_round("first"),
        text_round("second"),
    ]));
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    agent.subagents().set_parent_session("parent-x");
    let hook = agent.subagent_hook().unwrap();

    let out = hook(SubagentOp::Run, json!({ "prompt": "a" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let id = out["id"].as_str().unwrap().to_string();
    let session_id = out["session_id"].as_str().unwrap().to_string();

    // The restart: a fresh agent and registry over the same state dir.
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    agent.subagents().set_parent_session("parent-x");
    agent.subagents().rehydrate(&agent, &support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let list = hook(SubagentOp::List, json!({})).await;
    let entries = list["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{list}");
    assert_eq!(entries[0]["id"], json!(id), "{list}");
    assert_eq!(entries[0]["status"], json!("completed"), "{list}");
    assert_eq!(entries[0]["session_id"], json!(session_id), "{list}");

    let status = hook(SubagentOp::Status, json!({ "id": id })).await;
    assert_eq!(status["status"], json!("completed"), "{status}");

    let out = hook(SubagentOp::Run, json!({ "id": id, "prompt": "b" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("second"), "{out}");

    // The resume replayed the transcript: the provider saw the original
    // exchange before the new prompt.
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let roles: Vec<_> = requests[1]
        .iter()
        .map(|m| (m.role, first_text(m)))
        .collect();
    let a = roles
        .iter()
        .position(|(r, t)| *r == Role::User && t == "a")
        .unwrap();
    assert_eq!(roles[a + 1], (Role::Assistant, "first".to_string()));
    assert_eq!(roles[a + 2], (Role::User, "b".to_string()));
}

#[tokio::test]
async fn rehydrate_without_settle_marker_is_cancelled() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    // A transcript whose run never settled: prompt recorded, no marker.
    let store = crate::session::store::SessionStore::open().unwrap();
    let cursor = store
        .create_subagent_cursor(dir.path(), &"p/m".into(), Some("parent-x"), "runid001")
        .unwrap();
    cursor.append_events(&mut [user_event("a")]).unwrap();

    let agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.subagents().set_parent_session("parent-x");
    agent.subagents().rehydrate(&agent, &support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let status = hook(SubagentOp::Status, json!({ "id": "runid001" })).await;
    assert_eq!(status["status"], json!("cancelled"), "{status}");
    assert!(
        status["error"]
            .as_str()
            .unwrap()
            .contains("exited before the run settled"),
        "{status}"
    );
}

#[tokio::test]
async fn rehydrate_legacy_transcript_falls_back_to_file_stem() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let store = crate::session::store::SessionStore::open().unwrap();
    let cursor = store
        .create_subagent_cursor(dir.path(), &"p/m".into(), Some("parent-x"), "runid002")
        .unwrap();
    cursor.append_events(&mut [user_event("a")]).unwrap();
    // Transcripts from before rehydration carry no run_id: strip it from
    // the header to reproduce one.
    let path = cursor.path().to_path_buf();
    let stem = cursor.id();
    let text = std::fs::read_to_string(&path).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut header: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    header.as_object_mut().unwrap().remove("run_id");
    lines[0] = serde_json::to_string(&header).unwrap();
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();

    let agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.subagents().set_parent_session("parent-x");
    agent.subagents().rehydrate(&agent, &support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let list = hook(SubagentOp::List, json!({})).await;
    let entries = list["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{list}");
    assert_eq!(entries[0]["id"], json!(stem), "{list}");
}

#[tokio::test]
async fn rehydrate_runs_once_per_parent_and_after_clear() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let store = crate::session::store::SessionStore::open().unwrap();
    let cursor = store
        .create_subagent_cursor(dir.path(), &"p/m".into(), Some("parent-x"), "runid003")
        .unwrap();
    cursor.append_events(&mut [user_event("a")]).unwrap();

    let agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    let registry = agent.subagents();
    registry.set_parent_session("parent-x");
    registry.rehydrate(&agent, &support(dir.path()));
    registry.rehydrate(&agent, &support(dir.path()));
    let hook = agent.subagent_hook().unwrap();
    let list = hook(SubagentOp::List, json!({})).await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 1, "{list}");

    // A session switch drops the entries; rehydrate for the same parent
    // restores them.
    let _ = registry.clear();
    let list = hook(SubagentOp::List, json!({})).await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 0, "{list}");
    registry.rehydrate(&agent, &support(dir.path()));
    let list = hook(SubagentOp::List, json!({})).await;
    assert_eq!(list["entries"].as_array().unwrap().len(), 1, "{list}");
}

#[tokio::test]
async fn resume_after_rehydrate_trims_dangling_tool_calls() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let store = crate::session::store::SessionStore::open().unwrap();
    let cursor = store
        .create_subagent_cursor(dir.path(), &"p/m".into(), Some("parent-x"), "runid004")
        .unwrap();
    // The process died between the assistant's tool call and its result.
    let mut events = vec![
        user_event("a"),
        lofi_types::SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: lofi_types::SessionEventKind::Message(Message {
                origin: None,
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "exec".to_string(),
                    input: json!({}),
                }],
                kind: PromptKind::default(),
            }),
        },
    ];
    cursor.append_events(&mut events).unwrap();

    let provider = Arc::new(MockProvider::recording(vec![text_round("done")]));
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = provider.clone();
    agent.subagents().set_parent_session("parent-x");
    agent.subagents().rehydrate(&agent, &support(dir.path()));
    let hook = agent.subagent_hook().unwrap();

    let status = hook(SubagentOp::Status, json!({ "id": "runid004" })).await;
    assert_eq!(status["status"], json!("cancelled"), "{status}");

    let out = hook(SubagentOp::Run, json!({ "id": "runid004", "prompt": "b" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let requests = provider.requests();
    assert_eq!(requests.len(), 1);
    let has_tool_use = requests[0].iter().any(|m| {
        m.blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    });
    assert!(!has_tool_use, "{:?}", requests[0]);
    let texts: Vec<_> = requests[0]
        .iter()
        .filter(|m| m.role == Role::User)
        .map(first_text)
        .collect();
    assert!(texts.contains(&"a".to_string()), "{texts:?}");
    assert!(texts.contains(&"b".to_string()), "{texts:?}");
}
