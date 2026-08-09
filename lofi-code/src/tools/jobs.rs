//! Background job registry: spawn a bounded shell command without blocking
//! the current agent turn, then poll, page, wait, or cancel it by id.
//!
//! A job runs `sh -c <cmd>` in the workspace root with the same stripped /
//! redacted environment as `lofi.bash`, in its own process group so a kill
//! tears down the whole tree. Stdout and stderr share one per-job log file
//! under the session tmp dir (a read root, so `lofi.read(log_path)` also
//! works); `job_read` pages that file over a byte cursor. A terminal
//! transition queues a completion notice that the host agent injects at
//! the next round boundary and surfaces live as a `Notice`. Jobs are
//! scoped to the owning session: the agent creates one registry and shares
//! it with every exec, there is no cross-session visibility, and dropping
//! the last registry clone kills any surviving process groups.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Notify;

use lofi_error::{Error, Result};

use super::bash_util::PgrpKillGuard;
use super::BuiltinTools;

/// Byte budget for a single `job_read` page. Generous compared to the
/// user-visible bash tail because the agent explicitly pages; still bounded
/// so a chatty job cannot flood one tool result.
const MAX_JOB_READ_BYTES: usize = 64 * 1024;
/// How often the driver task re-checks the child, the kill flag, and the
/// timeout.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Default progress-notice interval when `job_notify` enables periodic
/// pings without passing `interval_ms`.
const DEFAULT_NOTIFY_INTERVAL_MS: u64 = 30_000;
/// Floor for the progress-notice interval. Anything lower is clamped here
/// so a chatty interval cannot flood the transcript.
const MIN_NOTIFY_INTERVAL_MS: u64 = 5_000;

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
            "exit_code": self.exit_code,
            "signal": self.signal,
            "duration_ms": duration_ms,
            "timeout_ms": self.timeout_ms,
            "log_path": self.log_path,
            "notify": self.notify.enabled,
            "notify_interval_ms": self.notify.interval_ms,
            "notify_changed": self.notify.changed,
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
}

struct JobHandle {
    data: Mutex<Job>,
    /// Fired when the job reaches a terminal state; `job_wait` listens.
    done: Notify,
    /// Set by `job_kill`; the driver task polls it between `try_wait`s.
    cancel: std::sync::atomic::AtomicBool,
}

struct Inner {
    /// Seeded from the current time at registry construction so two
    /// registries sharing a tmp dir (as in tests, or a session restart
    /// reusing the same state dir) never mint the same job id and
    /// collide on the same `create_new` log file.
    next_id: AtomicU64,
    jobs: Mutex<HashMap<u64, Arc<JobHandle>>>,
    /// One-line terminal-state summaries (completed/failed/cancelled) awaiting
    /// injection into the model conversation. The agent drains this at each
    /// round boundary so a background job's outcome reaches the model even
    /// when it finished between turns. Periodic progress ticks do NOT land
    /// here — they never reach the model, so a running job stays async.
    terminal_notices: Mutex<Vec<String>>,
    /// Periodic progress ticks and terminal notices for the live UI. The TUI
    /// drains this each frame to render the transcript marker and badge; it
    /// never feeds the model conversation, so ticks never force a new turn.
    ui_notices: Mutex<Vec<String>>,
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
                jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
                terminal_notices: std::sync::Mutex::new(Vec::new()),
                ui_notices: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Take all queued terminal (completion) notices, leaving the queue
    /// empty. The host agent calls this at each round boundary and injects
    /// the batch into the model conversation. Periodic ticks are not here —
    /// only terminal transitions, so a still-running job never re-notifies
    /// the model.
    #[must_use]
    pub fn drain_notices(&self) -> Vec<String> {
        self.inner
            .terminal_notices
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    /// Take all queued UI notices (periodic ticks + terminal), leaving the
    /// queue empty. The TUI drains this each frame to render transcript
    /// markers and refresh the badge. These never enter the model
    /// conversation.
    #[must_use]
    pub fn drain_ui_notices(&self) -> Vec<String> {
        self.inner
            .ui_notices
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
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

    /// Kill a job's whole process group and mark it cancelled. Synchronous
    /// core shared by the `job_kill` tool and the UI modal; `true` when the
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
            job.pid
        };
        if let Some(pid) = pid {
            kill_pgrp(pid);
        }
        handle.cancel.store(true, Ordering::Relaxed);
        handle.done.notify_waiters();
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

    fn get(&self, id: u64) -> Option<Arc<JobHandle>> {
        self.inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&id)
            .cloned()
    }

