//! Background job registry: spawn a bounded shell command without blocking
//! the current agent turn, then inspect, page, type into, or cancel it by
//! id.
//!
//! A job runs `sh -c <cmd>` in the workspace root with the same stripped /
//! redacted environment as `lofi.bash`, in its own process group so a kill
//! tears down the whole tree. Stdout and stderr share one per-job log file
//! under the session tmp dir (a read root, so `lofi.read(log_path)` also
//! works); `jobRead` pages that file over a byte cursor. An opt-in `tty`
//! job runs on a pseudo-terminal instead: the driver copies the PTY master
//! into the same log and tracks the rendered screen so interactive prompts
//! can be answered with `jobType` / `jobKeyPress`. A terminal transition
//! queues a completion notice that the host agent injects at the next round
//! boundary and surfaces live as a `Notice`. Jobs are scoped to the owning
//! session: the agent creates one registry and shares it with every exec,
//! there is no cross-session visibility, and dropping the last registry
//! clone kills any surviving process groups.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lofi_error::{Error, Result};
use serde_json::{json, Value};

mod bindings;
mod driver;
mod pty;

pub use pty::{JobAttributes, JobColor, JobScreen, JobScreenLine, JobSpan, JobStyle};

/// Byte budget for a single `jobRead` page. Generous compared to the
/// user-visible bash tail because the agent explicitly pages; still bounded
/// so a chatty job cannot flood one tool result.
const MAX_JOB_READ_BYTES: usize = 64 * 1024;
/// How often the driver task re-checks the child, the kill flag, and the
/// timeout.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Default progress-notice interval when `jobNotify` enables periodic
/// pings without passing `intervalMs`.
const DEFAULT_NOTIFY_INTERVAL_MS: u64 = 30_000;
/// Floor for the progress-notice interval. Anything lower is clamped here
/// so a chatty interval cannot flood the transcript.
const MIN_NOTIFY_INTERVAL_MS: u64 = 5_000;
/// Floor for the idle threshold. Anything lower is clamped here so a
/// transient scheduling pause cannot look like an interactive prompt.
const MIN_IDLE_MS: u64 = 500;
/// Default idle threshold for `tty` jobs. Non-tty jobs are long-running
/// compute where silence is expected, so idle is opt-in for them.
const DEFAULT_TTY_IDLE_MS: u64 = 15_000;
/// Default PTY size. Matches the agent-facing `tu` terminal default.
const DEFAULT_TTY_COLS: u16 = 120;
const DEFAULT_TTY_ROWS: u16 = 40;
/// Upper bound for a single PTY resize dimension.
const MAX_TTY_DIM: u16 = 1000;
/// Upper bound for one `jobType` payload.
const MAX_TTY_WRITE_BYTES: usize = 64 * 1024;
/// Terminal job records kept for inspection. Older terminal records are
/// evicted (newest kept) so a long session's registry stays bounded;
/// running jobs are never evicted.
const MAX_TERMINAL_JOBS: usize = 64;
/// Notices buffered while no subscriber is live. Overflow drops the oldest
/// notices and queues one summary notice with the running drop total.
const MAX_PENDING_NOTICES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl State {
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

struct Job {
    id: u64,
    cmd: String,
    state: State,
    pid: Option<u32>,
    started: Instant,
    ended: Option<Instant>,
    exit_code: Option<i32>,
    signal: Option<i32>,
    log_path: String,
    /// Kill deadline; `None` means the job runs until it exits, is killed,
    /// or the session ends.
    timeout_ms: Option<u64>,
    notify: NotifyOpts,
    /// Whether the child runs on a pseudo-terminal.
    tty: bool,
    /// Current PTY window size. Only meaningful for `tty` jobs.
    cols: u16,
    rows: u16,
    /// Output-idle threshold; `None` disables idle notices. Defaults on for
    /// `tty` jobs, off for plain jobs.
    idle_ms: Option<u64>,
    /// Whether the job's output has been still long enough to be idle.
    idle: bool,
    /// When output last changed. Always tracked while running; the basis for
    /// idle notices.
    last_output_at: Option<Instant>,
    /// Owning subagent run id; `None` for jobs spawned by the session's own
    /// execs. Lets a subagent cancellation kill exactly its process tree.
    owner: Option<String>,
}

/// Progress-notification settings for a job.
#[derive(Debug, Clone, Copy)]
struct NotifyOpts {
    /// Master switch. When false, no notices are queued at all.
    enabled: bool,
    /// Periodic-progress interval; `None` means only the terminal notice.
    interval_ms: Option<u64>,
    /// When true (the default), a periodic tick only emits if the log grew
    /// since the last tick. When false, every tick emits while running.
    changed: bool,
}

impl NotifyOpts {
    /// The spawn-time default: terminal notice only.
    fn terminal_only() -> Self {
        Self {
            enabled: true,
            interval_ms: None,
            changed: true,
        }
    }
}

impl Job {
    fn to_json(&self, root: &std::path::Path) -> Value {
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = self
            .ended
            .unwrap_or_else(Instant::now)
            .duration_since(self.started)
            .as_millis() as u64;
        json!({
            "ok": true,
            "id": self.id.to_string(),
            "state": self.state.as_str(),
            "command": self.cmd,
            "directory": root.display().to_string(),
            "pid": self.pid,
            "exitCode": self.exit_code,
            "signal": self.signal,
            "durationMs": duration_ms,
            "timeoutMs": self.timeout_ms,
            "logPath": self.log_path,
            "notify": self.notify.enabled,
            "notifyIntervalMs": self.notify.interval_ms,
            "notifyChanged": self.notify.changed,
            "tty": self.tty,
            "cols": self.cols,
            "rows": self.rows,
            "idle": self.idle,
            "idleMs": self.idle_ms,
            "idleForMs": self.idle.then(|| {
                self.last_output_at
                    .map_or(0, |t| t.elapsed().as_millis() as u64)
            }),
        })
    }
}

/// A UI-facing snapshot of one job. Plain data, no async, no lock held past
/// the copy. `state` is the `as_str` form (`"running"`, `"completed"`, ...).
/// `duration_ms` is wall time to now while running, or to `ended` once done.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: u64,
    pub cmd: String,
    pub state: String,
    pub running: bool,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    pub log_path: String,
    pub tty: bool,
    /// Whether completion publishes a notice (`jobSpawn` `notify`). The UI
    /// keeps a terminal job's running line until that notice arrives.
    pub notify: bool,
}

