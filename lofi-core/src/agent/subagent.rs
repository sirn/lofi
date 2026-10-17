//! In-process subagents: an `Agent` hook that runs a child agent turn on a
//! forked message history and returns the child's final text to the caller.
//!
//! The sandbox exposes the hook as `lofi.subagent`. Each spawn registers an
//! entry in the session's [`SubagentRegistry`]; passing `id` resumes that
//! entry with its stored history instead of starting fresh.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lofi_code::{SubagentFn, SubagentOp};
use lofi_error::{Error, Result};
use lofi_types::{Config, ContentBlock, Message, PromptKind, Role, ThinkingLevel};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::AbortHandle;

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
/// Maximum concurrently running subagents per session. Bounds provider
/// load from fan-out; separate from the retention cap above.
const MAX_RUNNING: usize = 8;
/// Retained events per run for `subagentRead`. Overflow drops the oldest
/// events; the read reports the gap via `dropped`.
const EVENT_CAP: usize = 512;
/// Cap on any string carried by a public event.
const EVENT_TEXT_CAP: usize = 8 * 1024;
/// Default `subagentWait` limit: below the two-minute guest exec timeout so
/// a default wait reports `running` instead of dying with the exec.
const DEFAULT_WAIT_MS: u64 = 60_000;
/// Grace period between a kill's cooperative cancel and the task abort
/// backstop.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Smallest periodic notice interval `subagentNotify` accepts.
const MIN_NOTIFY_INTERVAL_MS: u64 = 5_000;

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
    // Immutable creation order; the `subagentList` page cursor.
    born: u64,
    // Terminal JSON body, stored at settle so `subagentWait` and
    // `subagentStatus` can return it to any later caller.
    outcome: Option<Value>,
    // Status broadcast for `subagentWait`. Never blocks, so sending under
    // the entries lock is safe.
    state_tx: watch::Sender<Status>,
    // Abort backstop for `subagentKill` and `clear()` when the cooperative
    // cancel flag is not observed.
    abort: Option<AbortHandle>,
    // Bounded public event log paged by `subagentRead`.
    events: EventLog,
    // Terminal notice switch; on by default, cleared by
    // `subagentNotify({enabled: false})` and by `clear()`.
    notify_terminal: bool,
}

/// Bounded per-run event buffer for `subagentRead`. Sequence numbers are
/// stable per run; overflow evicts from the front and the read reports the
/// gap as `dropped`.
#[derive(Default)]
struct EventLog {
    events: VecDeque<Value>,
    next_seq: u64,
}

impl EventLog {
    fn push(&mut self, kind: &str, mut data: Value) {
        data["seq"] = json!(self.next_seq);
        data["type"] = json!(kind);
        self.next_seq += 1;
        self.events.push_back(data);
        if self.events.len() > EVENT_CAP {
            self.events.pop_front();
        }
    }

    /// Events from `cursor` on, filtered by `types`. Returns the page, the
    /// next cursor, and how many events before `cursor` were evicted.
    fn page(&self, cursor: u64, limit: usize, types: Option<&[String]>) -> (Vec<Value>, u64, u64) {
        let first = self.next_seq - self.events.len() as u64;
        let dropped = first.saturating_sub(cursor);
        let mut out = Vec::new();
        let mut next = cursor.max(first);
        for event in &self.events {
            let seq = event["seq"].as_u64().unwrap_or(0);
            // The cursor advances over filtered-out events: a filtered page
            // must not stall the reader.
            next = seq + 1;
            if let Some(types) = types {
                if !types.iter().any(|t| event["type"].as_str() == Some(t)) {
                    continue;
                }
            }
            out.push(event.clone());
            if out.len() >= limit {
                break;
            }
        }
        (out, next, dropped)
    }
}

/// A state change of a background subagent, published to the host UI as a
/// notice prompt. Subagent ids are strings, so this does not reuse
/// `JobNotice`.
#[derive(Clone, Debug)]
pub struct SubagentNotice {
    pub id: String,
    pub text: String,
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
    /// Creation order for `subagentList` cursors.
    next_born: Arc<AtomicU64>,
    /// Runtime that owns background runs. Captured at the start of the
    /// parent's turn on the host; spawning on the sandbox worker runtime
    /// would stop the run unsettled when the worker idles out.
    host_handle: Arc<std::sync::OnceLock<tokio::runtime::Handle>>,
    /// Terminal and progress notices for the host UI.
    notices: Arc<NoticeHub>,
}