    /// Sigkill every still-running job's process group. Called from
    /// [`Drop`] when the owning session ends, so a model-started process
    /// never outlives the session that owns it.
    fn kill_all(&self) {
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
                job.pid
            };
            if let Some(pid) = pid {
                kill_pgrp(pid);
            }
            handle.cancel.store(true, Ordering::Relaxed);
            handle.done.notify_waiters();
        }
    }
}

impl Drop for JobRegistry {
    fn drop(&mut self) {
        // Only the last clone tears down; intermediate clones (per-exec tool
        // bundles) must not kill jobs still owned by the session.
        if Arc::strong_count(&self.inner) == 1 {
            self.kill_all();
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

fn parse_id(args: &Value) -> Result<u64> {
    args.get("id")
        .and_then(|v| {
            v.as_str()
                .map(str::to_owned)
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| Error::Tool("job: missing or invalid 'id'".into()))
}

fn no_such_job(id: u64) -> Value {
    json!({ "ok": false, "error": format!("no such job: {id}") })
}

impl BuiltinTools {
    /// Spawn `cmd` detached and return immediately with the job id and
    /// initial state. The command runs with the same shell policy, working
    /// directory, environment, and process-group isolation as `lofi.bash`;
    /// a background job is *not* auto-approved, so a policy `Ask` still
    /// prompts before anything starts.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `cmd` is missing, or [`Error::Io`] when
    /// the log file or the process cannot be created.
    pub async fn job_spawn(&self, args: Value) -> Result<Value> {
        let cmd = args
            .get("cmd")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("job_spawn: missing 'cmd'".into()))?
            .to_owned();
        let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);

        if let Some(blocked) = self.check_policy(&cmd).await {
            return Ok(blocked);
        }

        let id = self.jobs.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let log_path = self
            .tmp_dir
            .join(format!("lofi-job-{id}.log"))
            .to_string_lossy()
            .into_owned();
        // Append mode so the shared stdout/stderr file interleaves both
        // streams at end-of-file with no offset race between the two fds.
        let log_file = std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&log_path)
            .map_err(Error::Io)?;
        // One shared file for both streams: the shared offset keeps merged
        // output in arrival order with no async plumbing in the driver.
        let stderr_file = log_file.try_clone().map_err(Error::Io)?;

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&cmd)
            .current_dir(&self.root)
            .stdout(std::process::Stdio::from(log_file))
            .stderr(std::process::Stdio::from(stderr_file))
            .process_group(0);
        self.bash_env.apply(&mut command);
        let child = command.spawn()?;
        let pid = child.id();

        let handle = Arc::new(JobHandle {
            data: Mutex::new(Job {
                id,
                cmd: cmd.clone(),
                state: State::Running,
                pid,
                started: Instant::now(),
                ended: None,
                exit_code: None,
                signal: None,
                log_path: log_path.clone(),
                timeout_ms,
                notify: NotifyOpts::terminal_only(),
            }),
            done: Notify::new(),
            cancel: std::sync::atomic::AtomicBool::new(false),
        });
        self.jobs
            .inner
            .jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, handle.clone());