/// A job lifecycle notice pushed to subscribers (and buffered for late
/// ones). `terminal` marks the single completion notice; progress and idle
/// ticks carry `false`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobNotice {
    pub id: u64,
    pub terminal: bool,
    pub text: String,
}

struct JobHandle {
    data: Mutex<Job>,
    /// Set by `jobKill`; the driver task polls it between `try_wait`s.
    cancel: std::sync::atomic::AtomicBool,
    /// Completion belongs to the exec that acquired this job. Retaining the
    /// hook here prevents a later exec from replacing its transcript owner.
    on_release: Mutex<Option<crate::JobReleaseFn>>,
    /// PTY master, cloned for the writer side (`jobType` / `jobKeyPress`).
    /// The driver holds its own clone for reads.
    master: Mutex<Option<OwnedFd>>,
    /// Latest parsed PTY screen for the user-facing job viewer. Raw output
    /// remains in the disk log for agent reads and diagnostics.
    screen: Mutex<Option<JobScreen>>,
    /// Selects the arrow-key sequence expected by the running terminal app.
    application_cursor: std::sync::atomic::AtomicBool,
}

/// Oldest-first buffer of notices produced while no UI subscriber is live.
/// Bounded: overflow drops the oldest notices and records one summary
/// notice carrying the running drop total, so the loss is visible to the
/// host agent instead of silent.
struct PendingNotices {
    queue: std::collections::VecDeque<JobNotice>,
    dropped_total: u64,
    /// `dropped_total` at the time the newest summary was queued. A summary
    /// is queued only when drops happened since the previous one.
    summarized: u64,
}

impl PendingNotices {
    fn new() -> Self {
        Self {
            queue: std::collections::VecDeque::new(),
            dropped_total: 0,
            summarized: 0,
        }
    }

