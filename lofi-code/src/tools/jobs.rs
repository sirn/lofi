//! Background job registry: spawn a bounded shell command without blocking
//! the current agent turn, then poll, page, wait, type into, or cancel it by
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
use std::io::Write as _;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Notify;

use lofi_error::{Error, Result};

use super::bash_util::{wait_for_cancel, PgrpKillGuard};
use super::BuiltinTools;

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
/// Tail window searched by `jobWaitForInput` pattern matching.
const PATTERN_TAIL_BYTES: usize = 64 * 1024;

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
    /// both idle notices and `jobWaitForInput` stability waits.
    last_output_at: Option<Instant>,
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
}

struct JobHandle {
    data: Mutex<Job>,
    /// Fired when the job reaches a terminal state; `jobWait` listens.
    done: Notify,
    /// Set by `jobKill`; the driver task polls it between `try_wait`s.
    cancel: std::sync::atomic::AtomicBool,
    /// Completion belongs to the exec that acquired this job. Retaining the
    /// hook here prevents a later exec from replacing its transcript owner.
    on_release: Mutex<Option<crate::JobReleaseFn>>,
    /// PTY master, cloned for the writer side (`jobType` / `jobKeyPress` /
    /// `jobResize`). The driver holds its own clone for reads.
    master: Mutex<Option<OwnedFd>>,
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
    subscribers: Mutex<Vec<tokio::sync::mpsc::UnboundedSender<String>>>,
    /// Buffered notices produced while no UI subscriber is live, or while a
    /// test drives the registry headless. `subscribe_notices` flushes this
    /// buffer through the newly added subscriber.
    pending: Mutex<Vec<String>>,
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
                pending: std::sync::Mutex::new(Vec::new()),
            }),
        }
    }

    /// Take all queued notices, leaving the queue empty. Used by tests and
    /// any host that has not subscribed to the live notice channel.
    #[must_use]
    pub fn drain_notices(&self) -> Vec<String> {
        self.inner
            .pending
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }

    /// Subscribe to live job-completion notices. Any notices buffered before
    /// the subscription flush through the returned receiver first, so tests
    /// and the live UI see the same stream regardless of subscribe timing.
    #[must_use]
    pub fn subscribe_notices(&self) -> tokio::sync::mpsc::UnboundedReceiver<String> {
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
            .map(|mut q| std::mem::take(&mut *q))
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
            let _ = self
                .inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&id);
        }
        killed.sort_unstable();
        killed
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
            handle.done.notify_waiters();
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
            let pid = {
                let mut job = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                job.notify.enabled = false;
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
            handle.done.notify_waiters();
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
    #[allow(clippy::too_many_lines)]
    pub async fn job_spawn(&self, args: Value) -> Result<Value> {
        let cmd = args
            .get("cmd")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("jobSpawn: missing 'cmd'".into()))?
            .to_owned();
        let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);

        // Notifications at spawn. Default: terminal only. `notify: false`
        // silences everything; `notifyIntervalMs` (clamped to the floor)
        // turns on periodic pings without needing a second `jobNotify`
        // call. Passing the interval alone is enough — opting into periodic
        // ticks implies `notify: true`.
        let mut notify = NotifyOpts::terminal_only();
        if let Some(on) = args.get("notify").and_then(Value::as_bool) {
            notify.enabled = on;
        }
        if let Some(ms) = args.get("notifyIntervalMs").and_then(Value::as_u64) {
            notify.interval_ms = Some(ms.max(MIN_NOTIFY_INTERVAL_MS));
            notify.enabled = true;
        }

        // Terminal setup at spawn. `tty` opts into a pseudo-terminal; plain
        // jobs keep the original pipe-to-log behaviour. Idle defaults on for
        // tty jobs (where a prompt is silence) and off for plain jobs (where
        // silence is normal compute).
        let tty = args.get("tty").and_then(Value::as_bool).unwrap_or(false);
        let cols = clamp_dim(args.get("cols").and_then(Value::as_u64), DEFAULT_TTY_COLS);
        let rows = clamp_dim(args.get("rows").and_then(Value::as_u64), DEFAULT_TTY_ROWS);
        let mut idle_ms = args
            .get("idleMs")
            .and_then(Value::as_u64)
            .map(|ms| ms.max(MIN_IDLE_MS));
        if tty && idle_ms.is_none() {
            idle_ms = Some(DEFAULT_TTY_IDLE_MS);
        }

        if let Some(blocked) = self.check_policy(&cmd).await {
            return Ok(blocked);
        }

        let generation = self.jobs.inner.generation.load(Ordering::SeqCst);
        let id = self.jobs.inner.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let log_path = self
            .tmp_dir
            .join(format!("lofi-job-{id}.log"))
            .to_string_lossy()
            .into_owned();
        // Append mode so the shared stdout/stderr file interleaves both
        // streams at end-of-file with no offset race between the two fds.
        // For a tty job the child writes to the PTY slave instead; the driver
        // copies master bytes into this same file, so `jobRead` and
        // `lofi.read(logPath)` keep working unchanged.
        let log_file = std::fs::OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&log_path)
            .map_err(Error::Io)?;

        let pty = if tty {
            Some(open_pty(cols, rows).map_err(Error::Io)?)
        } else {
            None
        };
        // One clone for the driver reader, one for the handle writer side.
        let (driver_master, handle_master) = match &pty {
            Some(pty) => (
                Some(pty.master.try_clone().map_err(Error::Io)?),
                Some(pty.master.try_clone().map_err(Error::Io)?),
            ),
            None => (None, None),
        };

        let mut command = Command::new("sh");
        command.arg("-c").arg(&cmd).current_dir(&self.root);
        if let Some(pty) = &pty {
            let stdin = pty.slave.try_clone().map_err(Error::Io)?;
            let stdout = pty.slave.try_clone().map_err(Error::Io)?;
            let stderr = pty.slave.try_clone().map_err(Error::Io)?;
            command
                .stdin(std::process::Stdio::from(stdin))
                .stdout(std::process::Stdio::from(stdout))
                .stderr(std::process::Stdio::from(stderr));
            // Give the child its own session and controlling terminal so the
            // PTY line discipline turns a Ctrl+C byte into SIGINT and
            // `/dev/tty` works. Only async-signal-safe syscalls run here.
            #[allow(unsafe_code)]
            unsafe {
                command.as_std_mut().pre_exec(|| {
                    if nix::libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if nix::libc::ioctl(0, nix::libc::TIOCSCTTY, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            // One shared file for both streams: the shared offset keeps
            // merged output in arrival order with no async plumbing.
            let stderr_file = log_file.try_clone().map_err(Error::Io)?;
            command
                .stdout(std::process::Stdio::from(log_file))
                .stderr(std::process::Stdio::from(stderr_file))
                .process_group(0);
        }
        self.bash_env.apply(&mut command);
        if tty {
            command.env("TERM", "xterm-256color");
        }
        let mut child = command.spawn()?;
        let pid = child.id();

        let started = Instant::now();
        let handle = Arc::new(JobHandle {
            data: Mutex::new(Job {
                id,
                cmd: cmd.clone(),
                state: State::Running,
                pid,
                started,
                ended: None,
                exit_code: None,
                signal: None,
                log_path: log_path.clone(),
                timeout_ms,
                notify,
                tty,
                cols,
                rows,
                idle_ms,
                idle: false,
                last_output_at: Some(started),
            }),
            done: Notify::new(),
            cancel: std::sync::atomic::AtomicBool::new(false),
            on_release: Mutex::new(None),
            master: Mutex::new(handle_master),
        });
        {
            let mut jobs = self
                .jobs
                .inner
                .jobs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.jobs.inner.generation.load(Ordering::SeqCst) != generation {
                drop(jobs);
                if let Some(pid) = pid {
                    kill_pgrp(pid);
                }
                let _ = child.wait().await;
                return Err(Error::Tool("session changed while spawning job".into()));
            }
            let on_release = self.on_job_acquired.as_ref().and_then(|hook| hook(id));
            *handle
                .on_release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = on_release;
            jobs.insert(id, handle.clone());
        }

        // Start the driver only after the acquisition marker is published, so
        // even an immediately exiting child cannot finish first.
        tokio::spawn(run_job(
            Arc::downgrade(&self.jobs.inner),
            handle,
            child,
            timeout_ms,
            generation,
            driver_master,
            self.bash_env.clone(),
        ));

        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "state": "running",
            "command": cmd,
            "directory": self.root.display().to_string(),
            "pid": pid,
            "timeoutMs": timeout_ms,
            "logPath": log_path,
            "tty": tty,
            "cols": cols,
            "rows": rows,
            "idleMs": idle_ms,
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

        let (bytes, next, total) = match read_log_page(&log_path, cursor, limit).await {
            Ok(page) => page,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), 0, 0),
            Err(e) => return Err(Error::Io(e)),
        };
        let mut chunk = String::from_utf8_lossy(&bytes).into_owned();
        self.bash_env.redact(&mut chunk);
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "state": state.as_str(),
            "cursor": next,
            "totalBytes": total,
            "output": chunk,
            "done": state.is_terminal(),
        }))
    }

    /// Bounded wait for the job to reach a terminal state. Returns the
    /// final status, or the still-running status when `timeoutMs` elapses.
    /// Waiting never cancels the job, but user cancellation (ESC/Ctrl-C)
    /// breaks the wait early and returns the current state with
    /// `cancelled: true`.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    pub async fn job_wait(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);

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
        let timed = async {
            if let Some(ms) = timeout_ms {
                let _ = tokio::time::timeout(Duration::from_millis(ms), wait).await;
            } else {
                wait.await;
            }
        };
        tokio::pin!(timed);
        // Race the wait against user cancellation, mirroring `bash`: while
        // the guest awaits here, no QuickJS bytecode ticks, so the sandbox
        // interrupt handler cannot observe `cancel`. A cancelled wait leaves
        // the job running and returns the current state flagged `cancelled`.
        let cancelled = if let Some(flag) = &self.cancel {
            let cancel_wait = wait_for_cancel(flag);
            tokio::pin!(cancel_wait);
            tokio::select! {
                biased;
                () = &mut timed => false,
                () = &mut cancel_wait => true,
            }
        } else {
            timed.await;
            false
        };
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v = job.to_json(&self.root);
        if cancelled {
            v["cancelled"] = json!(true);
        }
        Ok(v)
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
    /// queues one notice (unless `enabled: false`); `intervalMs` turns on
    /// periodic progress pings while the job runs. With `changed` (the
    /// default) a tick only emits when the log grew since the last tick, so
    /// an idle-but-alive job stays quiet; `changed: false` emits every tick.
    /// `intervalMs` is clamped to a 5s floor and defaults to 30s.
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
        // Calling `jobNotify` at all means "notify me", so a missing
        // `enabled` key is treated as `true`.
        let enabled = args.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        job.notify.enabled = enabled;
        // Enabling without an explicit interval turns periodic pings on at
        // the default, so bare `job_notify({id})` does the useful thing.
        // `enabled: false` leaves the interval untouched.
        if enabled && job.notify.interval_ms.is_none() {
            job.notify.interval_ms = Some(DEFAULT_NOTIFY_INTERVAL_MS);
        }
        if let Some(ms) = args.get("intervalMs").and_then(Value::as_u64) {
            job.notify.interval_ms = Some(ms.max(MIN_NOTIFY_INTERVAL_MS));
        }
        if let Some(changed) = args.get("changed").and_then(Value::as_bool) {
            job.notify.changed = changed;
        }
        if let Some(ms) = args.get("idleMs").and_then(Value::as_u64) {
            job.idle_ms = Some(ms.max(MIN_IDLE_MS));
        }
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "notify": job.notify.enabled,
            "intervalMs": job.notify.interval_ms,
            "changed": job.notify.changed,
            "idleMs": job.idle_ms,
        }))
    }

    /// Write literal bytes to a tty job's PTY input. The agent uses this to
    /// answer a prompt; pair it with `jobKeyPress Enter` or include a
    /// trailing newline in `text`.
    /// # Errors
    /// Returns [`Error::Tool`] when `id`/`text` is missing, the job is not a
    /// tty job, or the PTY input buffer is full.
    #[allow(clippy::unused_async)]
    pub async fn job_type(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("jobType: missing 'text'".into()))?;
        if text.len() > MAX_TTY_WRITE_BYTES {
            return Err(Error::Tool("jobType: text too long".into()));
        }
        let sent = write_to_master(&handle, text.as_bytes())?;
        Ok(json!({ "ok": true, "id": id.to_string(), "sent": sent }))
    }

    /// Send one named key press to a tty job. Uses the same key names as the
    /// agent-facing `tu` terminal.
    /// # Errors
    /// Returns [`Error::Tool`] when `id`/`key` is missing or the key is not
    /// recognised.
    #[allow(clippy::unused_async)]
    pub async fn job_key_press(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let key = args
            .get("key")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("jobKeyPress: missing 'key'".into()))?;
        let bytes = key_press_bytes(key)
            .ok_or_else(|| Error::Tool(format!("jobKeyPress: unknown key '{key}'")))?;
        let sent = write_to_master(&handle, &bytes)?;
        if sent != bytes.len() {
            return Err(Error::Tool("jobKeyPress: partial PTY write".into()));
        }
        Ok(json!({ "ok": true, "id": id.to_string(), "key": key }))
    }

    /// Resize a tty job's pseudo-terminal. Full-screen programs observe this
    /// as a real terminal resize and reflow.
    /// # Errors
    /// Returns [`Error::Tool`] when `id`/`cols`/`rows` is missing or invalid,
    /// or [`Error::Io`] when the resize ioctl fails.
    #[allow(clippy::unused_async)]
    pub async fn job_resize(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        ensure_job_running(&handle)?;
        let cols = args
            .get("cols")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n > 0);
        let rows = args
            .get("rows")
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n > 0);
        let (Some(cols), Some(rows)) = (cols, rows) else {
            return Err(Error::Tool(
                "jobResize: missing or invalid 'cols'/'rows'".into(),
            ));
        };
        let cols = cols.min(MAX_TTY_DIM);
        let rows = rows.min(MAX_TTY_DIM);
        {
            let guard = handle
                .master
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(master) = guard.as_ref() else {
                return Err(Error::Tool("job is not a tty job".into()));
            };
            set_winsize(master, rows, cols)?;
        }
        {
            let mut job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            job.cols = cols;
            job.rows = rows;
        }
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "cols": cols,
            "rows": rows,
        }))
    }

    /// Bounded wait until a job is waiting for input. Two ways to wait:
    ///
    /// - `pattern` returns when the job's output tail contains the text.
    /// - `stableMs` returns when the job's output has been unchanged for
    ///   that long (the same signal the idle notice uses).
    ///
    /// Waiting never writes to the job. Pass at least one of `pattern` or
    /// `stableMs`; `timeoutMs` bounds the whole wait. The result stays
    /// `{ ok: true }` on timeout or user cancellation, with the reason
    /// flagged, mirroring `jobWait`.
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or neither wait mode is
    /// supplied.
    pub async fn job_wait_for_input(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let stable_ms = args.get("stableMs").and_then(Value::as_u64);
        if pattern.is_none() && stable_ms.is_none() {
            return Err(Error::Tool(
                "jobWaitForInput: pass 'pattern' or 'stableMs'".into(),
            ));
        }
        let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64);
        let deadline = timeout_ms.map(|ms| Instant::now() + Duration::from_millis(ms));
        let log_path = {
            let job = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            job.log_path.clone()
        };

        loop {
            {
                let job = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if job.state.is_terminal() {
                    return Ok(job.to_json(&self.root));
                }
            }
            if let Some(pattern) = &pattern {
                let tail = read_log_tail(&log_path, PATTERN_TAIL_BYTES);
                if tail.contains(pattern.as_str()) {
                    let mut v = json!({
                        "ok": true,
                        "id": id.to_string(),
                        "matched": pattern,
                    });
                    v["tail"] = json!(tail);
                    return Ok(v);
                }
            }
            if let Some(stable_ms) = stable_ms {
                let last = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .last_output_at;
                if last.is_some_and(|t| t.elapsed() >= Duration::from_millis(stable_ms)) {
                    return Ok(json!({
                        "ok": true,
                        "id": id.to_string(),
                        "idle": true,
                    }));
                }
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(json!({
                    "ok": true,
                    "id": id.to_string(),
                    "timedOut": true,
                }));
            }
            if let Some(flag) = &self.cancel {
                let cancel_wait = wait_for_cancel(flag);
                tokio::pin!(cancel_wait);
                tokio::select! {
                    biased;
                    () = tokio::time::sleep(JOB_POLL_INTERVAL) => {},
                    () = &mut cancel_wait => {
                        return Ok(json!({
                            "ok": true,
                            "id": id.to_string(),
                            "cancelled": true,
                        }));
                    }
                }
            } else {
                tokio::time::sleep(JOB_POLL_INTERVAL).await;
            }
        }
    }
}