        tokio::spawn(run_job(self.clone(), handle, child, timeout_ms));

        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "state": "running",
            "command": cmd,
            "directory": self.root.display().to_string(),
            "pid": pid,
            "timeout_ms": timeout_ms,
            "log_path": log_path,
        }))
    }

    /// Current state, timestamps, exit status, and limits. Session-scoped:
    /// only job ids this session spawned are visible.
    // Async for symmetry with the other job tools (and a uniform binding
    // shape), though the body is synchronous; the sandbox binding awaits it.
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    #[allow(clippy::unused_async)]
    pub async fn job_status(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(job.to_json(&self.root))
    }

    /// Incremental read of the job's merged stdout/stderr log. `cursor` is
    /// a byte offset into the log file; the result returns the next page and
    /// the cursor to resume from. Output is redacted and bounded per call.
    /// The log path is also under the session tmp dir, so `lofi.read` on it
    /// works as an escape hatch for large outputs.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing, or [`Error::Io`] when
    /// the log file cannot be read.
    pub async fn job_read(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let cursor = args.get("cursor").and_then(Value::as_u64).unwrap_or(0);
        #[allow(clippy::cast_possible_truncation)]
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(MAX_JOB_READ_BYTES, |n| (n as usize).min(MAX_JOB_READ_BYTES));

        let (state, log_path) = {
            let job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (job.state, job.log_path.clone())
        };

        let bytes = match tokio::fs::read(&log_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(Error::Io(e)),
        };
        let total = bytes.len() as u64;
        let start = (cursor as usize).min(bytes.len());
        let end = (start + limit).min(bytes.len());
        let mut chunk = String::from_utf8_lossy(&bytes[start..end]).into_owned();
        self.bash_env.redact(&mut chunk);
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "state": state.as_str(),
            "cursor": end as u64,
            "total_bytes": total,
            "output": chunk,
            "done": state.is_terminal(),
        }))
    }

    /// Bounded wait for the job to reach a terminal state. Returns the
    /// final status, or the still-running status when `timeout_ms` elapses.
    /// Waiting never cancels the job.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    pub async fn job_wait(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let timeout_ms = args.get("timeout_ms").and_then(Value::as_u64);

        // Fast path: already terminal.
        {
            let job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if job.state.is_terminal() {
                return Ok(job.to_json(&self.root));
            }
        }

        let wait = handle.done.notified();
        tokio::pin!(wait);
        // `enable` arms the notification even if the job finishes between
        // the fast path above and the await below.
        wait.as_mut().enable();
        if let Some(ms) = timeout_ms {
            let _ = tokio::time::timeout(Duration::from_millis(ms), wait).await;
        } else {
            wait.await;
        }
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(job.to_json(&self.root))
    }

    /// Idempotent cancellation: kill the whole process group and mark the
    /// job cancelled. Safe to call on an already-terminal job (no-op).
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    #[allow(clippy::unused_async)]
    pub async fn job_kill(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("killed by agent")
            .to_owned();
        let was_running = self.jobs.kill(id);
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v = job.to_json(&self.root);
        v["reason"] = json!(reason);
        v["killed"] = json!(was_running);
        Ok(v)
    }

    /// Configure notifications for this job. The terminal transition always
    /// queues one notice (unless `enabled: false`); `interval_ms` turns on
    /// periodic progress pings while the job runs. With `changed` (the
    /// default) a tick only emits when the log grew since the last tick, so
    /// an idle-but-alive job stays quiet; `changed: false` emits every tick.
    /// `interval_ms` is clamped to a 5s floor and defaults to 30s.
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    #[allow(clippy::unused_async)]
    pub async fn job_notify(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Calling `job_notify` at all means "notify me", so a missing
        // `enabled` key is treated as `true`.
        let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        job.notify.enabled = enabled;
        // Enabling without an explicit interval turns periodic pings on at
        // the default, so bare `job_notify({id})` does the useful thing.
        // `enabled: false` leaves the interval untouched.
        if enabled && job.notify.interval_ms.is_none() {
            job.notify.interval_ms = Some(DEFAULT_NOTIFY_INTERVAL_MS);
        }
        if let Some(ms) = args.get("interval_ms").and_then(Value::as_u64) {
            job.notify.interval_ms = Some(ms.max(MIN_NOTIFY_INTERVAL_MS));
        }
        if let Some(changed) = args.get("changed").and_then(Value::as_bool) {
            job.notify.changed = changed;
        }
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "notify": job.notify.enabled,
            "interval_ms": job.notify.interval_ms,
            "changed": job.notify.changed,
        }))
    }
}

