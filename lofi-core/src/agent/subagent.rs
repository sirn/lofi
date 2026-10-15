//! In-process subagents: an `Agent` hook that runs a child agent turn on a
//! forked message history and returns the child's final text to the caller.
//!
//! The sandbox exposes the hook as `lofi.subagent`. Each spawn registers an
//! entry in the session's [`SubagentRegistry`]; passing `id` resumes that
//! entry with its stored history instead of starting fresh.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lofi_code::SubagentFn;
use lofi_error::{Error, Result};
use lofi_types::{Config, ContentBlock, Message, PromptKind, Role, ThinkingLevel};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use super::{lock, select_model, Agent, AgentEvent, RunControls, TurnRequest};
use crate::models::ModelRegistry;
use crate::session::store::{SessionCursor, SessionStore};

/// Maximum time a subagent turn may run when `timeout_ms` is not set.
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// Cap on the `result` text returned to the parent when `output_limit` is
/// not set.
const DEFAULT_OUTPUT_LIMIT: usize = 32 * 1024;
/// Maximum retained subagent entries per session. Spawning past the cap
/// evicts a finished entry to make room; if every entry is still active the
/// spawn fails instead of growing the registry without bound.
const MAX_ENTRIES: usize = 64;

/// Lifecycle states of a subagent entry, reported in tool results.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }
}

struct Entry {
    status: Status,
    agent: Agent,
    messages: Vec<Message>,
    cancel: Arc<AtomicBool>,
    // Retained so the entry outlives a timed-out or abandoned run; reused on
    // the next resume.
    cursor: Option<SessionCursor>,
    // Transcript file name of this child; lets `/subagent` offer a drill-in
    // replay while the run is still live and the entry has not settled.
    session: Option<String>,
    // Bumped on spawn and on every resume: at capacity the terminal entry
    // with the oldest stamp is evicted, never the one just finished.
    used: u64,
}

/// Per-session store of spawned subagents, shared across model switches.
#[derive(Clone, Default)]
pub struct SubagentRegistry {
    entries: Arc<Mutex<HashMap<String, Entry>>>,
    next_used: Arc<AtomicU64>,
    /// Cost and tokens of settled runs not yet folded into the parent
    /// session record.
    unreported: Arc<Mutex<(f64, lofi_types::Usage)>>,
    /// Session id of the owning session, stamped into child transcript
    /// headers so `/subagent` can list the children of one session.
    parent_session: Arc<Mutex<Option<String>>>,
    /// Bumped by `clear()`. A detached run from before a session switch
    /// still calls `report` at settle; its spend must not land in the new
    /// session's totals.
    generation: Arc<AtomicU64>,
}

impl SubagentRegistry {
    /// Record the owning session's transcript id. The turn loop calls this
    /// at the start of every turn, so /new and /resume follow the live
    /// session without extra wiring.
    pub(crate) fn set_parent_session(&self, id: &str) {
        let mut p = lock(&self.parent_session);
        if p.as_deref() != Some(id) {
            *p = Some(id.to_string());
        }
    }

    pub(crate) fn parent_session(&self) -> Option<String> {
        lock(&self.parent_session).clone()
    }
}

impl SubagentRegistry {
    fn stamp(&self) -> u64 {
        self.next_used.fetch_add(1, Ordering::SeqCst)
    }
}