    fn push(&mut self, notice: JobNotice) {
        // Reserve room for a possible summary plus the notice itself.
        while self.queue.len() + 2 > MAX_PENDING_NOTICES {
            self.queue.pop_front();
            self.dropped_total += 1;
        }
        if self.dropped_total > self.summarized {
            self.summarized = self.dropped_total;
            self.queue.push_back(JobNotice {
                id: notice.id,
                terminal: false,
                text: format!(
                    "{} earlier job notices dropped (queue overflow); \
                     check jobList for current state",
                    self.dropped_total,
                ),
            });
        }
        self.queue.push_back(notice);
    }

    fn drain(&mut self) -> Vec<JobNotice> {
        self.queue.drain(..).collect()
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.dropped_total = 0;
        self.summarized = 0;
    }
}

struct Inner {
    /// Seeded from the current time at registry construction so two
    /// registries sharing a tmp dir (as in tests, or a session restart
    /// reusing the same state dir) never mint the same job id and
    /// collide on the same `create_new` log file.
    next_id: AtomicU64,
    /// Changes whenever the host starts or attaches a different session.
    /// Drivers publish only into the generation they were spawned in.
    generation: AtomicU64,
    jobs: Mutex<HashMap<u64, Arc<JobHandle>>>,
    /// Live notice subscribers (one per UI host). Zero subscribers means
    /// headless; notices still land in `pending` so a late subscriber or a
    /// `drain_notices` call sees the same sequence.
    subscribers: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<JobNotice>>>,
    /// Buffered notices produced while no UI subscriber is live, or while a
    /// test drives the registry headless. `subscribe_notices` flushes this
    /// buffer through the newly added subscriber. Bounded; overflow is
    /// summarized by [`PendingNotices`].
    pending: Mutex<PendingNotices>,
}

/// Per-session registry of background jobs. Cheap to clone; every clone
/// shares the same map, so the exec-time tool bundle and the agent that
/// owns the session see the same jobs. Dropping the last clone kills all
/// surviving process groups.
#[derive(Clone)]
pub struct JobRegistry {
    inner: Arc<Inner>,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl JobRegistry {
    #[must_use]
    pub fn new() -> Self {
        #[allow(clippy::cast_possible_truncation)]
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        Self {
            inner: Arc::new(Inner {
                next_id: AtomicU64::new(nanos),
                generation: AtomicU64::new(0),
                jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
                subscribers: std::sync::Mutex::new(Vec::new()),
                pending: std::sync::Mutex::new(PendingNotices::new()),
            }),
        }
    }

    /// Take all queued notices, leaving the queue empty. Used by tests and
    /// any host that has not subscribed to the live notice channel.
    #[must_use]
    pub fn drain_notices(&self) -> Vec<JobNotice> {
        self.inner
            .pending
            .lock()
            .map(|mut q| q.drain())
            .unwrap_or_default()
    }

    /// Subscribe to live job-completion notices. Any notices buffered before
    /// the subscription flush through the returned receiver first, so tests
    /// and the live UI see the same stream regardless of subscribe timing.
    #[must_use]
    pub fn subscribe_notices(&self) -> tokio::sync::mpsc::UnboundedReceiver<JobNotice> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        // Install the subscriber and drain the buffer under the same lock
        // order used by publish. Otherwise a completion can land after the
        // buffer drain but before subscriber insertion and remain stranded
        // in `pending` until a second subscriber appears.
        let mut subscribers = self
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.push(tx.clone());
        let buffered = self
            .inner
            .pending
            .lock()
            .map(|mut q| q.drain())
            .unwrap_or_default();
        for notice in buffered {
            let _ = tx.send(notice);
        }
        rx
    }

