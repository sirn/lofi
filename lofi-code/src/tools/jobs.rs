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
//! scoped to the owning session: the registry is created per agent run,
//! there is no cross-session visibility, and dropping the registry kills
//! any surviving process groups.

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

/// Default per-job timeout when the caller does not pass `timeoutMs`.
/// Longer than `lofi.bash`'s two minutes because background jobs are the
/// builds and test runs that do not fit a synchronous tool call.
const DEFAULT_JOB_TIMEOUT_MS: u64 = 10 * 60 * 1000;
/// Byte budget for a single `job_read` page. Generous compared to the
/// user-visible bash tail because the agent explicitly pages; still bounded
/// so a chatty job cannot flood one tool result.
const MAX_JOB_READ_BYTES: usize = 64 * 1024;
/// Hard cap on a job's log file. Checked between reads, so a runaway writer
/// may overshoot by one poll interval's worth of output before it is killed;
/// without it `yes` would fill the disk.
const MAX_JOB_LOG_BYTES: u64 = 64 * 1024 * 1024;
/// How often the driver task re-checks the child, the kill flag, the
/// timeout, and the log-size cap.
const JOB_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
    timeout_ms: u64,
    notify: bool,
    /// Set when the log file exceeded [`MAX_JOB_LOG_BYTES`] and the job was
    /// killed for it; surfaces in the completion notice so the agent can
    /// tell a disk-protection kill apart from a plain failure.
    log_overflow: bool,
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
            "notify": self.notify,
        })
    }
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
    /// One-line completion summaries awaiting injection into the
    /// conversation. The agent drains this at each round boundary so a
    /// background job's result reaches the model even when it finished
    /// between turns.
    notices: Mutex<Vec<String>>,
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
                notices: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Take all queued completion notices, leaving the queue empty. The
    /// host agent calls this at each round boundary and injects the batch
    /// into the conversation.
    #[must_use]
    pub fn drain_notices(&self) -> Vec<String> {
        self.inner
            .notices
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
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
        let timeout_ms = args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_JOB_TIMEOUT_MS);

        if let Some(blocked) = self.check_policy(&cmd).await {
            return Ok(blocked);
        }

        let id = self.jobs.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let log_path = self
            .tmp_dir
            .join(format!("lofi-job-{id}.log"))
            .to_string_lossy()
            .into_owned();
        let log_file = std::fs::OpenOptions::new()
            .write(true)
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
                notify: true,
                log_overflow: false,
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

        let pid = {
            let mut job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if job.state.is_terminal() {
                return Ok(job.to_json(&self.root));
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
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v = job.to_json(&self.root);
        v["reason"] = json!(reason);
        Ok(v)
    }

    /// Toggle the completion notification for this job. When enabled (the
    /// default at spawn), a terminal transition queues a notice that the
    /// host injects at the next round boundary; `enabled: false` suppresses
    /// it when the agent has already collected the result via `job_wait`.
    /// `interval_ms` is accepted for forward compatibility; periodic
    /// progress pings are not emitted.
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    #[allow(clippy::unused_async)]
    pub async fn job_notify(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.notify = enabled;
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "notify": job.notify,
        }))
    }
}

/// Drive a spawned child to completion. Polls `try_wait` so cancellation,
/// timeout, and the log-size cap are all observed on one clock; each
/// terminal transition updates the job record, wakes `job_wait` listeners,
/// and queues the completion notice for the host agent.
async fn run_job(
    tools: BuiltinTools,
    handle: Arc<JobHandle>,
    mut child: tokio::process::Child,
    timeout_ms: u64,
) {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut guard = PgrpKillGuard::new(child.id());
    // On any early return the guard SIGKILLs the process group; on a clean
    // reap it is disarmed.
    let mut outcome: Option<(State, Option<i32>, Option<i32>)> = None;

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
        if Instant::now() >= deadline {
            outcome = Some((State::TimedOut, None, None));
            break;
        }
        if log_overflowed(&handle) {
            outcome = Some((State::Failed, None, None));
            break;
        }
        tokio::time::sleep(JOB_POLL_INTERVAL).await;
    }

    let log_overflow =
        matches!(outcome, Some((State::Failed, None, None))) && log_overflowed(&handle);
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
        job.log_overflow = log_overflow;
        job.ended = Some(job.ended.unwrap_or_else(Instant::now));
        let notice = if job.notify {
            Some(format_notice(&job))
        } else {
            None
        };
        notice
    };
    handle.done.notify_waiters();
    if let Some(text) = notice {
        tools
            .jobs
            .inner
            .notices
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text);
    }
}

fn log_overflowed(handle: &JobHandle) -> bool {
    let log_path = handle
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .log_path
        .clone();
    std::fs::metadata(&log_path).is_ok_and(|m| m.len() > MAX_JOB_LOG_BYTES)
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
    if job.log_overflow {
        s.push_str(" [log exceeded size cap]");
    }
    let _ = write!(s, " — log: {}", job.log_path);
    s
}