impl SubagentRegistry {
    /// Drop every entry and return the spend settled since the last take.
    /// Called when the session is replaced (/new, /resume), so ids from the
    /// old session cannot be resumed into the new one. The take and the
    /// generation bump happen while holding the unreported lock, so a
    /// settling run either folds into the returned value or is dropped by
    /// the generation check, never into the new session.
    #[must_use]
    pub fn clear(&self) -> (f64, lofi_types::Usage) {
        lock(&self.entries).clear();
        let mut unreported = lock(&self.unreported);
        let usage = std::mem::take(&mut *unreported);
        self.generation.fetch_add(1, Ordering::SeqCst);
        usage
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Spend of runs settled since the last take, if any. The parent turn
    /// loop takes it after each tool round, so runs that settle detached or
    /// after a cancel still count toward the session totals.
    #[must_use]
    pub fn take_unreported(&self) -> Option<(f64, lofi_types::Usage)> {
        let mut u = lock(&self.unreported);
        (u.0 > 0.0 || u.1.input_tokens > 0 || u.1.output_tokens > 0)
            .then(|| std::mem::take(&mut *u))
    }

    /// Live overlay rows for `/subagent`: (id, transcript file name,
    /// running). The store listing is the durable half; this covers runs
    /// still in flight or not yet flushed.
    #[must_use]
    pub fn live(&self) -> Vec<(String, Option<String>, bool)> {
        lock(&self.entries)
            .iter()
            .map(|(id, e)| {
                (
                    id.clone(),
                    e.session.clone(),
                    matches!(e.status, Status::Running),
                )
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<String> {
        lock(&self.entries).keys().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn status_of(&self, id: &str) -> Option<String> {
        lock(&self.entries)
            .get(id)
            .map(|e| e.status.as_str().to_string())
    }
}

/// What the hook needs to resolve subagent model overrides.
#[derive(Clone)]
pub struct SubagentSupport {
    registry: ModelRegistry,
    config: Config,
    /// Workspace root the subagent transcript is filed under.
    cwd: PathBuf,
    /// Whether subagent turns write transcripts. Disabled by the host when
    /// the parent session does not persist (`--no-session`, print mode).
    persist_sessions: bool,
}

impl SubagentSupport {
    #[must_use]
    pub fn new(registry: ModelRegistry, config: Config, cwd: PathBuf) -> Self {
        Self {
            registry,
            config,
            cwd,
            persist_sessions: true,
        }
    }

    #[must_use]
    pub fn with_persistence(mut self, persist_sessions: bool) -> Self {
        self.persist_sessions = persist_sessions;
        self
    }

    /// Build the `lofi.models` hook: the registry's available choices as JSON.
    #[must_use]
    pub fn models_fn(&self) -> lofi_code::ModelsFn {
        let choices: Vec<Value> = self
            .registry
            .choices()
            .iter()
            .map(|c| {
                json!({
                    "model": format!("{}/{}", c.provider, c.id),
                    "name": c.name,
                    "thinking_levels": c.thinking_levels.iter().map(lofi_types::ThinkingLevel::as_str).collect::<Vec<_>>(),
                    "service_tiers": c.service_tiers.iter().map(lofi_types::ServiceTier::as_str).collect::<Vec<_>>(),
                })
            })
            .collect();
        Arc::new(move || json!({ "models": choices }))
    }
}

/// Signal the child turn to stop when the hook future is dropped mid-run
/// (parent turn aborted, exec interrupted). The spawned task observes the
/// flag, then finalizes the entry itself, so an abandoned run still ends
/// with a consistent history and a terminal status.
struct CancelOnDrop {
    cancel: Arc<AtomicBool>,
    armed: bool,
}

impl CancelOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.store(true, Ordering::SeqCst);
        }
    }
}

impl Agent {
    /// Build the `lofi.subagent` hook for this agent, or `None` when the
    /// agent has no subagent support (nested subagent agents).
    #[must_use]
    pub fn subagent_hook(&self) -> Option<SubagentFn> {
        let support = self.subagent_support.clone()?;
        let mut template = self.clone();
        template.subagent_support = None;
        // Fresh registry per child: entries store child agents, so sharing
        // this registry would form an Arc cycle that leaks both the entries
        // and their sandbox workers.
        template.subagents = SubagentRegistry::default();
        let registry = self.subagents.clone();
        Some(Arc::new(move |args: Value| {
            let template = template.clone();
            let support = support.clone();
            let registry = registry.clone();
            Box::pin(async move { run(template, support, registry, args).await })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>
        }))
    }

    /// Attach subagent support so the agent's sandbox exposes `lofi.subagent`.
    #[must_use]
    pub fn with_subagent_support(mut self, support: SubagentSupport) -> Self {
        self.subagent_support = Some(support);
        self
    }

    /// Toggle transcript persistence for spawned subagents. No-op when the
    /// agent has no subagent support.
    #[must_use]
    pub fn with_subagent_persistence(mut self, persist_sessions: bool) -> Self {
        if let Some(support) = self.subagent_support.take() {
            self.subagent_support = Some(support.with_persistence(persist_sessions));
        }
        self
    }
}

/// Absent, null and empty string all mean "not given"; any other non-string
/// value is a caller error, not a silent default.
fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) if !s.is_empty() => Ok(Some(s)),
            Some(_) => Ok(None),
            None => Err(Error::Tool(format!("{key} must be a string"))),
        },
    }
}