async fn read_log_page(
    path: &str,
    cursor: u64,
    limit: usize,
) -> std::io::Result<(Vec<u8>, u64, u64)> {
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    let mut file = tokio::fs::File::open(path).await?;
    let total = file.metadata().await?.len();
    let start = cursor.min(total);
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let available = usize::try_from(total - start).unwrap_or(usize::MAX);
    let mut bytes = vec![0; limit.min(available)];
    let read = file.read(&mut bytes).await?;
    bytes.truncate(read);
    Ok((bytes, start + read as u64, total))
}

/// Clamp an optional PTY dimension to a valid, bounded window size.
/// Out-of-range values clamp to the allowed extremes rather than falling
/// back to the default, so an oversized request still gets the largest
/// supported terminal and `0` gets the smallest.
fn clamp_dim(v: Option<u64>, default: u16) -> u16 {
    v.map_or(default, |n| n.clamp(1, u64::from(MAX_TTY_DIM)) as u16)
}

/// Allocate a pseudo-terminal with the requested window size and put the
/// master in non-blocking mode so the driver poll never blocks on a silent
/// child.
fn open_pty(cols: u16, rows: u16) -> std::io::Result<nix::pty::OpenptyResult> {
    let winsize = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let pty = nix::pty::openpty(Some(&winsize), None::<&nix::sys::termios::Termios>)
        .map_err(std::io::Error::from)?;
    set_nonblock(pty.master.as_raw_fd())?;
    Ok(pty)
}