/// Drive a spawned child to completion. Polls `try_wait` so cancellation,
/// the (optional) timeout, and the log-size cap are observed on one clock; each
/// terminal transition updates the job record, wakes `job_wait` listeners,
/// and queues the completion notice for the host agent.
async fn run_job(
    tools: BuiltinTools,
    handle: Arc<JobHandle>,
    mut child: tokio::process::Child,
    timeout_ms: Option<u64>,
) {
    let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
    let mut guard = PgrpKillGuard::new(child.id());
    // On any early return the guard SIGKILLs the process group; on a clean
    // reap it is disarmed.
    let mut outcome: Option<(State, Option<i32>, Option<i32>)> = None;
    // Progress-tick bookkeeping. `last_tick` starts at now so the first
    // periodic notice lands one full interval after the job starts;
    // `last_bytes` is what `changed` compares the log size against.
    let mut last_tick = Instant::now();
    let mut last_bytes: u64 = 0;

    loop {
        if handle.cancel.load(Ordering::Relaxed) {
            // `job_kill` already set the state; just reap.
            let _ = child.wait().await;
            guard.disarm();
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt as _;
                guard.disarm();
                let state = if status.success() {
                    State::Completed
                } else {
                    State::Failed
                };
                outcome = Some((state, status.code(), status.signal()));
                break;
            }
            Ok(None) => {}
            Err(_) => {
                outcome = Some((State::Failed, None, None));
                break;
            }
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            outcome = Some((State::TimedOut, None, None));
            break;
        }
        if let Some(text) = progress_tick(&handle, &mut last_tick, &mut last_bytes) {
            // Periodic ticks are transient: they refresh the transcript marker
            // and badge but never enter the model conversation, so a running
            // job stays async and never forces a new turn.
            push_ui_notice(&tools, text);
        }
        tokio::time::sleep(JOB_POLL_INTERVAL).await;
    }

    let notice = {
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((state, exit_code, signal)) = outcome {
            // A `job_kill` that won the race already set Cancelled; do not
            // overwrite a terminal state.
            if !job.state.is_terminal() {
                job.state = state;
            }
            job.exit_code = exit_code;
            job.signal = signal;
        }
        job.ended = Some(job.ended.unwrap_or_else(Instant::now));
        // The terminal notice always fires once when notifications are on,
        // regardless of how `changed` treated the intermediate ticks.
        if job.notify.enabled {
            Some(format_notice(&job))
        } else {
            None
        }
    };
    handle.done.notify_waiters();
    if let Some(text) = notice {
        // A terminal transition is recorded both for the model (so it learns
        // the outcome at the next round boundary) and for the UI (so the
        // transcript marker and badge update live).
        push_terminal_notice(&tools, text.clone());
        push_ui_notice(&tools, text);
    }
}

/// Queue a terminal notice for the host agent to inject into the model
/// conversation at the next round boundary.
fn push_terminal_notice(tools: &BuiltinTools, text: String) {
    tools
        .jobs
        .inner
        .terminal_notices
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(text);
}

/// Queue a notice for the live UI (transcript marker + badge). Never reaches
/// the model conversation.
fn push_ui_notice(tools: &BuiltinTools, text: String) {
    tools
        .jobs
        .inner
        .ui_notices
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(text);
}

/// Emit one periodic progress notice when it is due. A tick is due once
/// `interval_ms` has elapsed since `last_tick`. With `changed` (the
/// default) the tick only fires when the log grew since `last_bytes`, so a
/// live-but-silent job produces no pings; either way the tick that fires
/// (or is skipped for no change) resets the interval. Returns the notice
/// text, or `None` when not due / disabled / unchanged.
fn progress_tick(
    handle: &JobHandle,
    last_tick: &mut Instant,
    last_bytes: &mut u64,
) -> Option<String> {
    let (notify, id, cmd, started, log_path) = {
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if job.state.is_terminal() {
            return None;
        }
        (
            job.notify,
            job.id,
            job.cmd.clone(),
            job.started,
            job.log_path.clone(),
        )
    };
    let interval = notify.interval_ms?;
    if !notify.enabled {
        return None;
    }
    if last_tick.elapsed() < Duration::from_millis(interval) {
        return None;
    }
    *last_tick = Instant::now();
    let bytes = std::fs::metadata(&log_path).map_or(0, |m| m.len());
    if notify.changed && bytes == *last_bytes {
        return None;
    }
    *last_bytes = bytes;
    let secs = started.elapsed().as_secs();
    let mut short = String::new();
    let mut chars = cmd.chars();
    for _ in 0..60 {
        match chars.next() {
            Some(c) => short.push(c),
            None => break,
        }
    }
    if chars.next().is_some() {
        short.push('\u{2026}');
    }
    Some(format!(
        "job {id} running {secs}s, {bytes} bytes logged: {short}"
    ))
}