fn u64_arg(args: &Value, key: &str) -> Result<Option<u64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if n > 0 => Ok(Some(n)),
            _ => Err(Error::Tool(format!("{key} must be a positive integer"))),
        },
    }
}

// The hook returns errors as a JSON body (never a tool error) so the parent
// model can react to `status` and resume by `id` after a failure.
async fn run(
    template: Agent,
    support: SubagentSupport,
    registry: SubagentRegistry,
    args: Value,
) -> Value {
    match run_inner(template, support, registry, &args).await {
        Ok(value) => value,
        Err(RunRejection::Busy(state)) => {
            // A busy id did not fail: the run is still going, and the parent
            // can resume it once it ends.
            let mut out = json!({
                "ok": false,
                "status": "running",
                "error": format!("subagent is {state}; resume is only possible after a run ends"),
            });
            if let Ok(Some(id)) = str_arg(&args, "id") {
                out["id"] = json!(id);
            }
            out
        }
        Err(RunRejection::Error(e)) => {
            let mut out = json!({ "ok": false, "status": "failed", "error": e.to_string() });
            if let Ok(Some(id)) = str_arg(&args, "id") {
                out["id"] = json!(id);
            }
            out
        }
    }
}

/// Why a subagent call did not start. A busy id is not a failure: its run
/// keeps going and can be resumed once it ends.
enum RunRejection {
    Busy(String),
    Error(Error),
}

impl From<Error> for RunRejection {
    fn from(e: Error) -> Self {
        Self::Error(e)
    }
}