fn set_nonblock(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    let flags =
        nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL).map_err(std::io::Error::from)?;
    let flags = nix::fcntl::OFlag::from_bits_truncate(flags) | nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(flags)).map_err(std::io::Error::from)?;
    Ok(())
}

/// Map a named key to the byte sequence an xterm sends for it.
fn key_press_bytes(key: &str) -> Option<Vec<u8>> {
    let bytes: &[u8] = match key {
        "Enter" | "Return" => b"\r",
        "Tab" => b"\t",
        "Escape" | "Esc" => b"\x1b",
        "Backspace" => b"\x7f",
        "Delete" => b"\x1b[3~",
        "Insert" => b"\x1b[2~",
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        "PageUp" => b"\x1b[5~",
        "PageDown" => b"\x1b[6~",
        "Space" => b" ",
        "Ctrl+C" => b"\x03",
        "Ctrl+D" => b"\x04",
        "Ctrl+Z" => b"\x1a",
        "Ctrl+U" => b"\x15",
        "Ctrl+L" => b"\x0c",
        "Ctrl+A" => b"\x01",
        "Ctrl+E" => b"\x05",
        "F1" => b"\x1bOP",
        "F2" => b"\x1bOQ",
        "F3" => b"\x1bOR",
        "F4" => b"\x1bOS",
        "F5" => b"\x1b[15~",
        "F6" => b"\x1b[17~",
        "F7" => b"\x1b[18~",
        "F8" => b"\x1b[19~",
        "F9" => b"\x1b[20~",
        "F10" => b"\x1b[21~",
        "F11" => b"\x1b[23~",
        "F12" => b"\x1b[24~",
        _ => return None,
    };
    Some(bytes.to_vec())
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

fn write_to_master(handle: &JobHandle, bytes: &[u8]) -> Result<usize> {
    ensure_job_running(handle)?;
    let guard = handle
        .master
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(master) = guard.as_ref() else {
        return Err(Error::Tool("job is not a tty job".into()));
    };
    let mut written = 0;
    while written < bytes.len() {
        match nix::unistd::write(master.as_fd(), &bytes[written..]) {
            Ok(0) => return Ok(written),
            Ok(n) => written += n,
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                if written > 0 {
                    return Ok(written);
                }
                return Err(Error::Tool("pty input buffer full; retry".into()));
            }
            Err(e) => return Err(Error::Io(std::io::Error::from(e))),
        }
    }
    Ok(written)
}