/// Bounded broadcast of subagent notices. Lagging subscribers skip ahead;
/// a missed terminal notice is recoverable through `subagentStatus`.
struct NoticeHub {
    tx: broadcast::Sender<SubagentNotice>,
}

impl Default for NoticeHub {
    fn default() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { tx }
    }
}

impl SubagentRegistry {
    /// Notice stream for the host UI. Re-subscribe after a session rebuild;
    /// a lagging receiver skips to the newest notices.
    #[must_use]
    pub fn subscribe_notices(&self) -> broadcast::Receiver<SubagentNotice> {
        self.notices.tx.subscribe()
    }

    fn publish_notice(&self, id: &str, text: String) {
        let _ = self.notices.tx.send(SubagentNotice {
            id: id.to_string(),
            text,
        });
    }

    /// Record the runtime that owns background runs. Called at the start of
    /// the parent's turn, which always runs on the host runtime.
    pub(crate) fn set_host_handle(&self, handle: tokio::runtime::Handle) {
        let _ = self.host_handle.set(handle);
    }

    /// The runtime to spawn background runs on. Falls back to the current
    /// runtime (tests, print mode before the first turn); never panics
    /// because every caller runs under some runtime.
    fn host_handle(&self) -> tokio::runtime::Handle {
        self.host_handle
            .get()
            .cloned()
            .unwrap_or_else(tokio::runtime::Handle::current)
    }
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