async fn run_inner(
    template: Agent,
    support: SubagentSupport,
    registry: SubagentRegistry,
    args: &Value,
) -> std::result::Result<Value, RunRejection> {
    let prompt = str_arg(args, "prompt")?
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| Error::Tool("prompt is required and must be non-empty".into()))?
        .to_string();
    let timeout = Duration::from_millis(u64_arg(args, "timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS));
    let output_limit = u64_arg(args, "output_limit")?.map_or(DEFAULT_OUTPUT_LIMIT, |v| v as usize);

    let id = str_arg(args, "id")?.map(str::to_string);
    if args.get("id").is_some_and(|v| !v.is_null()) && id.is_none() {
        return Err(Error::Tool("id must be a non-empty string".into()).into());
    }
    let (agent, messages, cursor, cancel, id) = prepare(&template, &support, &registry, id, args)?;
    // The transcript id lets the caller locate the persisted session later;
    // the registry id alone is meaningless after a restart or an eviction.
    let session_id = cursor.as_ref().map(SessionCursor::id);
    // History length before this run: the result text and the timeout
    // rollback both consider only what this run appended.
    let base_len = messages.len();
    // Transcript head before this run: the timeout and panic paths move it
    // back so the transcript and the in-memory history stay in sync.
    let pre_run_leaf = cursor.as_ref().and_then(SessionCursor::leaf_id);
    let task_cursor = cursor.clone();
    let task_pre_run_leaf = pre_run_leaf.clone();
    let guard = CancelOnDrop {
        cancel: cancel.clone(),
        armed: true,
    };

    // Detached task: when the hook future is dropped (parent turn aborted),
    // the guard cancels the turn and the task still finalizes the entry, so
    // the stored history stays consistent and resumable. The task is polled
    // on the caller's runtime — usually the parent's single-threaded sandbox
    // runtime — so parallel children share one thread and synchronous guest
    // code blocks them. A dedicated runtime per child is not worth it: turns
    // are I/O-bound on provider streams, and the shared job registry already
    // pins the lifetime of child processes to the session.
    let task = {
        let registry = registry.clone();
        let id = id.clone();
        tokio::spawn(async move {
            run_turn_with_timeout(
                agent,
                messages,
                cursor,
                cancel,
                registry,
                id,
                base_len,
                pre_run_leaf,
                timeout,
                prompt,
                output_limit,
            )
            .await
        })
    };
    let outcome = match task.await {
        Ok(done) => {
            guard.disarm();
            done
        }
        Err(e) => {
            let mut entries = lock(&registry.entries);
            if let Some(entry) = entries.get_mut(&id) {
                entry.status = Status::Failed;
                entry.agent.exec_worker = Arc::new(std::sync::OnceLock::new());
            }
            drop(entries);
            // The panicked task may have written partial records; move the
            // transcript head back so a resume matches the stored history.
            if let Some(cursor) = task_cursor.as_ref() {
                if let Err(e) = cursor.restore_branch(task_pre_run_leaf) {
                    tracing::debug!(error = %e, "subagent transcript rollback failed");
                }
            }
            // The entry stays in the registry as failed and can be resumed:
            // return the id so the caller can find it. A spawn has no id
            // argument, so the run() fallback cannot attach it.
            let mut out = json!({
                "ok": false,
                "id": id,
                "status": Status::Failed.as_str(),
                "error": format!("subagent task failed: {e}"),
            });
            if let Some(session_id) = session_id {
                out["session_id"] = json!(session_id);
            }
            return Ok(out);
        }
    };

    let mut out = match (outcome.status, outcome.result, outcome.error) {
        (status @ Status::Completed, Some(result), _) => {
            json!({ "ok": true, "id": id, "status": status.as_str(), "result": result })
        }
        (status, _, Some(error)) => json!({
            "ok": false,
            "id": id,
            "status": status.as_str(),
            "error": error,
        }),
        (status, _, _) => json!({
            "ok": false,
            "id": id,
            "status": status.as_str(),
            "error": "subagent completed without a text result",
        }),
    };
    if let Some(full_bytes) = outcome.full_bytes {
        out["truncated"] = json!(true);
        out["full_bytes"] = json!(full_bytes);
    }
    if let Some(usage) = outcome.usage {
        out["usage"] = usage;
    }
    if let Some(session_id) = session_id {
        out["session_id"] = json!(session_id);
    }
    Ok(out)
}

/// The finalized state of one subagent run.
struct RunOutcome {
    status: Status,
    result: Option<String>,
    error: Option<String>,
    // Byte length of the full answer when `result` was cut to fit the output
    // limit.
    full_bytes: Option<usize>,
    // Token and cost totals of the run, when the provider reported any.
    usage: Option<Value>,
}

/// Drain the child event stream into usage totals and the pressure flag.
/// Sender clones escape the turn through tool contexts, so the channel may
/// never close: the caller stops the drain explicitly and the task flushes
/// what is buffered before exiting.
fn spawn_event_drain(
    mut rx: mpsc::Receiver<AgentEvent>,
    totals: Arc<std::sync::Mutex<ChildUsage>>,
    pressure: Arc<AtomicBool>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let drain = tokio::spawn(async move {
        let note = |event: &AgentEvent| match event {
            AgentEvent::RoundUsage { cost, usage } => lock(&totals).add(*cost, usage),
            AgentEvent::ContextPressure { .. } => pressure.store(true, Ordering::SeqCst),
            _ => {}
        };
        loop {
            tokio::select! {
                _ = &mut stop_rx => {
                    while let Ok(event) = rx.try_recv() {
                        note(&event);
                    }
                    break;
                }
                event = rx.recv() => match event {
                    Some(event) => note(&event),
                    None => break,
                },
            }
        }
    });
    (stop_tx, drain)
}

/// Run one turn to completion and finalize the registry entry: status,
/// stored history, transcript rollback on timeout, and the sandbox thread of
/// an idle entry.
#[allow(clippy::too_many_arguments)]
async fn run_turn_with_timeout(
    agent: Agent,
    mut messages: Vec<Message>,
    cursor: Option<SessionCursor>,
    cancel: Arc<AtomicBool>,
    registry: SubagentRegistry,
    id: String,
    base_len: usize,
    pre_run_leaf: Option<String>,
    timeout: Duration,
    prompt: String,
    output_limit: usize,
) -> RunOutcome {
    // Capture the registry generation at run start: a /new or /resume bumps
    // it, and a detached settle must not fold spend into the new session.
    let generation = registry.generation();
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    // The child events have no consumer: the parent transcript records the
    // tool call, not the child stream. Collect the usage so the spend is
    // still visible in the tool result; drain the rest.
    let totals = Arc::new(std::sync::Mutex::new(ChildUsage::default()));
    // run_turn reports a context-pressure stop as Ok: the flag separates it
    // from a real completion, which ends on the final assistant message.
    let pressure = Arc::new(AtomicBool::new(false));
    let (stop_tx, drain) = spawn_event_drain(rx, totals.clone(), pressure.clone());
    let turn = agent.run_turn(
        &mut messages,
        TurnRequest::prompt(prompt, PromptKind::User),
        tx,
        cursor.as_ref(),
        RunControls {
            cancel: Some(cancel.clone()),
            preempt: None,
        },
    );
    let outcome = tokio::time::timeout(timeout, turn).await;
    // Sender clones may outlive the turn, so the channel may never close:
    // stop the drain explicitly; it flushes the buffer before exiting.
    let _ = stop_tx.send(());
    let _ = drain.await;

    let (status, error, keep) = match outcome {
        // run_turn reports cancellation as Ok: the flag decides.
        Ok(Ok(())) if cancel.load(Ordering::SeqCst) => {
            (Status::Cancelled, Some("cancelled".to_string()), true)
        }
        // ContextPressure ends run_turn with Ok: without this check the run
        // would surface as completed with an intermediate note as result. A
        // resume does not compact the child history, so it would stop again.
        Ok(Ok(())) if pressure.load(Ordering::SeqCst) => (
            Status::Failed,
            Some("context window is full".to_string()),
            true,
        ),
        Ok(Ok(())) => (Status::Completed, None, true),
        Ok(Err(e)) => (Status::Failed, Some(e.to_string()), true),
        Err(_) => {
            cancel.store(true, Ordering::SeqCst);
            // The dropped turn may have appended a tool call without
            // its result; resume from the pre-run history instead.
            if let Some(cursor) = cursor.as_ref() {
                if let Err(e) = cursor.restore_branch(pre_run_leaf.clone()) {
                    tracing::debug!(error = %e, "subagent transcript rollback failed");
                }
            }
            (
                Status::TimedOut,
                Some(format!("timed out after {} ms", timeout.as_millis())),
                false,
            )
        }
    };
    if !keep {
        messages.truncate(base_len);
    }
    let (result, full_bytes) = if status == Status::Completed {
        last_text(&messages[base_len..], output_limit)
            .map_or((None, None), |(text, full)| (Some(text), full))
    } else {
        (None, None)
    };
    // A completed run without a final text is a failure: reporting
    // an empty string as a successful result would hide it.
    let (status, error) = if status == Status::Completed && result.is_none() {
        (
            Status::Failed,
            Some("subagent completed without a text result".to_string()),
        )
    } else {
        (status, error)
    };
    let entry_present = if let Some(entry) = lock(&registry.entries).get_mut(&id) {
        entry.status = status;
        // Restamp at settle: the stamp orders eviction, and a long run that
        // finishes last must not become the oldest victim.
        entry.used = registry.stamp();
        entry.messages = messages;
        // Idle entries must not hold a sandbox thread: it restarts
        // lazily on the first exec of the next resume.
        entry.agent.exec_worker = Arc::new(std::sync::OnceLock::new());
        true
    } else {
        false
    };
    // Fold the run's spend into the session's unreported total; the parent
    // turn loop records and reports it after the current tool round. Skip
    // when the entry is gone (clear ran) or the generation moved on: both
    // mean the spend belongs to a session that no longer owns this registry.
    if entry_present {
        let mut unreported = lock(&registry.unreported);
        if registry.generation() == generation {
            lock(&totals).report(&mut unreported);
        }
    }
    let usage = lock(&totals).json();
    RunOutcome {
        status,
        result,
        error,
        full_bytes,
        usage,
    }
}

/// Usage totals of one subagent run, collected from the drained events.
#[derive(Default)]
struct ChildUsage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    cost: f64,
    seen: bool,
}