fn set_winsize(master: &OwnedFd, rows: u16, cols: u16) -> Result<()> {
    let size = nix::pty::Winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // TIOCSWINSZ is the only way to deliver a real PTY resize to the child.
    #[allow(unsafe_code)]
    let rc =
        unsafe { nix::libc::ioctl(master.as_raw_fd(), nix::libc::TIOCSWINSZ, &raw const size) };
    if rc == -1 {
        return Err(Error::Io(std::io::Error::last_os_error()));
    }
    Ok(())
}

/// Drain whatever the PTY master has ready, appending raw bytes to the log
/// and feeding the same bytes to the screen parser. Returns without blocking;
/// EAGAIN/EWOULDBLOCK means no more data, EIO means the slave side closed.
fn drain_master(
    master: &OwnedFd,
    parser: &mut Option<vt100::Parser>,
    log_out: &mut Option<std::fs::File>,
) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        match nix::unistd::read(master.as_raw_fd(), &mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(out) = log_out.as_mut() {
                    out.write_all(&buf[..n])?;
                }
                if let Some(parser) = parser.as_mut() {
                    parser.process(&buf[..n]);
                }
            }
            Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                break;
            }
            Err(nix::errno::Errno::EIO) => break,
            Err(e) => return Err(std::io::Error::from(e)),
        }
    }
    Ok(())
}

