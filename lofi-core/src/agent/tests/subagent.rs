#![allow(clippy::await_holding_lock)]
// The lock serializes tests against the process-global LOFI_STATE_HOME, not
// against shared runtime state, so holding it across awaits is the point.

use super::*;
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

    let out = hook(json!({ "prompt": "do it" })).await;
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

    let out = hook(json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(json!({ "id": id, "prompt": "b" })).await;
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

    let out = hook(json!({ "prompt": "a" })).await;
    let id = out["id"].as_str().unwrap().to_string();
    let out = hook(json!({ "id": id, "prompt": "b", "model": "p/m" })).await;
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

    let out = hook(json!({ "id": "deadbeef", "prompt": "b" })).await;
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

    let out = hook(json!({ "prompt": "a", "output_limit": 5 })).await;
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
    let out = hook(json!({ "prompt": "do it" })).await;
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
    let out = hook(json!({ "prompt": "do it" })).await;
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

    let out = hook(json!({ "id": 123, "prompt": "a" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("must be a string"),
        "{out}"
    );

    let out = hook(json!({ "prompt": "a", "timeout_ms": 0 })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"].as_str().unwrap().contains("positive integer"),
        "{out}"
    );

    let out = hook(json!({})).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("prompt is required"),
        "{out}"
    );

    let out = hook(json!({ "prompt": "   " })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("prompt is required"),
        "{out}"
    );

    let out = hook(json!({ "id": "", "prompt": "a" })).await;
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
    let out = hook(json!({ "prompt": "a", "output_limit": 4 })).await;
    assert_eq!(out["result"].as_str().unwrap(), "ab", "{out}");
    // Limit 6 fits the emoji exactly.
    let out = hook(json!({ "prompt": "b", "output_limit": 6 })).await;
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
    let out = hook(json!({ "prompt": "a", "timeout_ms": 100 })).await;
    assert_eq!(out["status"], json!("timed_out"), "{out}");
    let id = out["id"].as_str().unwrap().to_string();

    let out = hook(json!({ "id": id, "prompt": "b", "timeout_ms": 5000 })).await;
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
            crate::session::store::IndexKind::UserPrompt,
            crate::session::store::IndexKind::AssistantMessage,
            crate::session::store::IndexKind::TurnEnd,
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
        async move { hook(json!({ "prompt": "a" })).await }
    });
    let id = wait_for_status(&registry, "running").await;
    let out = hook(json!({ "id": id, "prompt": "b" })).await;
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
        async move { hook(json!({ "prompt": "a" })).await }
    });
    let id = wait_for_status(&registry, "running").await;
    run.abort();
    let _ = run.await;
    let cancelled = wait_for_status(&registry, "cancelled").await;
    assert_eq!(cancelled, id);

    let out = hook(json!({ "id": id, "prompt": "b", "timeout_ms": 5000 })).await;
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
        let out = hook(json!({ "prompt": format!("p{i}") })).await;
        assert_eq!(out["status"], json!("completed"), "{out}");
        ids.push(out["id"].as_str().unwrap().to_string());
    }
    assert_eq!(registry.ids().len(), 64);
    // The 65th spawn evicts the least recently used finished entry (p0)
    // instead of failing or dropping a run the guest may still resume.
    let out = hook(json!({ "prompt": "overflow" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(registry.ids().len(), 64);
    let overflow = out["id"].as_str().unwrap().to_string();

    let out = hook(json!({ "id": ids[0], "prompt": "again" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("unknown subagent id"),
        "{out}"
    );
    let out = hook(json!({ "id": ids[63], "prompt": "again" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("r65"), "{out}");
    let out = hook(json!({ "id": overflow, "prompt": "again" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    assert_eq!(out["result"], json!("r66"), "{out}");
}

#[tokio::test]
async fn registry_full_of_running_entries_rejects_spawn() {
    let _env = env_lock();
    let _state = StateEnv::new();
    let dir = tempdir().unwrap();
    let mut agent = agent_with(vec![], dir.path()).with_subagent_support(support(dir.path()));
    agent.provider = Arc::new(PendingProvider);
    let registry = agent.subagents();
    let hook = agent.subagent_hook().unwrap();

    let mut runs = Vec::new();
    for _ in 0..64 {
        runs.push(tokio::spawn({
            let hook = hook.clone();
            async move { hook(json!({ "prompt": "a" })).await }
        }));
    }
    for _ in 0..1000 {
        if registry.ids().len() == 64 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(registry.ids().len(), 64, "entries never filled");

    let out = hook(json!({ "prompt": "overflow" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("too many active subagents"),
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

    let out = hook(json!({ "prompt": "b", "system": "custom sys" })).await;
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
    let out = hook(json!({ "prompt": "b" })).await;
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
    let out = hook(json!({ "prompt": "a" })).await;
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
    let out = hook(json!({ "id": id, "prompt": "b" })).await;
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
    let out = hook(json!({ "prompt": "a", "thinking": "high" })).await;
    assert_eq!(out["status"], json!("failed"), "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("does not support thinking"),
        "{out}"
    );
    // `off` is always allowed.
    let out = hook(json!({ "prompt": "a", "thinking": "off" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
}

// Observable through the sandbox, not the fields: after `without_subagent`
// a guest that calls `lofi.models()` gets the unavailable status.
#[tokio::test]
async fn without_subagent_makes_models_unavailable_in_the_sandbox() {
    let dir = tempdir().unwrap();
    let mut executor = ToolExecutor::new(dir.path(), &lofi_types::Config::default(), &[]).unwrap();
    executor.subagent = Some(Arc::new(
        |_| -> std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>> {
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

    let out = hook(json!({ "prompt": "do it" })).await;
    assert_eq!(out["status"], json!("completed"), "{out}");
    let registry = agent.subagents();
    let (cost, usage) = registry.take_unreported().unwrap();
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 7);
    assert!(cost >= 0.0);
    assert!(registry.take_unreported().is_none(), "spend is taken once");
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