impl ChildUsage {
    fn add(&mut self, cost: f64, usage: &lofi_types::Usage) {
        self.input += usage.input_tokens;
        self.output += usage.output_tokens;
        self.cache_read += usage.cache_read_tokens;
        self.cache_write += usage.cache_write_tokens;
        // RoundUsage reports the turn's cumulative cost, so keep the latest.
        self.cost = cost;
        self.seen = true;
    }

    fn json(&self) -> Option<Value> {
        self.seen.then(|| {
            json!({
                "input_tokens": self.input,
                "output_tokens": self.output,
                "cache_read_tokens": self.cache_read,
                "cache_write_tokens": self.cache_write,
                "cost": self.cost,
            })
        })
    }

    /// Fold the run totals into the session's unreported subagent spend.
    fn report(&self, unreported: &mut (f64, lofi_types::Usage)) {
        if self.seen {
            unreported.0 += self.cost;
            unreported.1.input_tokens += self.input;
            unreported.1.output_tokens += self.output;
            unreported.1.cache_read_tokens += self.cache_read;
            unreported.1.cache_write_tokens += self.cache_write;
        }
    }
}

/// Build or resume the entry, and hand out the state the turn needs.
#[allow(clippy::type_complexity)]
fn prepare(
    template: &Agent,
    support: &SubagentSupport,
    registry: &SubagentRegistry,
    id: Option<String>,
    args: &Value,
) -> std::result::Result<
    (
        Agent,
        Vec<Message>,
        Option<SessionCursor>,
        Arc<AtomicBool>,
        String,
    ),
    RunRejection,