fn screen_changed(parser: Option<&vt100::Parser>, last: &mut Option<String>) -> bool {
    let Some(parser) = parser else {
        return false;
    };
    let contents = parser.screen().contents();
    if last.as_deref() == Some(contents.as_str()) {
        return false;
    }
    *last = Some(contents);
    true
}

fn sync_parser_size(parser: &mut Option<vt100::Parser>, handle: &JobHandle) {
    let Some(parser) = parser else {
        return;
    };
    let (cols, rows) = {
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (job.cols, job.rows)
    };
    if parser.screen().size() != (rows, cols) {
        parser.screen_mut().set_size(rows, cols);
    }
}

/// Output-idle bookkeeping for one running job. Screen text is the change
/// signal for a tty job (so cursor movement or redraws do not count as work);
/// log byte count is the signal for a plain job.
struct IdleTracker {
    last_bytes: u64,
    last_screen: Option<String>,
    last_output_at: Instant,
    idle: bool,
}

impl IdleTracker {
    fn new() -> Self {
        Self {
            last_bytes: 0,
            last_screen: None,
            last_output_at: Instant::now(),
            idle: false,
        }
    }
}

/// Update activity timestamps from the current output. Returns true when the
/// output changed since the previous poll. `job.last_output_at` is always
/// tracked so `jobWaitForInput` stability waits work even with idle notices
/// disabled.
fn update_activity(
    handle: &JobHandle,
    parser: Option<&vt100::Parser>,
    log_path: &str,
    tracker: &mut IdleTracker,
) -> bool {
    let changed = if parser.is_some() {
        screen_changed(parser, &mut tracker.last_screen)
    } else {
        let bytes = std::fs::metadata(log_path).map_or(0, |m| m.len());
        if bytes == tracker.last_bytes {
            false
        } else {
            tracker.last_bytes = bytes;
            true
        }
    };
    if changed {
        let now = Instant::now();
        tracker.last_output_at = now;
        tracker.idle = false;
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.last_output_at = Some(now);
        job.idle = false;
    }
    changed
}

/// Emit one edge-triggered idle notice when output has been still for the
/// configured threshold. Returns `None` while active or before the threshold.
fn observe_idle(
    handle: &JobHandle,
    parser: Option<&vt100::Parser>,
    log_path: &str,
    tracker: &mut IdleTracker,
    bash_env: &crate::BashEnv,
) -> Option<String> {
    if update_activity(handle, parser, log_path, tracker) {
        return None;
    }
    let (idle_ms, notify_enabled) = {
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (job.idle_ms, job.notify.enabled)
    };
    let idle_ms = idle_ms?;
    let now = Instant::now();
    if tracker.idle || now.duration_since(tracker.last_output_at) < Duration::from_millis(idle_ms) {
        return None;
    }
    tracker.idle = true;
    {
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        job.idle = true;
    }
    if !notify_enabled {
        return None;
    }
    let tail = idle_tail(parser, log_path, bash_env);
    let id = handle
        .data
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .id;
    Some(format!("job {id} idle (no output for {idle_ms}ms): {tail}"))
}

fn idle_tail(parser: Option<&vt100::Parser>, log_path: &str, bash_env: &crate::BashEnv) -> String {
    let mut tail = if let Some(parser) = parser {
        let contents = parser.screen().contents();
        contents.lines().last().unwrap_or("").trim_end().to_owned()
    } else {
        let redact_overlap = bash_env.redact.iter().map(String::len).max().unwrap_or(0);
        read_log_tail(log_path, 200usize.saturating_add(redact_overlap))
            .trim()
            .to_owned()
    };
    bash_env.redact(&mut tail);
    short_text(&tail, 80)
}

