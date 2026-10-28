//! In-process subagents: a `lofi.subagent` hook that runs a child backend turn
//! on a forked message history and returns the child's final text to the
//! caller.
//!
//! The sandbox exposes the hook as `lofi.subagent`. Each spawn registers an
//! entry in the session's [`SubagentRegistry`]; passing `id` resumes that
//! entry with its stored history instead of starting fresh.
//!
//! A child is an ordinary backend from the same factory as its parent, so a
//! CLI parent spawns an HTTP child and the reverse. Nothing here branches on
//! the transport.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lofi_code::{SubagentFn, SubagentOp};
use lofi_error::{Error, Result};
use lofi_types::{
    Config, ContentBlock, Message, PromptKind, Role, SessionEvent, SessionEventKind, ThinkingLevel,
};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::AbortHandle;

use super::mailbox::{Mailbox, MailboxScope, PARENT_ADDRESS};
use super::{lock, Agent, AgentEvent, RunControls, ToolExecutor, TurnRequest};
use crate::backend::{Backend, BackendFactory, BackendRef};
use crate::models::ModelRegistry;
use crate::session::recorder::TurnOutcome;
use crate::session::store::{SessionCursor, SessionFile, SessionStore};

/// Maximum time a subagent turn may run when `timeout_ms` is not set.
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// Cap on the `result` text returned to the parent when `output_limit` is
/// not set. Also the retention cap: the registry entry and the transcript
/// settle marker keep at most this much of the result and error text, so
/// one idle entry never holds more than a few KB.
const DEFAULT_OUTPUT_LIMIT: usize = 32 * 1024;
/// Hard bound on the serialized settle-marker line. `record_settled` cuts
/// the result until the line fits, so the rehydration tail window always
/// contains the whole marker.
const SETTLED_MARKER_LIMIT: usize = 128 * 1024;
/// Tail bytes scanned for the settle marker at rehydration: twice the
/// marker limit, so the marker line always starts inside the window.
const SETTLED_TAIL_WINDOW: u64 = 2 * SETTLED_MARKER_LIMIT as u64;
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
/// Cap on the outcome line quoted in a terminal notice: the notice becomes
/// a parent prompt, so it stays short.
const NOTICE_DETAIL_CAP: usize = 200;
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

    /// Restore from a persisted settle marker. An unknown status (written by
    /// a newer build) degrades to failed rather than blocking the resume.
    fn from_marker(status: &str) -> Self {
        match status {
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            "timed_out" => Self::TimedOut,
            _ => Self::Failed,
        }
    }
}

struct Entry {
    status: Status,
    backend: BackendRef,
    messages: Vec<Message>,
    /// False only for entries rehydrated from a transcript after a restart:
    /// their history replays from the cursor on the first resume, so a
    /// session start does not pay for histories nobody resumes.
    history_loaded: bool,
    cancel: Arc<AtomicBool>,
    // Retained so the entry outlives a timed-out or abandoned run; reused on
    // the next resume.
    cursor: Option<SessionCursor>,
    /// Transcript of a rehydrated entry whose cursor is not open yet; the
    /// first resume opens it and fills `cursor`. Opening at rehydrate would
    /// index every transcript at session start for entries nobody resumes.
    transcript_path: Option<PathBuf>,
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
    // Terminal notice switch; on by default for background runs, cleared by
    // `subagentNotify({enabled: false})` and by `clear()`.
    notify_terminal: bool,
    // Generation of the periodic-notice loop: each `subagentNotify` with an
    // interval bumps it so a stale loop exits instead of doubling up.
    notify_tick: u64,
    // Handle of the active periodic-notice loop; reconfiguration aborts it
    // instead of leaving it asleep until its old interval expires.
    notify_task: Option<AbortHandle>,
    // Set when rehydration could not restore the transcript's model: the
    // entry keeps the session model instead. Surfaced by status and list.
    note: Option<String>,
    // Reset at every claim (spawn and resume): the parent's status line
    // shows a running run's elapsed time and task.
    started: std::time::Instant,
    task: String,
    // Set at spawn, cleared at resume: the parent's status line shows only
    // background runs; a foreground run already blocks the parent's exec
    // block, so listing it there would duplicate it.
    background: bool,
}

/// One running subagent, surfaced on the parent's status line.
pub struct SubagentRunning {
    pub id: String,
    pub elapsed_ms: u64,
    pub task: String,
}

/// One-line task summary for status surfaces: the first line, bounded so a
/// pasted file body cannot flood a status line.
fn task_preview(prompt: &str) -> String {
    const MAX: usize = 80;
    let first = prompt.lines().next().unwrap_or("").trim();
    let mut out: String = first.chars().take(MAX).collect();
    if first.chars().count() > MAX {
        out.push('\u{2026}');
    }
    out
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
            if seq < cursor {
                continue;
            }
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
    pub kind: SubagentNoticeKind,
}

/// Why a notice fired. The host reacts in kind: a settled run gets a turn
/// that reports it, while a mailbox wake must not end a turn that is
/// already running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentNoticeKind {
    /// A run reached a terminal state, or is still running.
    Run,
    /// A message landed in the parent inbox.
    Mailbox,
}

/// Per-session store of spawned subagents, shared across model switches.
#[derive(Clone)]
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
    /// Runtime that owns background runs. Captured at construction: the
    /// agent is built on the app's long-lived runtime, while a turn may
    /// run on a per-turn worker runtime that dies when the turn ends.
    /// `set_host_handle` remains as the fallback for registries built
    /// outside any runtime.
    host_handle: Arc<std::sync::OnceLock<tokio::runtime::Handle>>,
    /// Terminal and progress notices for the host UI.
    notices: Arc<NoticeHub>,
    /// Parent session id whose transcripts were already rehydrated into
    /// entries. Restart restore runs once per parent; `clear()` resets it.
    rehydrated_for: Arc<Mutex<Option<String>>>,
    /// Parent/child messaging. Shared by every child exec, which reaches it
    /// through a scoped capability rather than this registry.
    pub(crate) mailbox: Mailbox,
}

impl Default for SubagentRegistry {
    fn default() -> Self {
        let host_handle = Arc::new(std::sync::OnceLock::new());
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = host_handle.set(handle);
        }
        let notices = Arc::new(NoticeHub::default());
        // A child holds only a mailbox scope, so it cannot publish a notice
        // itself: the mailbox wakes the host through this hub.
        let mailbox = Mailbox::default().with_sink({
            let notices = notices.clone();
            Arc::new(move |sender: &str| notices.publish_mailbox(sender))
        });
        Self {
            entries: Arc::default(),
            next_used: Arc::default(),
            unreported: Arc::default(),
            parent_session: Arc::default(),
            generation: Arc::default(),
            next_born: Arc::default(),
            host_handle,
            notices,
            rehydrated_for: Arc::default(),
            mailbox,
        }
    }
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

/// Pointer text for a mailbox wake. One wake covers every sender, so the
/// text names the oldest one without claiming it is the only one. It carries
/// no body: it becomes a prompt, and a child's words must reach the parent as
/// tool output the parent asked for, never as an instruction.
fn mailbox_notice_text(sender: &str) -> String {
    format!(
        "subagent {sender} sent a message; drain the parent inbox with lofi.subagentMailboxReceive"
    )
}