> {
    if let Some(id) = id {
        return prepare_resume(registry, id, args);
    }

    let mut agent = template.clone();
    // The job registry stays shared with the session: an isolated registry
    // would leak child processes at session end.
    // Fresh sandbox thread per subagent: sharing the parent's `ExecWorker`
    // deadlocks, because the parent worker is busy awaiting this hook while
    // the child's exec jobs queue behind it.
    agent.exec_worker = Arc::new(std::sync::OnceLock::new());
    let mut agent = apply_model_overrides(agent, support, args)?;
    if let Some(system) = str_arg(args, "system")? {
        agent = agent.with_system_prompt(system.to_string());
    }

    let cancel = Arc::new(AtomicBool::new(false));
    // Reserve capacity, the id, and a placeholder entry under one lock;
    // transcript creation does blocking store I/O and must not hold it.
    // A rejected spawn returns inside the block, so it leaves no persistent
    // state behind; the placeholder is filled in with the transcript below.
    let id = {
        let mut entries = lock(&registry.entries);
        evict_for_spawn(&mut entries)?;
        let id = loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            if !entries.contains_key(&id) {
                break id;
            }
        };
        entries.insert(
            id.clone(),
            Entry {
                status: Status::Running,
                agent: agent.clone(),
                messages: Vec::new(),
                cancel: cancel.clone(),
                session: None,
                cursor: None,
                used: registry.stamp(),
            },
        );
        id
    };
    let cursor = if support.persist_sessions {
        let created = SessionStore::open().and_then(|store| {
            let cursor = store.create_subagent_cursor(
                &support.cwd,
                &agent.run_model(),
                registry.parent_session().as_deref(),
            )?;
            let system = agent.system_prompt();
            if !system.is_empty() {
                cursor
                    .record(crate::session::recorder::SessionRecord::System { prompt: system })?;
            }
            Ok(cursor)
        });
        match created {
            Ok(cursor) => Some(cursor),
            Err(e) => {
                tracing::debug!(error = %e, "subagent transcript disabled");
                None
            }
        }
    } else {
        None
    };
    // Fill the placeholder in with the transcript. The entry can be gone
    // when clear() ran during a session switch; the run then finishes
    // untracked, and its settle fold skips the missing entry.
    if let Some(cursor) = &cursor {
        if let Some(entry) = lock(&registry.entries).get_mut(&id) {
            entry.session = cursor
                .path()
                .file_name()
                .map(|n| n.to_string_lossy().into_owned());
            entry.cursor = Some(cursor.clone());
        }
    }
    Ok((agent, Vec::new(), cursor, cancel, id))
}

/// Make room for one more entry by evicting the terminal entry with the
/// oldest stamp. Caller must hold the entries lock.
fn evict_for_spawn(entries: &mut HashMap<String, Entry>) -> std::result::Result<(), RunRejection> {
    if entries.len() < MAX_ENTRIES {
        return Ok(());
    }
    let victim = entries
        .iter()
        .filter(|(_, entry)| entry.status.is_terminal())
        .min_by_key(|(_, entry)| entry.used)
        .map(|(key, _)| key.clone());
    match victim {
        Some(key) => {
            entries.remove(&key);
            Ok(())
        }
        None => Err(Error::Tool(format!(
            "too many active subagents ({MAX_ENTRIES}); wait for one to finish"
        ))
        .into()),
    }
}

/// Resume path of `prepare`: claim a settled entry for a fresh run.
#[allow(clippy::type_complexity)]
fn prepare_resume(
    registry: &SubagentRegistry,
    id: String,
    args: &Value,
) -> std::result::Result<
    (
        Agent,
        Vec<Message>,
        Option<SessionCursor>,
        Arc<AtomicBool>,
        String,
    ),
    RunRejection,