    /// The terminal body stored at settle. `None` while the run is live or
    /// after the entry was evicted.
    fn outcome_of(&self, id: &str) -> Option<Value> {
        lock(&self.entries)
            .get(id)
            .and_then(|entry| entry.outcome.clone())
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
    ///
    /// Running entries are cancelled first: a background run can outlive
    /// the switch by minutes, and without the cancel it would keep spending
    /// against a session that no longer owns it. Their jobs die with them;
    /// their terminal notices are silenced.
    #[must_use]
    pub fn clear(&self) -> (f64, lofi_types::Usage) {
        {
            let mut entries = lock(&self.entries);
            for (id, entry) in entries.iter_mut() {
                if entry.status.is_terminal() {
                    continue;
                }
                entry.cancel.store(true, Ordering::SeqCst);
                entry.notify_terminal = false;
                entry.agent.jobs().kill_owned(id);
                if let Some(abort) = &entry.abort {
                    abort.abort();
                }
            }
            entries.clear();
        }
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

/// Finalizes an entry when its task ends unwatched: a panic, or an abort
/// from `subagentKill`'s backstop or `clear()`. Normal completion disarms
/// it; a kill settles first, so the guard no-ops. All work is synchronous:
/// Drop cannot await.
struct SettleOnDrop {
    registry: SubagentRegistry,
    id: String,
    cursor: Option<SessionCursor>,
    pre_run_leaf: Option<String>,
    cancel: Arc<AtomicBool>,
    generation: u64,
    armed: bool,
}

impl SettleOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SettleOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut entries = lock(&self.registry.entries);
        let Some(entry) = entries.get_mut(&self.id) else {
            return;
        };
        if entry.status.is_terminal() {
            return;
        }
        let status = if self.cancel.load(Ordering::SeqCst) {
            Status::Cancelled
        } else {
            Status::Failed
        };
        entry.status = status;
        entry.agent.exec_worker = Arc::new(std::sync::OnceLock::new());
        entry.outcome = Some(json!({
            "ok": false,
            "id": self.id,
            "status": status.as_str(),
            "error": if status == Status::Cancelled {
                "cancelled"
            } else {
                "subagent task ended unexpectedly"
            },
        }));
        entry
            .events
            .push("state", json!({ "status": status.as_str() }));
        let _ = entry.state_tx.send(status);
        let notify = entry.notify_terminal;
        drop(entries);
        // The ended task may have written partial records; move the
        // transcript head back so a resume matches the stored history.
        if let Some(cursor) = self.cursor.as_ref() {
            if let Err(e) = cursor.restore_branch(self.pre_run_leaf.clone()) {
                tracing::debug!(error = %e, "subagent transcript rollback failed");
            }
        }
        if notify && self.registry.generation() == self.generation {
            self.registry.publish_notice(
                &self.id,
                format!("subagent {} {}", self.id, status.as_str()),
            );
        }
    }
}

impl Agent {
    /// Build the `lofi.subagent*` hook for this agent, or `None` when the
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
        Some(Arc::new(move |op: SubagentOp, args: Value| {
            let template = template.clone();
            let support = support.clone();
            let registry = registry.clone();
            Box::pin(async move { dispatch(template, support, registry, op, args).await })
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
async fn dispatch(
    template: Agent,
    support: SubagentSupport,
    registry: SubagentRegistry,
    op: SubagentOp,
    args: Value,
) -> Value {
    match op {
        SubagentOp::Run => run(template, support, registry, args).await,
        SubagentOp::Spawn => spawn(&template, &support, &registry, &args),
        SubagentOp::Status => status_op(&registry, &args),
        SubagentOp::List => list_op(&registry, &args),
        SubagentOp::Wait => wait_op(&registry, args).await,
        SubagentOp::Read => read_op(&registry, &args),
        SubagentOp::Kill => kill_op(&registry, &args),
        SubagentOp::Notify => notify_op(&registry, &args),
    }
}

/// Rejections carry no tool error so the model sees a structured body.
fn rejection_json(rejection: RunRejection, args: &Value) -> Value {
    let mut out = match rejection {
        RunRejection::Busy(state) => {
            // A busy id did not fail: the run is still going, and the parent
            // can resume it once it ends.
            json!({
                "ok": false,
                "status": "running",
                "error": format!("subagent is {state}; resume is only possible after a run ends"),
            })
        }
        RunRejection::Error(e) => {
            json!({ "ok": false, "status": "failed", "error": e.to_string() })
        }
    };
    if let Ok(Some(id)) = str_arg(args, "id") {
        out["id"] = json!(id);
    }
    out
}

fn arg_error_json(e: &Error) -> Value {
    json!({ "ok": false, "status": "failed", "error": e.to_string() })
}

/// Every id-addressed op shares these two bodies.
fn required_id(args: &Value) -> std::result::Result<String, Value> {
    match str_arg(args, "id") {
        Ok(Some(id)) => Ok(id.to_string()),
        Ok(None) => Err(json!({
            "ok": false,
            "status": "failed",
            "error": "id is required and must be a non-empty string",
        })),
        Err(e) => Err(arg_error_json(&e)),
    }
}

fn unknown_id(id: &str) -> Value {
    json!({ "ok": false, "id": id, "status": "failed", "error": "unknown subagent id" })
}

/// Foreground `lofi.subagent`: start the run and await it. Dropping this
/// future (exec timeout, Esc) arms `CancelOnDrop` and cancels the child.
async fn run(
    template: Agent,
    support: SubagentSupport,
    registry: SubagentRegistry,
    args: Value,
) -> Value {
    let started = match start(&template, &support, &registry, &args) {
        Ok(started) => started,
        Err(rejection) => return rejection_json(rejection, &args),
    };
    let guard = CancelOnDrop {
        cancel: started.cancel.clone(),
        armed: true,
    };
    // A JoinError means the task panicked; its SettleOnDrop already stored
    // the failure outcome. Both paths read the entry's stored outcome.
    let _ = started.handle.await;
    guard.disarm();
    registry
        .outcome_of(&started.id)
        .unwrap_or_else(|| unknown_id(&started.id))
}

/// Background `lofi.subagentSpawn`: start the run and return its handle id
/// at once. No `CancelOnDrop`: the run survives the end of this exec.
fn spawn(
    template: &Agent,
    support: &SubagentSupport,
    registry: &SubagentRegistry,
    args: &Value,
) -> Value {
    for rejected in ["id", "output_limit"] {
        if args.get(rejected).is_some_and(|v| !v.is_null()) {
            return json!({
                "ok": false,
                "status": "failed",
                "error": format!("subagentSpawn does not accept {rejected}"),
            });
        }
    }
    let started = match start(template, support, registry, args) {
        Ok(started) => started,
        Err(rejection) => return rejection_json(rejection, args),
    };
    // Detaching a JoinHandle keeps the task alive; the entry tracks it.
    drop(started.handle);
    let mut out = json!({ "ok": true, "id": started.id, "status": "running" });
    if let Some(session_id) = started.session_id {
        out["session_id"] = json!(session_id);
    }
    out
}

fn status_op(registry: &SubagentRegistry, args: &Value) -> Value {
    let id = match required_id(args) {
        Ok(id) => id,
        Err(out) => return out,
    };
    let entries = lock(&registry.entries);
    let Some(entry) = entries.get(&id) else {
        return unknown_id(&id);
    };
    let mut out = json!({ "ok": true, "id": id, "status": entry.status.as_str() });
    if let Some(session_id) = entry.cursor.as_ref().map(SessionCursor::id) {
        out["session_id"] = json!(session_id);
    }
    if let Some(outcome) = &entry.outcome {
        if let Some(usage) = outcome.get("usage") {
            out["usage"] = usage.clone();
        }
        if let Some(error) = outcome.get("error") {
            out["error"] = error.clone();
        }
    }
    out
}

fn list_op(registry: &SubagentRegistry, args: &Value) -> Value {
    let state = match str_arg(args, "state") {
        Ok(state) => state,
        Err(e) => return arg_error_json(&e),
    };
    let running_only = match state {
        None => None,
        Some("running") => Some(true),
        Some("terminal") => Some(false),
        Some(_) => {
            return json!({
                "ok": false,
                "status": "failed",
                "error": "state must be \"running\" or \"terminal\"",
            })
        }
    };
    let limit = match u64_arg(args, "limit") {
        Ok(limit) => limit.unwrap_or(20).min(100) as usize,
        Err(e) => return arg_error_json(&e),
    };
    let after = match str_arg(args, "cursor") {
        Ok(None) => 0,
        Ok(Some(cursor)) => match cursor.parse::<u64>() {
            Ok(born) => born,
            Err(_) => {
                return json!({
                    "ok": false,
                    "status": "failed",
                    "error": "cursor is not a valid page cursor",
                })
            }
        },
        Err(e) => return arg_error_json(&e),
    };
    let entries = lock(&registry.entries);
    let mut rows: Vec<(&String, &Entry)> = entries
        .iter()
        .filter(|(_, entry)| entry.born > after)
        .filter(|(_, entry)| running_only.is_none_or(|r| (entry.status == Status::Running) == r))
        .collect();
    rows.sort_by_key(|(_, entry)| entry.born);
    let more = rows.len() > limit;
    rows.truncate(limit);
    let cursor = more
        .then(|| rows.last().map(|(_, entry)| entry.born))
        .flatten();
    let list: Vec<Value> = rows
        .into_iter()
        .map(|(id, entry)| {
            let mut row = json!({ "id": id, "status": entry.status.as_str() });
            if let Some(session_id) = entry.cursor.as_ref().map(SessionCursor::id) {
                row["session_id"] = json!(session_id);
            }
            row
        })
        .collect();
    let mut out = json!({ "ok": true, "entries": list });
    if let Some(cursor) = cursor {
        out["cursor"] = json!(cursor.to_string());
    }
    out
}

async fn wait_op(registry: &SubagentRegistry, args: Value) -> Value {
    let id = match required_id(&args) {
        Ok(id) => id,
        Err(out) => return out,
    };
    let timeout = match u64_arg(&args, "timeout_ms") {
        Ok(ms) => Duration::from_millis(ms.unwrap_or(DEFAULT_WAIT_MS)),
        Err(e) => return arg_error_json(&e),
    };
    let mut rx = {
        let entries = lock(&registry.entries);
        match entries.get(&id) {
            Some(entry) => entry.state_tx.subscribe(),
            None => return unknown_id(&id),
        }
    };
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        if rx.borrow().is_terminal() {
            return registry.outcome_of(&id).unwrap_or_else(|| unknown_id(&id));
        }
        tokio::select! {
            biased;
            () = &mut deadline => {
                // A wait timeout never cancels: the run keeps going and can
                // be awaited again.
                return json!({ "ok": true, "id": id, "status": "running" });
            }
            changed = rx.changed() => {
                if changed.is_err() {
                    // The entry was evicted or cleared while waiting.
                    return registry
                        .outcome_of(&id)
                        .unwrap_or_else(|| unknown_id(&id));
                }
            }
        }
    }
}

fn read_op(registry: &SubagentRegistry, args: &Value) -> Value {
    let id = match required_id(args) {
        Ok(id) => id,
        Err(out) => return out,
    };
    let cursor = match u64_arg(args, "cursor") {
        Ok(cursor) => cursor.unwrap_or(0),
        Err(e) => return arg_error_json(&e),
    };
    let limit = match u64_arg(args, "limit") {
        Ok(limit) => limit.unwrap_or(50).min(200) as usize,
        Err(e) => return arg_error_json(&e),
    };
    let types: Option<Vec<String>> = match args.get("types") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(_) => {
            return json!({
                "ok": false,
                "status": "failed",
                "error": "types must be an array of strings",
            })
        }
    };
    let entries = lock(&registry.entries);
    let Some(entry) = entries.get(&id) else {
        return unknown_id(&id);
    };
    let (events, next, dropped) = entry.events.page(cursor, limit, types.as_deref());
    let done = entry.status.is_terminal() && next >= entry.events.next_seq;
    json!({
        "ok": true,
        "id": id,
        "events": events,
        "cursor": next,
        "dropped": dropped,
        "done": done,
    })
}

fn kill_op(registry: &SubagentRegistry, args: &Value) -> Value {
    let id = match required_id(args) {
        Ok(id) => id,
        Err(out) => return out,
    };
    let reason = match str_arg(args, "reason") {
        Ok(reason) => reason.map(str::to_string),
        Err(e) => return arg_error_json(&e),
    };
    let (jobs, abort) = {
        let mut entries = lock(&registry.entries);
        let Some(entry) = entries.get_mut(&id) else {
            return unknown_id(&id);
        };
        if entry.status.is_terminal() {
            // Idempotent: killing a finished run reports its state.
            return json!({ "ok": true, "id": id, "status": entry.status.as_str() });
        }
        entry.cancel.store(true, Ordering::SeqCst);
        let mut state = json!({ "status": "cancelling" });
        if let Some(reason) = reason {
            state["reason"] = json!(reason);
        }
        entry.events.push("state", state);
        (entry.agent.jobs(), entry.abort.clone())
    };
    // The child's process tree dies with it.
    jobs.kill_owned(&id);
    // Backstop: a run that never observes the cancel flag is aborted after
    // a grace period; its SettleOnDrop then settles it as cancelled.
    if let Some(abort) = abort {
        let registry = registry.clone();
        let id = id.clone();
        registry.host_handle().spawn(async move {
            tokio::time::sleep(KILL_GRACE).await;
            let wedged = lock(&registry.entries)
                .get(&id)
                .is_some_and(|entry| !entry.status.is_terminal());
            if wedged {
                abort.abort();
            }
        });
    }
    json!({ "ok": true, "id": id, "status": "running" })
}

fn notify_op(registry: &SubagentRegistry, args: &Value) -> Value {
    let id = match required_id(args) {
        Ok(id) => id,
        Err(out) => return out,
    };
    let enabled = match args.get("enabled") {
        None | Some(Value::Null) => None,
        Some(v) => match v.as_bool() {
            Some(enabled) => Some(enabled),
            None => {
                return json!({
                    "ok": false,
                    "status": "failed",
                    "error": "enabled must be a boolean",
                })
            }
        },
    };
    let interval = match u64_arg(args, "interval_ms") {
        Ok(ms) => ms.map(|ms| Duration::from_millis(ms.max(MIN_NOTIFY_INTERVAL_MS))),
        Err(e) => return arg_error_json(&e),
    };
    let mut entries = lock(&registry.entries);
    let Some(entry) = entries.get_mut(&id) else {
        return unknown_id(&id);
    };
    if let Some(enabled) = enabled {
        entry.notify_terminal = enabled;
    }
    if let Some(interval) = interval {
        if entry.status == Status::Running && entry.notify_terminal {
            // One tick task per run; it exits when the run settles or the
            // entry is cleared.
            let registry = registry.clone();
            let id = id.clone();
            registry.host_handle().spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    let running = {
                        let entries = lock(&registry.entries);
                        entries
                            .get(&id)
                            .is_some_and(|entry| entry.status == Status::Running)
                    };
                    if !running {
                        break;
                    }
                    registry.publish_notice(&id, format!("subagent {id} still running"));
                }
            });
        }
    }
    json!({ "ok": true, "id": id })
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

/// A started run: prepared, registered, and spawned on the host runtime.
struct Started {
    id: String,
    session_id: Option<String>,
    cancel: Arc<AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

/// Shared start path for foreground runs, resumes, and background spawns:
/// parse and prepare, then spawn the supervised task on the host runtime.
/// Spawning on the sandbox worker runtime would stop the run unsettled
/// when the worker idles out; `set_host_handle` records the host runtime
/// at the start of every parent turn.
fn start(
    template: &Agent,
    support: &SubagentSupport,
    registry: &SubagentRegistry,
    args: &Value,
) -> std::result::Result<Started, RunRejection> {
    let prompt = str_arg(args, "prompt")?
        .filter(|p| !p.trim().is_empty())
        .ok_or_else(|| Error::Tool("prompt is required and must be non-empty".into()))?
        .to_string();
    let timeout = Duration::from_millis(u64_arg(args, "timeout_ms")?.unwrap_or(DEFAULT_TIMEOUT_MS));
    let output_limit = u64_arg(args, "output_limit")?.map_or(DEFAULT_OUTPUT_LIMIT, |v| v as usize);
    let token_budget = u64_arg(args, "token_budget")?;
    if token_budget == Some(0) {
        return Err(Error::Tool("token_budget must be positive when set".into()).into());
    }

    let id = str_arg(args, "id")?.map(str::to_string);
    if args.get("id").is_some_and(|v| !v.is_null()) && id.is_none() {
        return Err(Error::Tool("id must be a non-empty string".into()).into());
    }
    let (agent, messages, cursor, cancel, id) = prepare(template, support, registry, id, args)?;
    // The transcript id lets the caller locate the persisted session later;
    // the registry id alone is meaningless after a restart or an eviction.
    let session_id = cursor.as_ref().map(SessionCursor::id);
    // History length before this run: the result text and the timeout
    // rollback both consider only what this run appended.
    let base_len = messages.len();
    // Transcript head before this run: the timeout and unwatched-end paths
    // move it back so the transcript and the in-memory history stay in sync.
    let pre_run_leaf = cursor.as_ref().and_then(SessionCursor::leaf_id);
    // Capture the generation at admission, not in the task: a clear()
    // between the two would let the task publish into the new session.
    let generation = registry.generation();
    {
        let mut entries = lock(&registry.entries);
        if let Some(entry) = entries.get_mut(&id) {
            let mut text = prompt.clone();
            text.truncate(EVENT_TEXT_CAP);
            entry.events.push("prompt", json!({ "text": text }));
        }
    }
    let handle = {
        let registry = registry.clone();
        let id = id.clone();
        let cancel = cancel.clone();
        let task_session_id = session_id.clone();
        registry.host_handle().spawn(async move {
            run_task(
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
                token_budget,
                generation,
                task_session_id,
            )
            .await;
        })
    };
    if let Some(entry) = lock(&registry.entries).get_mut(&id) {
        entry.abort = Some(handle.abort_handle());
    }
    Ok(Started {
        id,
        session_id,
        cancel,
        handle,
    })
}

/// The supervised body of one run: the turn, then the terminal record. The
/// `SettleOnDrop` guard covers the paths this body never reaches (panic,
/// abort), so every run settles exactly once.
#[allow(clippy::too_many_arguments)]
async fn run_task(
    agent: Agent,
    messages: Vec<Message>,
    cursor: Option<SessionCursor>,
    cancel: Arc<AtomicBool>,
    registry: SubagentRegistry,
    id: String,
    base_len: usize,
    pre_run_leaf: Option<String>,
    timeout: Duration,
    prompt: String,
    output_limit: usize,
    token_budget: Option<u64>,
    generation: u64,
    session_id: Option<String>,
) {
    let guard = SettleOnDrop {
        registry: registry.clone(),
        id: id.clone(),
        cursor: cursor.clone(),
        pre_run_leaf: pre_run_leaf.clone(),
        cancel: cancel.clone(),
        generation,
        armed: true,
    };
    let outcome = run_turn_with_timeout(
        agent,
        messages,
        cursor,
        cancel,
        registry.clone(),
        id.clone(),
        base_len,
        pre_run_leaf,
        timeout,
        prompt,
        output_limit,
        token_budget,
        generation,
    )
    .await;
    let status = outcome.status;
    let out = outcome_json(&id, session_id.as_deref(), outcome);
    let notify = {
        let mut entries = lock(&registry.entries);
        match entries.get_mut(&id) {
            Some(entry) => {
                entry.outcome = Some(out);
                entry
                    .events
                    .push("state", json!({ "status": status.as_str() }));
                let _ = entry.state_tx.send(status);
                entry.notify_terminal
            }
            // clear() removed the entry: nothing to record, nothing to say.
            None => false,
        }
    };
    if notify && registry.generation() == generation {
        registry.publish_notice(&id, terminal_notice(&id, status));
    }
    guard.disarm();
}

/// One-line terminal notice: status plus the first line of the outcome.
fn terminal_notice(id: &str, status: Status) -> String {
    format!("subagent {id} {}", status.as_str())
}

/// The JSON body stored at settle and returned to the foreground caller,
/// `subagentWait`, and (minus the result) `subagentStatus`.
fn outcome_json(id: &str, session_id: Option<&str>, outcome: RunOutcome) -> Value {
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
    out
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

/// Drain the child event stream into usage totals, the pressure flag, and
/// the entry's public event log. Sender clones escape the turn through tool
/// contexts, so the channel may never close: the caller stops the drain
/// explicitly and the task flushes what is buffered before exiting.
fn spawn_event_drain(
    mut rx: mpsc::Receiver<AgentEvent>,
    totals: Arc<std::sync::Mutex<ChildUsage>>,
    pressure: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    token_budget: Option<u64>,
    registry: SubagentRegistry,
    id: String,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let drain = tokio::spawn(async move {
        let note = |event: &AgentEvent| {
            match event {
                AgentEvent::RoundUsage { cost, usage } => {
                    let spent = {
                        let mut totals = lock(&totals);
                        totals.add(*cost, usage);
                        totals.input + totals.output
                    };
                    // Cancel as soon as the budget is crossed; the settle path
                    // reports the breach as the failure reason.
                    if token_budget.is_some_and(|budget| spent > budget) {
                        cancel.store(true, Ordering::SeqCst);
                    }
                }
                AgentEvent::ContextPressure { .. } => pressure.store(true, Ordering::SeqCst),
                _ => {}
            }
            let public = match event {
                AgentEvent::Text(text) => Some(("text", json!({ "text": cap_event_text(text) }))),
                AgentEvent::ToolStart { name, .. } => Some(("tool_start", json!({ "name": name }))),
                AgentEvent::ToolEnd {
                    result, is_error, ..
                } => Some((
                    "tool_end",
                    json!({ "result": cap_event_text(result), "is_error": is_error }),
                )),
                AgentEvent::RoundUsage { cost, usage } => {
                    Some(("usage", json!({ "cost": cost, "usage": usage })))
                }
                _ => None,
            };
            if let Some((kind, data)) = public {
                if let Some(entry) = lock(&registry.entries).get_mut(&id) {
                    entry.events.push(kind, data);
                }
            }
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

fn cap_event_text(text: &str) -> String {
    let mut text = text.to_string();
    text.truncate(EVENT_TEXT_CAP);
    text
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
    token_budget: Option<u64>,
    // Captured at admission: a /new or /resume bumps it, and a detached
    // settle must not fold spend into the new session.
    generation: u64,
) -> RunOutcome {
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    // The child events have no consumer: the parent transcript records the
    // tool call, not the child stream. Collect the usage so the spend is
    // still visible in the tool result; drain the rest.
    let totals = Arc::new(std::sync::Mutex::new(ChildUsage::default()));
    // run_turn reports a context-pressure stop as Ok: the flag separates it
    // from a real completion, which ends on the final assistant message.
    let pressure = Arc::new(AtomicBool::new(false));
    let (stop_tx, drain) = spawn_event_drain(
        rx,
        totals.clone(),
        pressure.clone(),
        cancel.clone(),
        token_budget,
        registry.clone(),
        id.clone(),
    );
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

    // The drain flushed before this point, so a breached budget is visible
    // here even when the provider finished the round before noticing.
    let budget_over = {
        let totals = lock(&totals);
        let spent = totals.input + totals.output;
        token_budget
            .filter(|&budget| spent > budget)
            .map(|budget| (spent, budget))
    };
    let (status, error, keep) = classify_outcome(
        outcome,
        budget_over,
        &cancel,
        &pressure,
        cursor.as_ref(),
        pre_run_leaf,
        timeout,
    );
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

/// Map the turn result to the terminal status. `budget_over` is `(spent,
/// budget)` when the run crossed its token budget; the drain flushed before
/// this call, so the breach is visible even when the provider finished the
/// round before noticing the cancel flag.
#[allow(clippy::too_many_arguments)]
fn classify_outcome(
    outcome: std::result::Result<Result<()>, tokio::time::error::Elapsed>,
    budget_over: Option<(u64, u64)>,
    cancel: &Arc<AtomicBool>,
    pressure: &Arc<AtomicBool>,
    cursor: Option<&SessionCursor>,
    pre_run_leaf: Option<String>,
    timeout: Duration,
) -> (Status, Option<String>, bool) {
    match outcome {
        Ok(Ok(())) => {
            if let Some((spent, budget)) = budget_over {
                (
                    Status::Failed,
                    Some(format!("token budget exceeded ({spent} > {budget})")),
                    true,
                )
            // run_turn reports cancellation as Ok: the flag decides.
            } else if cancel.load(Ordering::SeqCst) {
                (Status::Cancelled, Some("cancelled".to_string()), true)
            // ContextPressure ends run_turn with Ok: without this check the run
            // would surface as completed with an intermediate note as result. A
            // resume does not compact the child history, so it would stop again.
            } else if pressure.load(Ordering::SeqCst) {
                (
                    Status::Failed,
                    Some("context window is full".to_string()),
                    true,
                )
            } else {
                (Status::Completed, None, true)
            }
        }
        Ok(Err(e)) => (Status::Failed, Some(e.to_string()), true),
        Err(_) => {
            cancel.store(true, Ordering::SeqCst);
            // The dropped turn may have appended a tool call without
            // its result; resume from the pre-run history instead.
            if let Some(cursor) = cursor {
                if let Err(e) = cursor.restore_branch(pre_run_leaf) {
                    tracing::debug!(error = %e, "subagent transcript rollback failed");
                }
            }
            (
                Status::TimedOut,
                Some(format!("timed out after {} ms", timeout.as_millis())),
                false,
            )
        }
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
        let running = entries
            .values()
            .filter(|entry| !entry.status.is_terminal())
            .count();
        if running >= MAX_RUNNING {
            return Err(Error::Tool(format!(
                "concurrency cap reached ({MAX_RUNNING} running subagents)"
            ))
            .into());
        }
        let id = loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            if !entries.contains_key(&id) {
                break id;
            }
        };
        // Tag the child's jobs with the run id so a kill or a session
        // switch cancels exactly its process tree.
        agent.job_owner = Some(id.clone());
        let (state_tx, _) = watch::channel(Status::Running);
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
                // Born 0 would collide with the absent page cursor.
                born: registry.next_born.fetch_add(1, Ordering::SeqCst) + 1,
                outcome: None,
                state_tx,
                abort: None,
                events: EventLog::default(),
                notify_terminal: true,
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
    // The previous run's outcome and abort handle belong to that run; the
    // new run stores its own. Waiters see the state move back to running.
    entry.outcome = None;
    entry.abort = None;
    let _ = entry.state_tx.send(Status::Running);
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