impl NoticeHub {
    fn publish_mailbox(&self, sender: &str) {
        let _ = self.tx.send(SubagentNotice {
            id: sender.to_string(),
            text: mailbox_notice_text(sender),
            kind: SubagentNoticeKind::Mailbox,
        });
    }
}

impl SubagentRegistry {
    /// Notice stream for the host UI. Re-subscribe after a session rebuild;
    /// a lagging receiver skips to the newest notices.
    #[must_use]
    pub fn subscribe_notices(&self) -> broadcast::Receiver<SubagentNotice> {
        self.notices.tx.subscribe()
    }

    /// Whether a wake for the parent inbox is still untaken. A notice that
    /// arrives after the wake was spent must not wake the host again.
    #[must_use]
    pub fn mailbox_wake_unclaimed(&self) -> bool {
        self.mailbox.parent_wake_unclaimed()
    }

    /// Take the parent wake for a turn the host is about to start. False for
    /// a spent wake, and for mail a running turn drained itself.
    #[must_use]
    pub fn claim_mailbox_wake(&self) -> bool {
        self.mailbox.claim_parent_wake()
    }

    /// The wake a broadcast lag may have dropped: one the host never took,
    /// for mail that is still queued. A taken wake is not rebuilt, because
    /// the turn it started may have been cancelled with the mail unread.
    #[must_use]
    pub fn lost_mailbox_wake(&self) -> Option<String> {
        if !self.mailbox.parent_wake_unclaimed() {
            return None;
        }
        self.mailbox
            .parent_inbox_front()
            .map(|sender| mailbox_notice_text(&sender))
    }

    fn publish_notice(&self, id: &str, text: String) {
        let _ = self.notices.tx.send(SubagentNotice {
            id: id.to_string(),
            text,
            kind: SubagentNoticeKind::Run,
        });
    }

    /// Record the runtime that owns background runs when construction
    /// could not (built outside a runtime). First-wins: a turn may run on
    /// a per-turn worker runtime, so a later call must not replace the
    /// construction-time handle.
    pub(crate) fn set_host_handle(&self, handle: tokio::runtime::Handle) {
        let _ = self.host_handle.set(handle);
    }

    /// Whether both registries share one store (clones of one instance).
    /// A model switch that rebuilds the agent may keep the registry; the
    /// TUI must not cancel its live runs or re-subscribe then.
    #[must_use]
    pub fn same_instance(&self, other: &SubagentRegistry) -> bool {
        Arc::ptr_eq(&self.entries, &other.entries)
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

    /// Restore entries from this session's subagent transcripts so runs
    /// stay resumable across a process restart. Runs once per parent
    /// session id (the turn loop calls it at turn start); `clear()` resets
    /// the marker so a later /resume of the same parent re-runs it.
    ///
    /// Memory: reads only the header (via the listing) and a bounded tail
    /// window per file. Histories replay lazily on resume.
    pub fn rehydrate(&self, template: &dyn Backend, support: &SubagentSupport) {
        if !support.persist_sessions {
            return;
        }
        let Some(parent) = self.parent_session() else {
            return;
        };
        if lock(&self.rehydrated_for).as_deref() == Some(parent.as_str()) {
            return;
        }
        let generation = self.generation();
        let files = SessionStore::open().and_then(|store| {
            // Attach before restoring entries: a resumed session must replay
            // its own journal before the first spawn, or sends taken before
            // that point reuse sequence numbers the journal already holds.
            attach_mailbox_journal(self, &store);
            store.list_subagent_files_for_cwd(&support.cwd, &parent)
        });
        let files = match files {
            Ok(files) => files,
            Err(e) => {
                tracing::debug!(error = %e, "subagent rehydration skipped");
                return;
            }
        };
        let mut restored = Vec::new();
        // The listing is newest first: select within the cap, then build
        // oldest first so born order stays chronological.
        for file in files.into_iter().take(MAX_ENTRIES).rev() {
            let pair = restore_entry(template, support, self, &file);
            restored.push(pair);
        }
        let mut entries = lock(&self.entries);
        // A clear() during the scan switched sessions; these entries belong
        // to the old parent.
        if self.generation() != generation {
            return;
        }
        for (id, entry) in restored {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            // A spawn or a duplicate file with the same id wins over the
            // restored entry.
            let std::collections::hash_map::Entry::Vacant(slot) = entries.entry(id) else {
                continue;
            };
            // Every restored run is terminal: its address rejects sends, and a
            // message left pending by the replay stays visible as undelivered.
            // The resume claim reopens it. The close stays here, so it never
            // touches an address a live entry already owns.
            self.mailbox.close(slot.key(), entry.status.as_str());
            slot.insert(entry);
        }
        drop(entries);
        *lock(&self.rehydrated_for) = Some(parent);
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
                entry.backend.tool_executor().jobs().kill_owned(id);
                if let Some(abort) = &entry.abort {
                    abort.abort();
                }
            }
            entries.clear();
        }
        self.mailbox.close_children("session cleared");
        let mut unreported = lock(&self.unreported);
        let usage = std::mem::take(&mut *unreported);
        *lock(&self.rehydrated_for) = None;
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
        (u.0 > 0.0
            || u.1.input_tokens > 0
            || u.1.output_tokens > 0
            || u.1.cache_read_tokens > 0
            || u.1.cache_write_tokens > 0)
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

    /// Currently running background runs, for the parent's status line.
    /// Foreground runs are excluded: the parent's exec block already shows
    /// them. Sorted by start time so the status line does not reshuffle
    /// between frames.
    #[must_use]
    pub fn running(&self) -> Vec<SubagentRunning> {
        let mut running: Vec<_> = lock(&self.entries)
            .iter()
            .filter(|(_, e)| matches!(e.status, Status::Running) && e.background)
            .map(|(id, e)| (e.started, id.clone(), e.task.clone()))
            .collect();
        running.sort_by_key(|(started, _, _)| *started);
        running
            .into_iter()
            .map(|(started, id, task)| SubagentRunning {
                id,
                elapsed_ms: started.elapsed().as_millis() as u64,
                task,
            })
            .collect()
    }

    /// Whether a background run is still active. The tick loop polls this
    /// on every tick, so it must not copy the task strings the way
    /// [`Self::running`] does.
    #[must_use]
    pub fn has_running_background(&self) -> bool {
        lock(&self.entries)
            .values()
            .any(|e| matches!(e.status, Status::Running) && e.background)
    }

    /// Whether `id` names a run that is still active.
    #[must_use]
    pub fn is_running(&self, id: &str) -> bool {
        lock(&self.entries)
            .get(id)
            .is_some_and(|e| matches!(e.status, Status::Running))
    }

    #[cfg(test)]
    pub(crate) fn ids(&self) -> Vec<String> {
        lock(&self.entries).keys().cloned().collect()
    }

    #[cfg(test)]
    pub(crate) fn backend_of(&self, id: &str) -> Option<BackendRef> {
        lock(&self.entries).get(id).map(|e| e.backend.clone())
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
    /// Environment specs the session was opened with, so a child that
    /// overrides its model to another provider resolves that provider's
    /// credentials the same way.
    env: Vec<(String, String)>,
    /// Skill directory next to the session's config file.
    skills_dir: Option<PathBuf>,
}

impl SubagentSupport {
    #[must_use]
    pub fn new(registry: ModelRegistry, config: Config, cwd: PathBuf) -> Self {
        Self {
            registry,
            config,
            cwd,
            persist_sessions: true,
            env: Vec::new(),
            skills_dir: None,
        }
    }