> {
    let mut entries = lock(&registry.entries);
    let entry = entries
        .get_mut(&id)
        .ok_or_else(|| Error::Tool(format!("unknown subagent id: {id}")))?;
    if !entry.status.is_terminal() {
        return Err(RunRejection::Busy(entry.status.as_str().to_string()));
    }
    if str_arg(args, "model")?.is_some()
        || str_arg(args, "thinking")?.is_some()
        || str_arg(args, "system")?.is_some()
    {
        return Err(Error::Tool(
            "model, thinking and system only apply when spawning a subagent".into(),
        )
        .into());
    }
    // Claim the entry under the lock so a concurrent resume of the same
    // id is rejected instead of racing this run.
    entry.status = Status::Running;
    entry.used = registry.stamp();
    // Fresh cancel flag per run: the previous run's flag may still be
    // observed by its lingering sandbox work, and resetting it would
    // uncancel that work.
    entry.cancel = Arc::new(AtomicBool::new(false));
    Ok((
        entry.agent.clone(),
        entry.messages.clone(),
        entry.cursor.clone(),
        entry.cancel.clone(),
        id,
    ))
}

/// Apply the model and thinking overrides of a spawn call to the agent.
fn apply_model_overrides(agent: Agent, support: &SubagentSupport, args: &Value) -> Result<Agent> {
    let mut agent = agent;
    let model_query = str_arg(args, "model")?;
    let thinking_arg = str_arg(args, "thinking")?;
    let thinking_level = thinking_arg
        .map(|level| {
            ThinkingLevel::parse(level)
                .ok_or_else(|| Error::Tool(format!("unknown thinking level: {level}")))
        })
        .transpose()?;
    if let Some(query) = model_query {
        // An explicit level is resolved together with the model so it is
        // validated against the supported levels of the target (and wins
        // over an unsupported configured default).
        let (mut model, thinking) = match thinking_level {
            Some(level) => super::select_model_with_level(
                &support.registry,
                &support.config,
                Some(query),
                level,
            ),
            None => select_model(&support.registry, &support.config, Some(query)),
        }
        .map_err(|e| Error::Tool(format!("invalid model override: {e}")))?;
        model.thinking = thinking;
        let provider_cfg = support
            .config
            .providers
            .get(&model.provider)
            .ok_or_else(|| Error::Tool(format!("provider not found: {}", model.provider)))?;
        let provider = lofi_providers::open(model.api, provider_cfg)
            .map_err(|e| Error::Tool(format!("invalid model override: {e}")))?;
        let auto_continue = support
            .registry
            .auto_continue_policy(&support.config.agent, &model);
        agent = agent
            .with_model(provider, model)
            .with_auto_continue(auto_continue);
    } else if let Some(level) = thinking_level {
        if level != ThinkingLevel::Off {
            let levels: Vec<ThinkingLevel> = support
                .registry
                .choices()
                .iter()
                .find(|c| c.provider == agent.model.provider && c.id == agent.model.id)
                .map(|c| c.thinking_levels.clone())
                .unwrap_or_default();
            if levels.is_empty() {
                return Err(Error::Tool(
                    "the session model does not support thinking".into(),
                ));
            }
            if !levels.contains(&level) {
                let allowed: Vec<&str> = levels.iter().map(ThinkingLevel::as_str).collect();
                return Err(Error::Tool(format!(
                    "thinking level {} not supported by the session model; allowed: {}",
                    level.as_str(),
                    allowed.join(", ")
                )));
            }
        }
        agent.model.thinking = level;
    }
    Ok(agent)
}

/// Answer of the run: the text of its final assistant message, truncated to
/// `limit` bytes at a UTF-8 boundary. `messages` must cover only the run in
/// progress, so a resumed subagent never returns an earlier turn's answer.
/// A run whose final message has no text has no answer: returning an earlier
/// message would report an intermediate note as the final result. Returns
/// the text and, when it was cut, the full byte length.
fn last_text(messages: &[Message], limit: usize) -> Option<(String, Option<usize>)> {
    let message = messages.last()?;
    if message.role != Role::Assistant {
        return None;
    }
    // Text blocks join with a newline: a provider may split one answer into
    // several blocks (thinking blocks in between), and bare concatenation
    // would glue words together.
    let mut text: String = message
        .blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        return None;
    }
    let full = (text.len() > limit).then_some(text.len());
    if full.is_some() {
        let mut end = limit;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Some((text, full))
}