fn format_notice(job: &Job) -> String {
    use std::fmt::Write as _;
    let mut cmd = String::new();
    let mut chars = job.cmd.chars();
    for _ in 0..60 {
        match chars.next() {
            Some(c) => cmd.push(c),
            None => break,
        }
    }
    if chars.next().is_some() {
        cmd.push('\u{2026}');
    }
    let mut s = format!("job {} {}: {}", job.id, job.state.as_str(), cmd);
    if let Some(code) = job.exit_code {
        let _ = write!(s, " (exit {code})");
    }
    if let Some(sig) = job.signal {
        let _ = write!(s, " (signal {sig})");
    }
    let _ = write!(s, " — log: {}", job.log_path);
    s
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn handle_with(notify: NotifyOpts, log_path: String) -> Arc<JobHandle> {
        Arc::new(JobHandle {
            data: Mutex::new(Job {
                id: 1,
                cmd: "cmd".to_string(),
                state: State::Running,
                pid: None,
                started: Instant::now(),
                ended: None,
                exit_code: None,
                signal: None,
                log_path,
                timeout_ms: None,
                notify,
            }),
            done: Notify::new(),
            cancel: AtomicBool::new(false),
        })
    }

    fn periodic(changed: bool) -> NotifyOpts {
        NotifyOpts {
            enabled: true,
            interval_ms: Some(1),
            changed,
        }
    }

    #[test]
    fn tick_skips_when_log_unchanged() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_string_lossy().into_owned();
        let h = handle_with(periodic(true), p);
        let mut tick = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap();
        let mut bytes = 0;
        // Log empty and unchanged from last_bytes=0: changed suppresses it.
        assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
    }

    #[test]
    fn tick_fires_when_log_grew() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(f.path(), b"hello").unwrap();
        let p = f.path().to_string_lossy().into_owned();
        let h = handle_with(periodic(true), p);
        let mut tick = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap();
        let mut bytes = 0;
        let note = progress_tick(&h, &mut tick, &mut bytes).unwrap();
        assert!(note.contains("running"));
        assert!(note.contains("5 bytes"));
    }

    #[test]
    fn tick_fires_every_time_when_changed_false() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_string_lossy().into_owned();
        let h = handle_with(periodic(false), p);
        let mut tick = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap();
        let mut bytes = 0;
        // No output, but changed=false means emit anyway.
        assert!(progress_tick(&h, &mut tick, &mut bytes).is_some());
    }

    #[test]
    fn tick_not_due_before_interval() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_string_lossy().into_owned();
        let mut n = periodic(false);
        n.interval_ms = Some(60_000);
        let h = handle_with(n, p);
        let mut tick = Instant::now();
        let mut bytes = 0;
        assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
    }

    #[test]
    fn tick_suppressed_when_disabled() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_string_lossy().into_owned();
        let mut n = periodic(false);
        n.enabled = false;
        let h = handle_with(n, p);
        let mut tick = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap();
        let mut bytes = 0;
        assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
    }

    #[test]
    fn tick_none_when_terminal_only() {
        let f = tempfile::NamedTempFile::new().unwrap();
        let p = f.path().to_string_lossy().into_owned();
        let h = handle_with(NotifyOpts::terminal_only(), p);
        let mut tick = Instant::now()
            .checked_sub(Duration::from_millis(10))
            .unwrap();
        let mut bytes = 0;
        // No interval configured: periodic mode is off.
        assert!(progress_tick(&h, &mut tick, &mut bytes).is_none());
    }
}