    #[must_use]
    pub fn with_persistence(mut self, persist_sessions: bool) -> Self {
        self.persist_sessions = persist_sessions;
        self
    }

    #[must_use]
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub fn with_skills_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.skills_dir = dir;
        self
    }

    /// The factory a model override opens its child through. Built on demand:
    /// a spawn without an override keeps the template's own selection and
    /// never resolves another provider.
    ///
    /// The factory carries this catalog, so an override can name a model the
    /// provider discovered at startup.
    fn factory(&self) -> Result<BackendFactory> {
        Ok(
            BackendFactory::new(self.config.clone(), &self.cwd, self.env.clone())?
                .with_resolved_registry(self.registry.clone())
                .with_skills_dir(self.skills_dir.clone())
                .with_subagent_persistence(self.persist_sessions),
        )
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
/// it; on the kill path the guard is what settles the entry and reports
/// its spend. All work is synchronous: Drop cannot await.
struct SettleOnDrop {
    registry: SubagentRegistry,
    id: String,
    cursor: Option<SessionCursor>,
    pre_run_leaf: Option<String>,
    cancel: Arc<AtomicBool>,
    // Spend collected by the event drain. An aborted run never reaches the
    // report at the end of `run_turn_with_timeout`, so the guard folds in
    // what the drain saw instead of losing it.
    totals: Arc<std::sync::Mutex<ChildUsage>>,
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
        {
            let entries = lock(&self.registry.entries);
            let Some(entry) = entries.get(&self.id) else {
                return;
            };
            if entry.status.is_terminal() {
                return;
            }
        }
        let status = if self.cancel.load(Ordering::SeqCst) {
            Status::Cancelled
        } else {
            Status::Failed
        };
        // Rollback and spend reporting happen while the entry is still
        // running — a resume is rejected in that state, so no new run can
        // claim the transcript head this rollback is about to rewind. Both
        // do blocking store I/O, so they stay outside the entries lock.
        if let Some(cursor) = self.cursor.as_ref() {
            if let Err(e) = cursor.restore_branch(self.pre_run_leaf.clone()) {
                tracing::debug!(error = %e, "subagent transcript rollback failed");
            }
        }
        record_settled(
            self.cursor.as_ref(),
            status,
            None,
            Some(if status == Status::Cancelled {
                "cancelled"
            } else {
                "subagent task ended unexpectedly"
            }),
            &lock(&self.totals),
        );
        // The ended run never reached the report at the end of
        // run_turn_with_timeout; fold in what the drain saw. The generation
        // check happens under the lock, as in the normal reporting path, so
        // a concurrent clear() cannot split them.
        {
            let mut unreported = lock(&self.registry.unreported);
            if self.registry.generation() == self.generation {
                lock(&self.totals).report(&mut unreported);
            }
        }
        // Usage the drain collected before the abort. Buffered events may
        // still flush after this read — the drain task cannot be awaited
        // from Drop — so the figure can be short by one round.
        let usage = lock(&self.totals).json();
        let mut entries = lock(&self.registry.entries);
        // clear() may have removed the entry during the unlocked section.
        let Some(entry) = entries.get_mut(&self.id) else {
            return;
        };
        let mut outcome = json!({
            "ok": false,
            "id": self.id,
            "status": status.as_str(),
            "error": if status == Status::Cancelled {
                "cancelled"
            } else {
                "subagent task ended unexpectedly"
            },
        });
        if let Some(usage) = usage {
            outcome["usage"] = usage;
        }
        entry.status = status;
        // Restamp like the normal settle does: an aborted run that keeps
        // its start stamp becomes the oldest eviction victim, and a caller
        // reading the kill result right after could lose it.
        entry.used = self.registry.stamp();
        entry.backend = entry
            .backend
            .with_tool_executor(entry.backend.tool_executor().reset_exec_worker());
        entry.outcome = Some(outcome);
        if entry.cursor.is_some() {
            // The claim moved the history into the run, which died with it;
            // the rolled-back transcript still holds it.
            entry.messages = Vec::new();
            entry.history_loaded = false;
        }
        entry
            .events
            .push("state", json!({ "status": status.as_str() }));
        // send, not send_replace, would drop the value when no waiter is
        // subscribed: a wait started after the settle would hang.
        entry.state_tx.send_replace(status);
        // The close belongs to this critical section: a waiter that sees the
        // terminal status may claim a resume at once, and a close issued
        // after the lock was released would shut the fresh run's address.
        self.registry.mailbox.close(&self.id, status.as_str());
        let notify = entry.notify_terminal;
        drop(entries);
        if notify && self.registry.generation() == self.generation {
            self.registry.publish_notice(
                &self.id,
                format!("subagent {} {}", self.id, status.as_str()),
            );
        }
    }
}

/// The run-management hook of one backend. Every child is an ordinary
/// backend forked from this template, so the parent's transport does not
/// constrain the child's.
pub(crate) fn backend_hook(
    template: BackendRef,
    support: SubagentSupport,
    registry: SubagentRegistry,
) -> SubagentFn {
    Arc::new(move |op: SubagentOp, args: Value| {
        let template = template.clone();
        let support = support.clone();
        let registry = registry.clone();
        Box::pin(async move { dispatch(template, support, registry, op, args).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>
    })
}

impl Agent {
    /// Build the `lofi.subagent*` hook for this agent, or `None` when the
    /// agent has no subagent support (nested subagent agents).
    #[must_use]
    pub fn subagent_hook(&self) -> Option<SubagentFn> {
        let Some(support) = self.subagent_support.clone() else {
            // A child cannot manage runs, but it keeps the mailbox: the hook
            // answers the three mailbox ops and reports every other op as
            // unavailable.
            return self.mailbox.clone().map(mailbox_hook);
        };
        Some(backend_hook(
            Arc::new(self.clone()),
            support,
            self.subagents.clone(),
        ))
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

/// Point the mailbox at this store's journal for the parent session, so a
/// resumed session replays only its own conversation.
fn attach_mailbox_journal(registry: &SubagentRegistry, store: &SessionStore) {
    let name = registry
        .parent_session()
        .unwrap_or_else(|| PARENT_ADDRESS.to_string());
    registry
        .mailbox
        .attach_journal(&store.root().join(format!("mailbox-{name}.jsonl")));
}

/// Hook for a child agent: mailbox only, scoped to the child's own address.
fn mailbox_hook(scope: MailboxScope) -> SubagentFn {
    Arc::new(move |op: SubagentOp, args: Value| {
        let scope = scope.clone();
        Box::pin(async move {
            match op {
                SubagentOp::MailboxSend => scope.mailbox.send(&scope.me, false, &args),
                SubagentOp::MailboxReceive => scope.mailbox.receive(&scope.me, &args).await,
                SubagentOp::MailboxList => scope.mailbox.list(&scope.me, false, &args),
                _ => json!({
                    "ok": false,
                    "status": "unavailable",
                    "error": "subagent is not available in this context",
                }),
            }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Value> + Send>>
    })
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
    template: BackendRef,
    support: SubagentSupport,
    registry: SubagentRegistry,
    op: SubagentOp,
    args: Value,
) -> Value {
    match op {
        SubagentOp::Run => run(template, support, registry, args).await,
        SubagentOp::Spawn => spawn(&template, &support, &registry, &args).await,
        SubagentOp::Status => status_op(&registry, &args),
        SubagentOp::List => list_op(&registry, &args),
        SubagentOp::Wait => wait_op(&registry, args).await,
        SubagentOp::Read => read_op(&registry, &args),
        SubagentOp::Kill => kill_op(&registry, &args),
        SubagentOp::Notify => notify_op(&registry, &args),
        // The parent addresses runs by id; a child reaches the same store
        // through the scoped hook built below.
        SubagentOp::MailboxSend => registry.mailbox.send(PARENT_ADDRESS, true, &args),
        SubagentOp::MailboxReceive => registry.mailbox.receive(PARENT_ADDRESS, &args).await,
        SubagentOp::MailboxList => registry.mailbox.list(PARENT_ADDRESS, true, &args),
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
    template: BackendRef,
    support: SubagentSupport,
    registry: SubagentRegistry,
    args: Value,
) -> Value {
    // Foreground runs return their outcome to the caller directly: a
    // terminal notice on top would queue a duplicate as a parent prompt.
    let started = match start(&template, &support, &registry, &args, false).await {
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
async fn spawn(
    template: &BackendRef,
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
    let started = match start(template, support, registry, args, true).await {
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
    if let Some(session_id) = entry_session_id(entry) {
        out["session_id"] = json!(session_id);
    }
    if let Some(note) = &entry.note {
        out["note"] = json!(note);
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
            if let Some(session_id) = entry_session_id(entry) {
                row["session_id"] = json!(session_id);
            }
            if let Some(note) = &entry.note {
                row["note"] = json!(note);
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
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(item) => out.push(item.to_string()),
                    None => {
                        return json!({
                            "ok": false,
                            "status": "failed",
                            "error": "types must be an array of strings",
                        })
                    }
                }
            }
            Some(out)
        }
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
        (entry.backend.tool_executor().jobs(), entry.abort.clone())
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
    // Any reconfiguration retires the previous loop at once instead of
    // leaving it asleep until its old interval expires.
    entry.notify_tick += 1;
    if let Some(task) = entry.notify_task.take() {
        task.abort();
    }
    if let Some(interval) = interval {
        let tick = entry.notify_tick;
        if entry.status == Status::Running && entry.notify_terminal {
            let registry = registry.clone();
            let loop_id = id.clone();
            let task = registry.host_handle().spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    let active = {
                        let entries = lock(&registry.entries);
                        entries.get(&loop_id).is_some_and(|entry| {
                            entry.status == Status::Running
                                && entry.notify_terminal
                                && entry.notify_tick == tick
                        })
                    };
                    if !active {
                        break;
                    }
                    registry.publish_notice(&loop_id, format!("subagent {loop_id} still running"));
                }
            });
            entry.notify_task = Some(task.abort_handle());
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
async fn start(
    template: &BackendRef,
    support: &SubagentSupport,
    registry: &SubagentRegistry,
    args: &Value,
    notify: bool,
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
    let (backend, messages, cursor, cancel, id) =
        prepare(template, support, registry, id, args, notify).await?;
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
            entry
                .events
                .push("prompt", json!({ "text": cap_event_text(&prompt) }));
        }
    }
    let handle = {
        let registry = registry.clone();
        let id = id.clone();
        let cancel = cancel.clone();
        let task_session_id = session_id.clone();
        registry.host_handle().spawn(async move {
            run_task(
                backend,
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
    backend: BackendRef,
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
    let totals = Arc::new(std::sync::Mutex::new(ChildUsage::default()));
    let guard = SettleOnDrop {
        registry: registry.clone(),
        id: id.clone(),
        cursor: cursor.clone(),
        pre_run_leaf: pre_run_leaf.clone(),
        cancel: cancel.clone(),
        totals: totals.clone(),
        generation,
        armed: true,
    };
    let outcome = run_turn_with_timeout(
        backend,
        messages,
        cursor.clone(),
        cancel,
        registry.clone(),
        id.clone(),
        base_len,
        pre_run_leaf,
        timeout,
        prompt,
        output_limit,
        token_budget,
        totals.clone(),
        generation,
    )
    .await;
    let status = outcome.status;
    // Retention cap: the kept body bounds result and error so an idle entry
    // holds KBs regardless of the caller's output_limit. The full answer
    // stays in the transcript.
    let mut retained = outcome;
    if let Some(result) = retained.result.as_mut() {
        if result.len() > DEFAULT_OUTPUT_LIMIT {
            retained.full_bytes = Some(retained.full_bytes.unwrap_or(result.len()));
            *result = cap_at_boundary(result, DEFAULT_OUTPUT_LIMIT);
        }
    }
    if let Some(error) = retained.error.as_mut() {
        *error = cap_at_boundary(error, DEFAULT_OUTPUT_LIMIT);
    }
    let out = outcome_json(&id, session_id.as_deref(), retained);
    // Durable terminal record: after a restart the rehydration tail scan
    // restores exactly this status and outcome.
    let persisted = record_settled(
        cursor.as_ref(),
        status,
        out["result"].as_str(),
        out["error"].as_str(),
        &lock(&totals),
    );
    let notice_text = terminal_notice(&id, status, &out);
    // Terminal write in one critical section: a resume observes either
    // the running entry or the fully settled one, never half of either.
    let notify = {
        let mut entries = lock(&registry.entries);
        // Close inside this critical section: a send racing the settle sees
        // either a live recipient or a rejected one, a child blocked in
        // receive wakes with the closed result, and a resume claimed the
        // moment the terminal status is visible finds the inbox already shut.
        registry.mailbox.close(&id, status.as_str());
        match entries.get_mut(&id) {
            Some(entry) => {
                entry.status = status;
                entry.outcome = Some(out);
                entry
                    .events
                    .push("state", json!({ "status": status.as_str() }));
                entry.state_tx.send_replace(status);
                if persisted && entry.cursor.is_some() {
                    // The transcript holds the full history; an idle entry
                    // retains only the outcome. The next resume replays
                    // from the transcript.
                    entry.messages = Vec::new();
                    entry.history_loaded = false;
                }
                entry.notify_terminal
            }
            // clear() removed the entry: nothing to record, nothing to say.
            None => false,
        }
    };
    if notify && registry.generation() == generation {
        registry.publish_notice(&id, notice_text);
    }
    guard.disarm();
}

/// One-line terminal notice: status plus the first line of the outcome.
fn terminal_notice(id: &str, status: Status, outcome: &Value) -> String {
    let detail = outcome["result"]
        .as_str()
        .or_else(|| outcome["error"].as_str())
        .and_then(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        // A detail that only repeats the status adds nothing.
        .filter(|line| *line != status.as_str());
    match detail {
        Some(line) => format!(
            "subagent {id} {}: {}",
            status.as_str(),
            cap_at_boundary(line, NOTICE_DETAIL_CAP)
        ),
        None => format!("subagent {id} {}", status.as_str()),
    }
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

/// Append the run's terminal record as the leaf of the transcript so a
/// rehydrate after a restart restores this exact status and outcome instead
/// of inferring them from the transcript tail. Runs after any rollback, so
/// the marker is the physical last line of the file: the rehydration tail
/// scan relies on that. The serialized line is bounded by
/// [`SETTLED_MARKER_LIMIT`] so the tail window always contains it whole.
/// Returns whether the marker was persisted; an idle entry keeps its history
/// in memory when the transcript cannot reproduce it.
fn record_settled(
    cursor: Option<&SessionCursor>,
    status: Status,
    result: Option<&str>,
    error: Option<&str>,
    totals: &ChildUsage,
) -> bool {
    let Some(cursor) = cursor else { return false };
    let (usage, cost) = if totals.seen {
        (
            Some(lofi_types::Usage {
                input_tokens: totals.input,
                output_tokens: totals.output,
                cache_read_tokens: totals.cache_read,
                cache_write_tokens: totals.cache_write,
            }),
            Some(totals.cost),
        )
    } else {
        (None, None)
    };
    // Callers already cap both texts at DEFAULT_OUTPUT_LIMIT; the measure
    // loop is the hard guarantee that escaping and multi-byte UTF-8 cannot
    // push the serialized line past the tail window.
    let mut result = result.map(|text| cap_at_boundary(text, DEFAULT_OUTPUT_LIMIT));
    let error = error.map(|text| cap_at_boundary(text, DEFAULT_OUTPUT_LIMIT));
    let build = |result: &Option<String>| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::RunSettled {
            status: status.as_str().to_string(),
            result: result.clone(),
            error: error.clone(),
            usage,
            cost,
        },
    };
    let mut event = build(&result);
    while let Some(text) = result.as_ref() {
        let Ok(line) = serde_json::to_string(&event) else {
            break;
        };
        if line.len() <= SETTLED_MARKER_LIMIT || text.is_empty() {
            break;
        }
        result = Some(cap_at_boundary(text, text.len() / 2));
        event = build(&result);
    }
    let mut events = vec![event];
    if let Err(e) = cursor.append_events(&mut events) {
        tracing::debug!(error = %e, "subagent settle marker not recorded");
        return false;
    }
    true
}

/// A subagent run's terminal record as persisted by the `RunSettled` marker.
struct SettledRun {
    status: Status,
    result: Option<String>,
    error: Option<String>,
    usage: Option<Value>,
}

/// Restore the terminal record from the transcript tail. The marker is the
/// physical last line of a settled run; anything else at the tail — turn
/// events of a newer run, a torn final line without a newline — means the
/// process exited before the last run settled.
fn read_settled_marker(path: &Path) -> Option<SettledRun> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(SETTLED_TAIL_WINDOW)))
        .ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    // A final line without its newline is a torn write, not a marker.
    if !buf.ends_with(b"\n") {
        return None;
    }
    let text = String::from_utf8(buf).ok()?;
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let event = crate::session::store::parse_event(line).ok()?;
    if let SessionEventKind::RunSettled {
        status,
        result,
        error,
        usage,
        cost,
    } = event.kind
    {
        let usage = usage.map(|usage| {
            json!({
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_read_tokens": usage.cache_read_tokens,
                "cache_write_tokens": usage.cache_write_tokens,
                "cost": cost.unwrap_or(0.0),
            })
        });
        Some(SettledRun {
            status: Status::from_marker(&status),
            result,
            error,
            usage,
        })
    } else {
        None
    }
}

/// Drain the child event stream into usage totals and the entry's public event
/// log. Sender clones escape the turn through tool contexts, so the channel may
/// never close: the caller stops the drain explicitly and the task flushes what
/// is buffered before exiting.
fn spawn_event_drain(
    mut rx: mpsc::Receiver<AgentEvent>,
    totals: Arc<std::sync::Mutex<ChildUsage>>,
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
            if let AgentEvent::RoundUsage { cost, usage } = event {
                let spent = {
                    let mut totals = lock(&totals);
                    totals.add(*cost, usage);
                    totals.total()
                };
                // Cancel as soon as the budget is crossed; the settle path
                // reports the breach as the failure reason.
                if token_budget.is_some_and(|budget| spent > budget) {
                    cancel.store(true, Ordering::SeqCst);
                }
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

/// Truncate at `cap` bytes, backing up to a UTF-8 boundary:
/// `String::truncate` panics off one.
fn cap_at_boundary(text: &str, cap: usize) -> String {
    let mut text = text.to_string();
    if text.len() > cap {
        let mut end = cap;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn cap_event_text(text: &str) -> String {
    cap_at_boundary(text, EVENT_TEXT_CAP)
}

/// Run one turn to completion and finalize the registry entry: status,
/// stored history, transcript rollback on timeout, and the sandbox thread of
/// an idle entry.
#[allow(clippy::too_many_arguments)]
async fn run_turn_with_timeout(
    backend: BackendRef,
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
    // Created by the caller so the settle guard can report an aborted run's
    // partial spend; the event drain fills it as RoundUsage events arrive.
    totals: Arc<std::sync::Mutex<ChildUsage>>,
    // Captured at admission: a /new or /resume bumps it, and a detached
    // settle must not fold spend into the new session.
    generation: u64,
) -> RunOutcome {
    let (tx, rx) = mpsc::channel::<AgentEvent>(256);
    // The child events have no consumer: the parent transcript records the
    // tool call, not the child stream. Collect the usage so the spend is
    // still visible in the tool result; drain the rest.
    let (stop_tx, drain) = spawn_event_drain(
        rx,
        totals.clone(),
        cancel.clone(),
        token_budget,
        registry.clone(),
        id.clone(),
    );
    let turn = backend.run_turn(
        &mut messages,
        // The task is parent-written, so it must not read as user
        // authorization in the child's auto-mode context.
        TurnRequest::prompt(prompt, PromptKind::Task),
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
        let spent = totals.total();
        token_budget
            .filter(|&budget| spent > budget)
            .map(|budget| (spent, budget))
    };
    let (status, error, keep) = classify_outcome(
        outcome,
        budget_over,
        &cancel,
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
    // The terminal status is NOT set here: `run_task` writes status,
    // outcome, event, and the watch state in one critical section. Setting
    // it early would let a resume claim the entry in between and this
    // run's stale outcome would then land on the new run's waiters.
    let entry_present = if let Some(entry) = lock(&registry.entries).get_mut(&id) {
        // Restamp at settle: the stamp orders eviction, and a long run that
        // finishes last must not become the oldest victim.
        entry.used = registry.stamp();
        entry.messages = messages;
        // Idle entries must not hold a sandbox thread: it restarts
        // lazily on the first exec of the next resume.
        entry.backend = entry
            .backend
            .with_tool_executor(entry.backend.tool_executor().reset_exec_worker());
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
fn classify_outcome(
    outcome: std::result::Result<Result<TurnOutcome>, tokio::time::error::Elapsed>,
    budget_over: Option<(u64, u64)>,
    cancel: &Arc<AtomicBool>,
    cursor: Option<&SessionCursor>,
    pre_run_leaf: Option<String>,
    timeout: Duration,
) -> (Status, Option<String>, bool) {
    match outcome {
        Ok(Ok(outcome)) => {
            if let Some((spent, budget)) = budget_over {
                (
                    Status::Failed,
                    Some(format!("token budget exceeded ({spent} > {budget})")),
                    true,
                )
            } else {
                match outcome {
                    TurnOutcome::Finished => (Status::Completed, None, true),
                    TurnOutcome::Cancelled | TurnOutcome::Detached => {
                        (Status::Cancelled, Some("cancelled".to_string()), true)
                    }
                    // A pressure turn stops with the window full: a resume does
                    // not compact the child history, so it would stop again.
                    TurnOutcome::ContextPressure => (
                        Status::Failed,
                        Some("context window is full".to_string()),
                        true,
                    ),
                    // A failed turn settles as `Err` from the turn session, so
                    // this arm is a guard on that contract, not a live path.
                    TurnOutcome::Failed(error) => (Status::Failed, Some(error), true),
                }
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

    /// Total tokens across every category the provider reports.
    fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
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

/// Give a child its own turn context and its own evaluator. A clone of the
/// template shares the handle the parent evaluator captured, so without this
/// a child reads the session's context and its own turns overwrite it.
pub(super) fn isolate_auto_mode(executor: &mut ToolExecutor, support: &SubagentSupport) {
    executor.auto_mode_hints = super::auto_mode::hint_handle();
    if support.config.shell_policy.auto_mode.is_none() {
        executor.auto_mode = None;
        executor.auto_mode_config = None;
        return;
    }
    // The session config is unresolved and its provider table is pruned to the
    // model's own provider, so the child takes the evaluator config its parent
    // opened: same catalog entry, same resolved credentials.
    let Some(config) = executor.auto_mode_config.clone() else {
        executor.auto_mode = None;
        return;
    };
    let mut child = executor.clone();
    match super::auto_mode::attach(&mut child, &config, &support.registry) {
        Ok(()) => {
            executor.auto_mode = child.auto_mode;
            executor.auto_mode_config = child.auto_mode_config;
        }
        Err(error) => {
            tracing::warn!(%error, "subagent: auto-mode disabled for the child");
            executor.auto_mode = None;
            executor.auto_mode_config = None;
        }
    }
}

/// The child's execution state: the session's jobs, policy, truncation and
/// sandbox configuration stay shared; the sandbox thread, the auto-mode
/// context and the run-management hook are the child's own.
/// The user request a child inherits: the request its parent inherited
/// (a nested child), else the parent session's current request. Read from
/// the parent's hints handle, which the turn session fills from user input,
/// so a spawn call can never supply it.
pub(super) fn inherited_request(template: &dyn Backend) -> Option<String> {
    let executor = template.tool_executor();
    let hints = executor
        .auto_mode_hints
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hints.root_request.clone().or_else(|| hints.prompt.clone())
}

pub(super) fn child_executor(
    template: &dyn Backend,
    support: &SubagentSupport,
    root_request: Option<String>,
) -> ToolExecutor {
    let mut executor = template
        .tool_executor()
        .reset_exec_worker()
        .with_fresh_auto_mode_hints();
    isolate_auto_mode(&mut executor, support);
    // Seed the inherited request into the handle the turn loop preserves:
    // the child's own history holds only its task, so a later turn cannot
    // recover the request the user typed in the parent session. Seeded after
    // `isolate_auto_mode`, which hands the child a fresh handle.
    if let Some(request) = root_request {
        *executor
            .auto_mode_hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = lofi_code::AutoModeHints {
            root_request: Some(request),
            ..lofi_code::AutoModeHints::default()
        };
    }
    // A child cannot manage runs; it gets its own inbox once it has an id.
    executor.with_child_identity("", None, None, None)
}

/// The query a model override opens.
///
/// The target model is resolved with the level, and it inherits the session's
/// non-auto service tier when it declares one: a child spawned from a
/// flex-tier session stays on flex where the target supports it.
pub(super) fn override_query(
    support: &SubagentSupport,
    template: &dyn Backend,
    model: &str,
    level: Option<ThinkingLevel>,
) -> Result<String> {
    let (model, thinking) = super::model::select_model_for_subagent(
        &support.registry,
        &support.config,
        model,
        level,
        &template.selection().service_tier,
    )
    .map_err(|e| Error::Tool(format!("invalid model override: {e}")))?;
    Ok(format!(
        "{}/{}:{}@{}",
        model.provider,
        model.id,
        thinking.as_str(),
        model.service_tier.as_str()
    ))
}

/// The same child at another thinking level. Off is always allowed, and a
/// session model that declares no levels still runs at Off.
fn with_thinking(
    child: BackendRef,
    level: &ThinkingLevel,
) -> std::result::Result<BackendRef, RunRejection> {
    if *level == ThinkingLevel::Off {
        return Ok(child);
    }
    let levels = child.choice().thinking_levels;
    if levels.is_empty() {
        return Err(Error::Tool("the session model does not support thinking".into()).into());
    }
    if !levels.contains(level) {
        let allowed: Vec<&str> = levels.iter().map(ThinkingLevel::as_str).collect();
        return Err(Error::Tool(format!(
            "thinking level {} not supported by the session model; allowed: {}",
            level.as_str(),
            allowed.join(", ")
        ))
        .into());
    }
    let selection = child.selection();
    Ok(child.with_selection(&format!(
        "{}/{}:{}",
        child.name(),
        selection.model,
        level.as_str()
    ))?)
}

/// The child backend: the template's transport and selection, unless the call
/// names another model, in which case the factory opens whichever transport
/// serves it.
async fn child_backend(
    template: &BackendRef,
    support: &SubagentSupport,
    args: &Value,
) -> std::result::Result<BackendRef, RunRejection> {
    let model = str_arg(args, "model")?;
    let level = str_arg(args, "thinking")?
        .map(|level| {
            ThinkingLevel::parse(level)
                .ok_or_else(|| Error::Tool(format!("unknown thinking level: {level}")))
        })
        .transpose()?;
    // Captured once, before the fork, from the parent session's live hints.
    let root_request = inherited_request(template.as_ref());
    let mut child = if let Some(query) = model {
        // An explicit level is resolved together with the model so it is
        // validated against the supported levels of the target.
        let query = override_query(support, template.as_ref(), query, level.clone())?;
        let executor = child_executor(template.as_ref(), support, root_request.clone());
        // The factory opens a session backend; a child forks from it so it
        // runs without run management of its own.
        let opened = support
            .factory()?
            .open(Some(&query), executor.clone())
            .await
            .map_err(|e| Error::Tool(format!("invalid model override: {e}")))?;
        opened.fork(executor)
    } else {
        let child = template.fork(child_executor(template.as_ref(), support, root_request));
        match level {
            Some(level) => with_thinking(child, &level)?,
            None => child,
        }
    };
    if let Some(system) = str_arg(args, "system")? {
        child = child.with_system_prompt(system.to_string());
    }
    Ok(child)
}

/// Build or resume the entry, and hand out the state the turn needs.
#[allow(clippy::type_complexity, clippy::too_many_lines)]
async fn prepare(
    template: &BackendRef,
    support: &SubagentSupport,
    registry: &SubagentRegistry,
    id: Option<String>,
    args: &Value,
    notify: bool,
) -> std::result::Result<
    (
        BackendRef,
        Vec<Message>,
        Option<SessionCursor>,
        Arc<AtomicBool>,
        String,
    ),
    RunRejection,
> {
    if let Some(id) = id {
        return prepare_resume(registry, id, args, notify);
    }

    // Built before the entry exists, so a rejected override leaves no state.
    let mut agent = child_backend(template, support, args).await?;
    // The job registry stays shared with the session: an isolated registry
    // would leak child processes at session end.
    let jobs = template.tool_executor().jobs();

    let cancel = Arc::new(AtomicBool::new(false));
    // Reserve capacity, the id, and a placeholder entry under one lock;
    // transcript creation does blocking store I/O and must not hold it.
    // A rejected spawn returns inside the block, so it leaves no persistent
    // state behind; the placeholder is filled in with the transcript below.
    let id = {
        let mut entries = lock(&registry.entries);
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
        evict_for_spawn(&mut entries)?;
        let id = loop {
            let id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
            // A killed owner's job bar outlives its entry (its old worker
            // may still finish in-flight work): do not hand the id to a
            // new run, which would then be unable to spawn jobs.
            if !entries.contains_key(&id) && !jobs.owner_cancelled(&id) {
                break id;
            }
        };
        registry.mailbox.open(&id);
        let (state_tx, _) = watch::channel(Status::Running);
        entries.insert(
            id.clone(),
            Entry {
                status: Status::Running,
                backend: agent.clone(),
                messages: Vec::new(),
                history_loaded: true,
                cancel: cancel.clone(),
                session: None,
                cursor: None,
                transcript_path: None,
                used: registry.stamp(),
                // Born 0 would collide with the absent page cursor.
                born: registry.next_born.fetch_add(1, Ordering::SeqCst) + 1,
                notify_tick: 0,
                notify_task: None,
                outcome: None,
                state_tx,
                abort: None,
                events: EventLog::default(),
                notify_terminal: notify,
                note: None,
                started: std::time::Instant::now(),
                task: task_preview(str_arg(args, "prompt")?.unwrap_or("")),
                // The notify argument marks the spawn path: only
                // `subagentSpawn` starts a run with notices on.
                background: notify,
            },
        );
        id
    };
    // The child has an identity now: its jobs are tagged with the run id so a
    // kill or a session switch cancels exactly its process tree, and its
    // sandbox gets its own inbox.
    let scope = MailboxScope {
        mailbox: registry.mailbox.clone(),
        me: id.clone(),
    };
    agent = agent.with_tool_executor(agent.tool_executor().with_child_identity(
        &id,
        Some(scope.clone()),
        Some(mailbox_hook(scope)),
        None,
    ));
    if let Some(entry) = lock(&registry.entries).get_mut(&id) {
        entry.backend = agent.clone();
    }
    let cursor = if support.persist_sessions {
        let created = SessionStore::open().and_then(|store| {
            attach_mailbox_journal(registry, &store);
            let cursor = store.create_subagent_cursor(
                &support.cwd,
                &agent.run_model(),
                registry.parent_session().as_deref(),
                &id,
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

/// Transcript id of an entry, whether the cursor is open (live or resumed
/// in this process) or not (rehydrated, first resume pending).
fn entry_session_id(entry: &Entry) -> Option<String> {
    entry.cursor.as_ref().map(SessionCursor::id).or_else(|| {
        entry
            .session
            .as_deref()
            .map(|name| name.strip_suffix(".jsonl").unwrap_or(name).to_string())
    })
}

/// Build one registry entry from a transcript: identity, model, and the
/// persisted outcome only. The history replays on the first resume; the
/// system prompt comes from the replayed lineage (a header-level read could
/// pick a record from an abandoned branch).
fn restore_entry(
    template: &dyn Backend,
    support: &SubagentSupport,
    registry: &SubagentRegistry,
    file: &SessionFile,
) -> (String, Entry) {
    let meta = file.meta();
    // Transcripts written before rehydration existed carry no run_id; the
    // file stem stands in so they stay resumable.
    let id = meta.run_id.clone().unwrap_or_else(|| file.id());
    let session_id = file.id();
    let (status, outcome) = match read_settled_marker(file.path()) {
        Some(settled) => (
            settled.status,
            RunOutcome {
                status: settled.status,
                result: settled.result,
                error: settled.error,
                full_bytes: None,
                usage: settled.usage,
            },
        ),
        None => (
            Status::Cancelled,
            RunOutcome {
                status: Status::Cancelled,
                result: None,
                error: Some("lofi exited before the run settled".to_string()),
                full_bytes: None,
                usage: None,
            },
        ),
    };
    let outcome = outcome_json(&id, Some(&session_id), outcome);
    // Restore the model the transcript ran with. When it is gone (provider
    // or model removed from the config), keep the session model and say so
    // on the entry instead of failing the restore.
    let mut note = None;
    // A restored entry lost the capture across the restart, so it inherits
    // nothing: the current parent request did not start this run.
    let executor = child_executor(template, support, None);
    let mut child = template.fork(executor.clone());
    let selection = child.selection();
    let current = format!(
        "{}/{}:{}",
        child.name(),
        selection.model,
        selection.thinking.as_str()
    );
    let restored = format!(
        "{}/{}:{}",
        meta.model.provider,
        meta.model.selected_or_id(),
        meta.model.thinking.as_str()
    );
    if restored != current {
        match support
            .factory()
            .and_then(|factory| factory.open_resolved(Some(&restored), executor.clone()))
        {
            // The factory opens a session backend; a restored child runs as a
            // child, so it forks from it.
            Ok(opened) => child = opened.fork(executor.clone()),
            Err(e) => {
                note = Some(format!(
                    "transcript model {} is unavailable ({e}); resume uses the session model",
                    meta.model.label(),
                ));
            }
        }
    }
    // A restored child cannot manage runs: it messages its parent instead.
    let scope = MailboxScope {
        mailbox: registry.mailbox.clone(),
        me: id.clone(),
    };
    child = child.with_tool_executor(child.tool_executor().with_child_identity(
        &id,
        Some(scope.clone()),
        Some(mailbox_hook(scope)),
        None,
    ));
    let (state_tx, _) = watch::channel(status);
    let entry = Entry {
        status,
        backend: child,
        messages: Vec::new(),
        history_loaded: false,
        cancel: Arc::new(AtomicBool::new(false)),
        session: file
            .path()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        cursor: None,
        transcript_path: Some(file.path().to_path_buf()),
        used: registry.stamp(),
        // Born 0 would collide with the absent page cursor.
        born: registry.next_born.fetch_add(1, Ordering::SeqCst) + 1,
        outcome: Some(outcome),
        state_tx,
        abort: None,
        events: EventLog::default(),
        notify_terminal: false,
        notify_tick: 0,
        notify_task: None,
        note,
        started: std::time::Instant::now(),
        task: String::new(),
        // A rehydrated entry is settled; a resume re-claims it as
        // foreground and flips this flag itself.
        background: false,
    };
    (id, entry)
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
    notify: bool,
) -> std::result::Result<
    (
        BackendRef,
        Vec<Message>,
        Option<SessionCursor>,
        Arc<AtomicBool>,
        String,
    ),
    RunRejection,
> {
    let (agent, history, cursor, transcript_path, cancel, loaded) = {
        let mut entries = lock(&registry.entries);
        match entries.get(&id) {
            None => return Err(Error::Tool(format!("unknown subagent id: {id}")).into()),
            Some(entry) if !entry.status.is_terminal() => {
                return Err(RunRejection::Busy(entry.status.as_str().to_string()))
            }
            Some(_) => {}
        }
        // A resume re-enters the running set, so it is subject to the same cap.
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
        let entry = entries
            .get_mut(&id)
            .ok_or_else(|| Error::Tool(format!("unknown subagent id: {id}")))?;
        if str_arg(args, "model")?.is_some()
            || str_arg(args, "thinking")?.is_some()
            || str_arg(args, "system")?.is_some()
        {
            return Err(Error::Tool(
                "model, thinking and system only apply when spawning a subagent".into(),
            )
            .into());
        }
        // Parse before the claim: a failure must not strand the entry in
        // the running state.
        let task = task_preview(str_arg(args, "prompt")?.unwrap_or(""));
        // Claim the entry under the lock so a concurrent resume of the same
        // id is rejected instead of racing this run.
        entry.status = Status::Running;
        entry.used = registry.stamp();
        entry.started = std::time::Instant::now();
        entry.task = task;
        // A previous kill barred this owner from acquiring jobs; the new run
        // needs them again.
        entry.backend.tool_executor().jobs().uncancel_owner(&id);
        // Fresh cancel flag per run: the previous run's flag may still be
        // observed by its lingering sandbox work, and resetting it would
        // uncancel that work.
        entry.cancel = Arc::new(AtomicBool::new(false));
        // The previous run's outcome and abort handle belong to that run; the
        // new run stores its own. Waiters see the state move back to running.
        entry.outcome = None;
        entry.abort = None;
        // The resume path is always foreground: silence the terminal notice and
        // retire the previous run's periodic loop, which would otherwise wake
        // up on the running status.
        entry.notify_terminal = notify;
        entry.background = notify;
        entry.notify_tick += 1;
        if let Some(task) = entry.notify_task.take() {
            task.abort();
        }
        entry.state_tx.send_replace(Status::Running);
        // The previous run closed this inbox at settle; a resume continues the
        // same conversation and must receive what that run never read.
        registry.mailbox.open(&id);
        let transcript_backed = entry.cursor.is_some() || entry.transcript_path.is_some();
        // The run owns the history while it runs: a transcript-backed entry
        // gets it back at settle or reloads it from the transcript after an
        // abort, so nothing retains two copies. An entry without a
        // transcript keeps its only copy in the registry.
        let history = match (entry.history_loaded, transcript_backed) {
            (true, true) => std::mem::take(&mut entry.messages),
            (true, false) => entry.messages.clone(),
            (false, _) => Vec::new(),
        };
        (
            entry.backend.clone(),
            history,
            entry.cursor.clone(),
            entry.transcript_path.clone(),
            entry.cancel.clone(),
            entry.history_loaded,
        )
    };
    if loaded {
        return Ok((agent, history, cursor, cancel, id));
    }
    // First resume after a rehydrate or an unload: replay the transcript.
    // The store I/O stays outside the entries lock; a concurrent kill sets
    // the claimed cancel flag, which the run observes on its first round.
    match load_entry_history(cursor, transcript_path) {
        Ok((history, cursor, system)) => {
            let agent = match system {
                Some(prompt) if prompt != agent.system_prompt() => agent.with_system_prompt(prompt),
                _ => agent,
            };
            let mut entries = lock(&registry.entries);
            let Some(entry) = entries.get_mut(&id) else {
                return Err(Error::Tool(format!("unknown subagent id: {id}")).into());
            };
            entry.cursor = Some(cursor.clone());
            entry.history_loaded = true;
            Ok((agent, history, Some(cursor), entry.cancel.clone(), id))
        }
        Err(e) => {
            unclaim(registry, &id, &e);
            Err(Error::Tool(format!("subagent transcript could not be loaded: {e}")).into())
        }
    }
}

/// A resume whose transcript failed to load must not wedge the entry in the
/// running state: publish the failure and hand the entry back to waiters.
fn unclaim(registry: &SubagentRegistry, id: &str, error: &Error) {
    let mut entries = lock(&registry.entries);
    let Some(entry) = entries.get_mut(id) else {
        return;
    };
    entry.status = Status::Failed;
    entry.outcome = Some(json!({
        "ok": false,
        "id": id,
        "status": Status::Failed.as_str(),
        "error": format!("subagent transcript could not be loaded: {error}"),
    }));
    entry.state_tx.send_replace(Status::Failed);
    // The claim reopened the address before the load; a failed claim must
    // close it again, or a send would be accepted for a run that never starts.
    registry.mailbox.close(id, Status::Failed.as_str());
}

/// Replay a transcript into the resume history: a bounded projection of the
/// selected lineage (events stream one at a time; the full event vector is
/// never materialized), the latest system prompt split out for the agent,
/// and a trailing incomplete tool exchange dropped.
fn load_entry_history(
    cursor: Option<SessionCursor>,
    transcript_path: Option<PathBuf>,
) -> Result<(Vec<Message>, SessionCursor, Option<String>)> {
    let (cursor, snapshot) = if let Some(cursor) = cursor {
        let snapshot = cursor.snapshot()?;
        (cursor, snapshot)
    } else {
        let path = transcript_path
            .ok_or_else(|| Error::State("subagent entry has no transcript".to_string()))?;
        SessionCursor::open_snapshot(path)?
    };
    // Same leaf-first recipe as the parent session's restore: from
    // history_start so a compaction boundary is never crossed.
    let offsets: Vec<u64> = snapshot.index[snapshot.history_start.min(snapshot.index.len())..]
        .iter()
        .rev()
        .map(|entry| entry.offset)
        .collect();
    let mut projection = crate::session::replay::AgentHistoryProjection::default();
    cursor.visit_events(&offsets, |event| {
        projection.push(&event);
        Ok(())
    })?;
    let mut messages = projection.finish();
    drop(snapshot);
    lofi_code::memory::release_freed_memory();
    // The projection puts the latest system prompt first; the agent carries
    // it instead, matching entries that never reloaded.
    let system = if messages
        .first()
        .is_some_and(|message| message.role == Role::System)
    {
        let system = messages.remove(0);
        let text = system
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        Some(text)
    } else {
        None
    };
    trim_dangling_tool_calls(&mut messages);
    Ok((messages, cursor, system))
}

/// A process can exit between an assistant tool call and its result;
/// providers reject a history whose tail calls tools without results. Drop
/// the unfinished turn tail: the partial exchange stays in the transcript
/// for display, but the model never sees half a call.
fn trim_dangling_tool_calls(messages: &mut Vec<Message>) {
    while let Some(assistant) = messages
        .iter()
        .rposition(|message| message.role == Role::Assistant)
    {
        let dangling = {
            let answered: std::collections::HashSet<&str> = messages[assistant + 1..]
                .iter()
                .flat_map(|message| message.blocks.iter())
                .filter_map(|block| match block {
                    ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
                    _ => None,
                })
                .collect();
            messages[assistant].blocks.iter().any(|block| {
                matches!(block, ContentBlock::ToolUse { id, .. } if !answered.contains(id.as_str()))
            })
        };
        if !dangling {
            break;
        }
        messages.truncate(assistant);
    }
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

#[cfg(test)]
mod mailbox_capability {
    use super::*;

    #[tokio::test]
    async fn a_child_hook_answers_mailbox_ops_but_not_management() {
        let mailbox = Mailbox::default();
        mailbox.open("child");
        let hook = mailbox_hook(MailboxScope {
            mailbox: mailbox.clone(),
            me: "child".to_string(),
        });

        let run = hook(SubagentOp::Run, json!({ "prompt": "x" })).await;
        assert_eq!(run["status"], "unavailable", "{run}");

        let sent = hook(
            SubagentOp::MailboxSend,
            json!({ "to": PARENT_ADDRESS, "body": "hi" }),
        )
        .await;
        assert_eq!(sent["ok"], true, "{sent}");

        let sibling = hook(
            SubagentOp::MailboxSend,
            json!({ "to": "other", "body": "hi" }),
        )
        .await;
        assert_eq!(sibling["status"], "forbidden", "{sibling}");

        let received = mailbox.receive(PARENT_ADDRESS, &json!({})).await;
        assert_eq!(received["messages"][0]["body"], "hi", "{received}");
    }
}