fn read_log_tail(path: &str, max: usize) -> String {
    use std::io::{Read as _, Seek as _};

    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(total) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let start = total.saturating_sub(max as u64);
    if file.seek(std::io::SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity((total - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Collapse control characters to spaces and cap the visible length at a
/// char boundary for one-line notices.
fn short_text(s: &str, max: usize) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect();
    if s.chars().count() > max {
        out.push('\u{2026}');
    }
    out
}

/// Drive a spawned child to completion. Polls `try_wait` so cancellation,
/// the (optional) timeout, and the log-size cap are observed on one clock; each
/// terminal transition updates the job record, wakes `jobWait` listeners,
/// and queues the completion notice for the host agent.
// The driver takes only the JobRegistry it needs to publish notices. Taking
// the whole BuiltinTools would leak its tool callback (an UnboundedSender per
// execute_tools round) into the spawned task's lifetime, keeping the
// engine-to-UI channel — and therefore the run — alive until the job exits.
#[allow(clippy::too_many_lines)]
async fn run_job(
    jobs: std::sync::Weak<Inner>,
    handle: Arc<JobHandle>,
    mut child: tokio::process::Child,
    timeout_ms: Option<u64>,
    generation: u64,
    master: Option<OwnedFd>,
    bash_env: crate::BashEnv,
) {
    let (tty, cols, rows, log_path) = {
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (job.tty, job.cols, job.rows, job.log_path.clone())
    };
    // The screen parser is driver-local: the writer tools (`jobType` etc.)
    // only need the master fd, and `jobWaitForInput` reads the shared log.
    let mut parser = if tty {
        Some(vt100::Parser::new(rows, cols, 0))
    } else {
        None
    };
    let mut log_out = if tty {
        std::fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .ok()
    } else {
        None
    };
    let mut idle_tracker = IdleTracker::new();
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
            // `jobKill` already set the state; just reap.
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
            if let Some(pid) = child.id() {
                kill_pgrp(pid);
            }
            let _ = child.wait().await;
            guard.disarm();
            outcome = Some((State::TimedOut, None, Some(nix::libc::SIGKILL)));
            break;
        }
        if let Some(master) = &master {
            let _ = drain_master(master, &mut parser, &mut log_out);
            sync_parser_size(&mut parser, &handle);
        }
        if let Some(text) = observe_idle(
            &handle,
            parser.as_ref(),
            &log_path,
            &mut idle_tracker,
            &bash_env,
        ) {
            push_notice(&jobs, generation, text);
        }
        if let Some(text) = progress_tick(&handle, &mut last_tick, &mut last_bytes) {
            push_notice(&jobs, generation, text);
        }
        tokio::time::sleep(JOB_POLL_INTERVAL).await;
    }

    // The child may have written its final bytes into the PTY buffer before
    // exiting; drain them before the log handle closes so `jobRead` sees the
    // complete transcript.
    if let Some(master) = &master {
        let _ = drain_master(master, &mut parser, &mut log_out);
    }
    if let Some(mut out) = log_out {
        let _ = out.flush();
    }
    handle
        .master
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let notice = {
        let mut job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((state, exit_code, signal)) = outcome {
            // A `jobKill` that won the race already set Cancelled; do not
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
    let hook = handle
        .on_release
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(hook) = hook {
        let id = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .id;
        hook(id);
    }
    handle.done.notify_waiters();
    if let Some(text) = notice {
        push_notice(&jobs, generation, text);
    }
}

/// Publish a notice to every live subscriber, or buffer it for a future
/// subscriber when the registry is headless (tests, pre-UI agent).
fn push_notice(registry: &std::sync::Weak<Inner>, generation: u64, text: String) {
    let Some(inner) = registry.upgrade() else {
        return;
    };
    if inner.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    let mut subs = inner
        .subscribers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Reset increments the generation before it clears subscribers. Check
    // again under the subscriber lock so a publisher that paused after its
    // first check cannot enqueue an old-session notice after the clear.
    if inner.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    // send() on an unbounded channel fails iff the receiver is dropped; prune
    // dead subscribers on publish so we never hold stale senders.
    subs.retain(|tx| tx.send(text.clone()).is_ok());
    if subs.is_empty() {
        inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(text);
    }
}

/// Truncate a command line to 60 characters, appending U+2026 when more
/// follows. Multi-byte chars are kept whole; truncation is at a char
/// boundary so the ellipsis never lands mid-codepoint.
fn truncate_cmd(cmd: &str) -> String {
    let mut out = String::new();
    let mut chars = cmd.chars();
    for _ in 0..60 {
        match chars.next() {
            Some(c) => out.push(c),
            None => break,
        }
    }
    if chars.next().is_some() {
        out.push('\u{2026}');
    }
    out
}

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
    let short = truncate_cmd(&cmd);
    Some(format!(
        "job {id} running {secs}s, {bytes} bytes logged: {short}"
    ))
}

fn format_notice(job: &Job) -> String {
    use std::fmt::Write as _;
    let cmd = truncate_cmd(&job.cmd);
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
                tty: false,
                cols: DEFAULT_TTY_COLS,
                rows: DEFAULT_TTY_ROWS,
                idle_ms: None,
                idle: false,
                last_output_at: None,
            }),
            done: Notify::new(),
            cancel: AtomicBool::new(false),
            on_release: Mutex::new(None),
            master: Mutex::new(None),
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

    #[test]
    fn shutdown_cancels_every_running_job() {
        let registry = JobRegistry::new();
        let first_log = tempfile::NamedTempFile::new().unwrap();
        let second_log = tempfile::NamedTempFile::new().unwrap();
        let first = handle_with(
            NotifyOpts::terminal_only(),
            first_log.path().to_string_lossy().into_owned(),
        );
        let second = handle_with(
            NotifyOpts::terminal_only(),
            second_log.path().to_string_lossy().into_owned(),
        );
        second.data.lock().unwrap().id = 2;
        {
            let mut jobs = registry.inner.jobs.lock().unwrap();
            jobs.insert(1, first.clone());
            jobs.insert(2, second.clone());
        }

        registry.shutdown();

        for handle in [first, second] {
            assert_eq!(handle.data.lock().unwrap().state, State::Cancelled);
            assert!(handle.cancel.load(Ordering::Relaxed));
        }
    }

    #[test]
    fn reset_ends_the_old_scope_and_rejects_its_late_notices() {
        let registry = JobRegistry::new();
        let old_generation = registry.inner.generation.load(Ordering::SeqCst);
        let mut old_receiver = registry.subscribe_notices();
        let log = tempfile::NamedTempFile::new().unwrap();
        let handle = handle_with(
            NotifyOpts::terminal_only(),
            log.path().to_string_lossy().into_owned(),
        );
        let id = handle.data.lock().unwrap().id;
        registry
            .inner
            .jobs
            .lock()
            .unwrap()
            .insert(id, handle.clone());
        registry
            .inner
            .pending
            .lock()
            .unwrap()
            .push("buffered".into());

        registry.reset();

        assert!(registry.live_ids().is_empty());
        assert!(registry.drain_notices().is_empty());
        assert_eq!(handle.data.lock().unwrap().state, State::Cancelled);
        assert_eq!(handle.data.lock().unwrap().signal, Some(nix::libc::SIGKILL));
        assert!(handle.cancel.load(Ordering::Relaxed));
        assert!(old_receiver.try_recv().is_err());

        let weak = Arc::downgrade(&registry.inner);
        push_notice(&weak, old_generation, "stale".into());
        let current_generation = registry.inner.generation.load(Ordering::SeqCst);
        let mut current_receiver = registry.subscribe_notices();
        assert!(current_receiver.try_recv().is_err());
        push_notice(&weak, current_generation, "current".into());
        assert_eq!(current_receiver.try_recv().unwrap(), "current");
    }

    #[test]
    fn kill_not_in_disables_notify_before_killing() {
        // Regression: /tree rollback reconcile used to kill the off-lineage
        // job with notify still enabled, so the driver poll saw the
        // Cancelled state and pushed a "cancelled: <cmd>" notice that the UI
        // then delivered to the agent as a user-role message. The user
        // already opted the job out of the conversation by rolling back;
        // nothing should reach the agent.
        let registry = JobRegistry::new();
        let f = tempfile::NamedTempFile::new().unwrap();
        let handle = handle_with(
            NotifyOpts::terminal_only(),
            f.path().to_string_lossy().into_owned(),
        );
        let id = handle.data.lock().unwrap().id;
        registry
            .inner
            .jobs
            .lock()
            .unwrap()
            .insert(id, handle.clone());
        assert_eq!(registry.live_ids(), std::collections::HashSet::from([id]));

        let killed = registry.kill_not_in(&[]);
        assert_eq!(killed, vec![id]);

        // Job is gone from the registry...
        assert!(registry.live_ids().is_empty());
        // ...and notify was disabled before terminal, so the driver poll
        // (if it were running) would never push the "cancelled" notice.
        let job = handle.data.lock().unwrap();
        assert!(job.state.is_terminal());
        assert!(!job.notify.enabled);
    }

    fn tools_with_cancel(cancel: Arc<AtomicBool>) -> (tempfile::TempDir, BuiltinTools) {
        tools_with_env(cancel, crate::BashEnv::default())
    }

    fn tools_with_env(
        cancel: Arc<AtomicBool>,
        bash_env: crate::BashEnv,
    ) -> (tempfile::TempDir, BuiltinTools) {
        let dir = tempfile::tempdir().unwrap();
        let auto: crate::AutoModeFn =
            Arc::new(|_| Box::pin(async { crate::AutoModeOutcome::Allow { reason: "t".into() } }));
        let tmp = dir.path().join("lofi-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let tools = BuiltinTools::with_skills_dir(
            dir.path().to_path_buf(),
            None,
            tmp,
            bash_env,
            crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
            None,
            Some(auto),
            None,
        )
        .with_cancel(Some(cancel));
        (dir, tools)
    }

    #[tokio::test]
    async fn job_wait_settles_promptly_on_cancel() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel.clone());
        let spawned = tools
            .job_spawn(json!({ "cmd": "sleep 30", "timeoutMs": 60_000 }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();

        let waiter = tokio::spawn({
            let tools = tools.clone();
            async move { tools.job_wait(json!({ "id": id })).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.store(true, Ordering::Relaxed);
        let settled = tokio::time::timeout(Duration::from_secs(5), waiter).await;
        assert!(settled.is_ok(), "job_wait must settle promptly on cancel");
        let res = settled.unwrap().unwrap();
        assert_eq!(res["cancelled"], json!(true), "got: {res}");
        assert_eq!(res["state"], json!("running"), "got: {res}");

        // The wait was interrupted, not the job: it must still be alive and
        // respond to an explicit kill.
        let killed = tools.job_kill(json!({ "id": res["id"] })).await.unwrap();
        assert_eq!(killed["killed"], json!(true), "got: {killed}");
    }

    #[tokio::test]
    async fn job_wait_completes_normally_without_cancel() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools.job_spawn(json!({ "cmd": "true" })).await.unwrap();
        let res = tools
            .job_wait(json!({ "id": spawned["id"], "timeoutMs": 5_000 }))
            .await
            .unwrap();
        assert_eq!(res["state"], json!("completed"), "got: {res}");
        assert!(res.get("cancelled").is_none(), "got: {res}");
    }

    #[test]
    fn key_press_bytes_maps_named_keys() {
        assert_eq!(key_press_bytes("Enter"), Some(b"\r".to_vec()));
        assert_eq!(key_press_bytes("Tab"), Some(b"\t".to_vec()));
        assert_eq!(key_press_bytes("Up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_press_bytes("Ctrl+C"), Some(b"\x03".to_vec()));
        assert_eq!(key_press_bytes("F12"), Some(b"\x1b[24~".to_vec()));
        assert_eq!(key_press_bytes("NotAKey"), None);
    }

    #[test]
    fn read_log_tail_reads_only_the_requested_suffix() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"prefix-suffix").unwrap();

        assert_eq!(read_log_tail(file.path().to_str().unwrap(), 6), "suffix");
    }

    #[test]
    fn idle_tail_redacts_approved_environment_values() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"token=secret-value").unwrap();
        let bash_env = crate::BashEnv {
            redact: vec!["secret-value".to_string()],
            ..crate::BashEnv::default()
        };

        assert_eq!(
            idle_tail(None, file.path().to_str().unwrap(), &bash_env),
            "token=[redacted]"
        );
    }

    #[tokio::test]
    async fn tty_job_accepts_typed_input_and_key_presses() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "read x; echo \"got:$x\"", "tty": true }))
            .await
            .unwrap();
        assert_eq!(spawned["tty"], json!(true));
        let id = spawned["id"].as_str().unwrap().to_owned();

        let typed = tools
            .job_type(json!({ "id": id, "text": "hello" }))
            .await
            .unwrap();
        assert_eq!(typed["sent"], json!(5));
        let pressed = tools
            .job_key_press(json!({ "id": id, "key": "Enter" }))
            .await
            .unwrap();
        assert_eq!(pressed["ok"], json!(true));

        let done = tools
            .job_wait(json!({ "id": id, "timeoutMs": 5_000 }))
            .await
            .unwrap();
        assert_eq!(done["state"], json!("completed"), "got: {done}");
        let log = tools.job_read(json!({ "id": id })).await.unwrap();
        assert!(
            log["output"].as_str().unwrap().contains("got:hello"),
            "log: {log}"
        );
    }

    #[tokio::test]
    async fn tty_job_sets_term_after_applying_the_stripped_environment() {
        let cancel = Arc::new(AtomicBool::new(false));
        let bash_env = crate::BashEnv {
            strip_env: true,
            baseline: vec![
                (
                    "PATH".to_string(),
                    std::env::var("PATH").unwrap_or_default(),
                ),
                ("TERM".to_string(), "dumb".to_string()),
            ],
            ..crate::BashEnv::default()
        };
        let (_dir, tools) = tools_with_env(cancel, bash_env);
        let spawned = tools
            .job_spawn(json!({ "cmd": "printf %s \"$TERM\"", "tty": true }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let done = tools
            .job_wait(json!({ "id": id, "timeoutMs": 5_000 }))
            .await
            .unwrap();
        assert_eq!(done["state"], json!("completed"), "got: {done}");
        let log = tools.job_read(json!({ "id": id })).await.unwrap();
        assert!(
            log["output"].as_str().unwrap().contains("xterm-256color"),
            "log: {log}"
        );
    }

    #[tokio::test]
    async fn tty_input_rejects_a_completed_job() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "true", "tty": true }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        tools.job_wait(json!({ "id": id })).await.unwrap();

        let err = tools
            .job_type(json!({ "id": id, "text": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not running"), "got: {err}");
    }

    #[tokio::test]
    async fn job_type_rejects_a_non_tty_job() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "sleep 30", "notify": false }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let err = tools
            .job_type(json!({ "id": id, "text": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a tty job"), "got: {err}");
        tools.job_kill(json!({ "id": id })).await.unwrap();
    }

    #[tokio::test]
    async fn tty_job_queues_one_idle_notice() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "printf 'Name? '; read x; echo \"got:$x\"", "tty": true, "idleMs": 500 }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        // One idle window plus a little scheduling slack.
        tokio::time::sleep(Duration::from_millis(900)).await;
        let notices = tools.jobs.drain_notices();
        let idle = notices.iter().find(|n| n.contains("idle"));
        assert!(idle.is_some(), "notices: {notices:?}");
        assert!(idle.unwrap().contains("Name?"), "notices: {notices:?}");
        tools.job_kill(json!({ "id": id })).await.unwrap();
    }

    #[tokio::test]
    async fn job_wait_for_input_matches_a_prompt_pattern() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "printf 'READY\n'; read x; echo \"got:$x\"", "tty": true }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let waited = tools
            .job_wait_for_input(json!({ "id": id, "pattern": "READY", "timeoutMs": 5_000 }))
            .await
            .unwrap();
        assert_eq!(waited["matched"], json!("READY"), "got: {waited}");
        tools
            .job_type(json!({ "id": id, "text": "done\n" }))
            .await
            .unwrap();
        let done = tools
            .job_wait(json!({ "id": id, "timeoutMs": 5_000 }))
            .await
            .unwrap();
        assert_eq!(done["state"], json!("completed"), "got: {done}");
    }

    #[tokio::test]
    async fn job_wait_for_input_returns_after_stable_silence() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "printf 'prompt'; read x; echo done", "tty": true }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let waited = tools
            .job_wait_for_input(json!({ "id": id, "stableMs": 500, "timeoutMs": 5_000 }))
            .await
            .unwrap();
        assert_eq!(waited["idle"], json!(true), "got: {waited}");
        tools.job_kill(json!({ "id": id })).await.unwrap();
    }

    #[tokio::test]
    async fn job_wait_for_input_treats_no_output_as_stable() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({
                "cmd": "sleep 30",
                "tty": true,
                "notify": false
            }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let waited = tools
            .job_wait_for_input(json!({ "id": id, "stableMs": 500, "timeoutMs": 2_000 }))
            .await
            .unwrap();
        assert_eq!(waited["idle"], json!(true), "got: {waited}");
        tools.job_kill(json!({ "id": id })).await.unwrap();
    }

    #[tokio::test]
    async fn job_resize_updates_tty_status() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "sleep 30", "tty": true, "notify": false }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let resized = tools
            .job_resize(json!({ "id": id, "cols": 100, "rows": 30 }))
            .await
            .unwrap();
        assert_eq!(resized["cols"], json!(100));
        assert_eq!(resized["rows"], json!(30));
        let status = tools.job_status(json!({ "id": id })).await.unwrap();
        assert_eq!(status["cols"], json!(100));
        assert_eq!(status["rows"], json!(30));
        tools.job_kill(json!({ "id": id })).await.unwrap();
    }

    #[tokio::test]
    async fn job_wait_for_input_requires_a_wait_mode() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let spawned = tools
            .job_spawn(json!({ "cmd": "sleep 30", "notify": false }))
            .await
            .unwrap();
        let id = spawned["id"].as_str().unwrap().to_owned();
        let err = tools
            .job_wait_for_input(json!({ "id": id }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("pattern"), "got: {err}");
        tools.job_kill(json!({ "id": id })).await.unwrap();
    }
}