    /// Point-in-time snapshot of every job this session owns, newest first.
    /// Cheap and allocation-bounded: copies the small per-job fields, never
    /// the log. The UI polls this each frame to render the job list and the
    /// running-count badge.
    #[must_use]
    pub fn snapshot(&self) -> Vec<JobInfo> {
        let jobs = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut out: Vec<JobInfo> = jobs
            .values()
            .map(|h| {
                let j = h
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                JobInfo {
                    id: j.id,
                    cmd: j.cmd.clone(),
                    state: j.state.as_str().to_string(),
                    running: !j.state.is_terminal(),
                    pid: j.pid,
                    exit_code: j.exit_code,
                    #[allow(clippy::cast_possible_truncation)]
                    duration_ms: j
                        .ended
                        .unwrap_or_else(Instant::now)
                        .duration_since(j.started)
                        .as_millis() as u64,
                    log_path: j.log_path.clone(),
                    tty: j.tty,
                    notify: j.notify.enabled,
                }
            })
            .collect();
        out.sort_by_key(|j| std::cmp::Reverse(j.id));
        out
    }

    /// Number of jobs still running. Backs the persistent badge.
    #[must_use]
    pub fn running_count(&self) -> usize {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|h| {
                !h.data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .state
                    .is_terminal()
            })
            .count()
    }

    /// Ids of jobs currently registered, regardless of state.
    #[must_use]
    pub fn live_ids(&self) -> std::collections::HashSet<u64> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .copied()
            .collect()
    }

    /// Kills and unregisters every job whose id is not in `keep`. Returns
    /// the ids that were killed. The kill is silent: notifications are
    /// disabled before termination so the job driver does not push a
    /// "cancelled" notice. Used by `/tree` rollback reconcile, where the
    /// user has explicitly asked to drop the lineage that owned the job —
    /// neither the agent nor the user needs to be told the subsidiary
    /// process went away; the separate UI toast covers the user side.
    pub fn kill_not_in(&self, keep: &[u64]) -> Vec<u64> {
        let keep: std::collections::HashSet<u64> = keep.iter().copied().collect();
        let mut killed = Vec::new();
        for id in self.live_ids() {
            if keep.contains(&id) {
                continue;
            }
            if let Some(handle) = self.get(id) {
                // This resource no longer belongs to the selected lineage.
                // Do not let its asynchronous driver append a release marker
                // to the branch that replaced it.
                handle
                    .on_release
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take();
                handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .notify
                    .enabled = false;
            }
            if self.kill(id) {
                killed.push(id);
            }
            if let Some(handle) = self
                .inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id)
            {
                let log_path = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .log_path
                    .clone();
                let _ = std::fs::remove_file(log_path);
            }
        }
        killed.sort_unstable();
        killed
    }

    /// Number of running jobs owned by one subagent run.
    #[must_use]
    pub fn running_count_for(&self, owner: &str) -> usize {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .filter(|h| {
                let job = h
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                job.owner.as_deref() == Some(owner) && !job.state.is_terminal()
            })
            .count()
    }

    /// Kill every running job owned by one subagent run, silently: notices
    /// are disabled first so the parent session is not spammed with one
    /// "cancelled" notice per owned job. Returns the killed ids.
    pub fn kill_owned(&self, owner: &str) -> Vec<u64> {
        let ids: Vec<u64> = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(_, h)| {
                let job = h
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                job.owner.as_deref() == Some(owner) && !job.state.is_terminal()
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &ids {
            if let Some(handle) = self.get(*id) {
                handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .notify
                    .enabled = false;
            }
            self.kill(*id);
        }
        ids
    }

    /// Kill a job's whole process group and mark it cancelled. Synchronous
    /// core shared by the `jobKill` tool and the UI modal; `true` when the
    /// job existed and was running (so a kill actually happened).
    pub fn kill(&self, id: u64) -> bool {
        let Some(handle) = self.get(id) else {
            return false;
        };
        let pid = {
            let mut job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if job.state.is_terminal() {
                return false;
            }
            job.state = State::Cancelled;
            job.ended = Some(Instant::now());
            job.signal = Some(nix::libc::SIGKILL);
            job.pid
        };
        if let Some(pid) = pid {
            kill_pgrp(pid);
        }
        handle.cancel.store(true, Ordering::Relaxed);
        true
    }

    /// Read up to `limit` bytes of a job's log starting at byte `offset`.
    /// Bounded and disk-backed: the UI pages a growing log with this instead
    /// of loading the whole file, so an unbounded log never inflates memory.
    /// Only the requested window is read (seek + read), not the whole file.
    /// Returns `(bytes, next_offset, total_len)`; `next_offset` is the cursor
    /// for the following page.
    #[must_use]
    pub fn read_log(&self, id: u64, offset: u64, limit: usize) -> Option<(Vec<u8>, u64, u64)> {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let handle = self.get(id)?;
        let log_path = {
            let job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            job.log_path.clone()
        };
        let mut f = std::fs::File::open(&log_path).ok()?;
        let total = f.metadata().ok()?.len();
        let start = offset.min(total);
        f.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = vec![0u8; limit.min((total - start) as usize)];
        let n = f.read(&mut buf).ok()?;
        buf.truncate(n);
        Some((buf, start + n as u64, total))
    }

    /// Return the latest parsed terminal screen for a PTY job. A plain job
    /// has no terminal screen and continues to use its line-based log.
    #[must_use]
    pub fn screen(&self, id: u64) -> Option<JobScreen> {
        self.get(id)?
            .screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn get(&self, id: u64) -> Option<Arc<JobHandle>> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Snapshot every job in the registry, newest id first.
    fn list(&self) -> Vec<Arc<JobHandle>> {
        let mut entries: Vec<(u64, Arc<JobHandle>)> = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(id, h)| (*id, h.clone()))
            .collect();
        entries.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
        entries.into_iter().map(|(_, h)| h).collect()
    }

    /// Sigkill every still-running job's process group. Hosts call this
    /// during graceful shutdown; [`Drop`] is the final fallback when the
    /// owning session releases its last registry clone.
    pub fn shutdown(&self) {
        let jobs: Vec<Arc<JobHandle>> = self
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for handle in jobs {
            let pid = {
                let mut job = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if job.state.is_terminal() {
                    continue;
                }
                job.state = State::Cancelled;
                job.ended = Some(Instant::now());
                job.signal = Some(nix::libc::SIGKILL);
                job.pid
            };
            if let Some(pid) = pid {
                kill_pgrp(pid);
            }
            handle.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// End the current session's job scope. Running process groups are
    /// cancelled without notices, completed rows are removed, and buffered
    /// notices are discarded. Subscribers are disconnected so notices that
    /// were already delivered to an old receiver cannot enter the next
    /// session. Hosts must subscribe again after this reset.
    pub fn reset(&self) {
        let jobs: Vec<Arc<JobHandle>> = {
            let mut jobs = self
                .inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            self.inner.generation.fetch_add(1, Ordering::SeqCst);
            jobs.drain().map(|(_, handle)| handle).collect()
        };
        let mut subscribers = self
            .inner
            .subscribers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscribers.clear();
        self.inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        drop(subscribers);
        for handle in jobs {
            let (pid, log_path) = {
                let mut job = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                job.notify.enabled = false;
                let running_pid = if job.state.is_terminal() {
                    None
                } else {
                    job.state = State::Cancelled;
                    job.ended = Some(Instant::now());
                    job.signal = Some(nix::libc::SIGKILL);
                    job.pid
                };
                (running_pid, job.log_path.clone())
            };
            // The record is gone, so nothing can page its log. A killed
            // job's driver may still drain final bytes into the unlinked
            // file; that is harmless.
            let _ = std::fs::remove_file(log_path);
            if let Some(pid) = pid {
                kill_pgrp(pid);
            }
            handle.cancel.store(true, Ordering::Relaxed);
        }
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        // Only the last clone tears down; intermediate clones (per-exec tool
        // bundles) must not kill jobs still owned by the session.
        if Arc::strong_count(&self.inner) == 1 {
            self.shutdown();
        }
    }
}

#[allow(clippy::cast_possible_wrap)]
fn kill_pgrp(pid: u32) {
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-(pid as i32)),
        nix::sys::signal::Signal::SIGKILL,
    );
}

fn ensure_job_running(handle: &JobHandle) -> Result<()> {
    let job = handle
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if job.state.is_terminal() {
        return Err(Error::Tool("job is not running".into()));
    }
    Ok(())
}
#[cfg(test)]
mod tests;
