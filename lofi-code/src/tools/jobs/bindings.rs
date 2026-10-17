//! `QuickJS` tool bindings for the job API. Spawning wires a job into the
//! registry and the run driver; the remaining calls inspect, page, type
//! into, cancel, and reconfigure jobs by id.

use std::os::unix::process::CommandExt;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{json, Value};
use tokio::process::Command;

use lofi_error::{Error, Result};

use super::driver::run_job;

/// Maximum concurrently running jobs one subagent may hold. Bounds a
/// runaway child without capping the session's own jobs.
const MAX_JOBS_PER_OWNER: usize = 16;
use super::pty::{key_press_bytes, open_pty, write_to_master, JobScreen, JobScreenLine};
use super::{
    kill_pgrp, Job, JobHandle, NotifyOpts, State, DEFAULT_NOTIFY_INTERVAL_MS, DEFAULT_TTY_COLS,
    DEFAULT_TTY_IDLE_MS, DEFAULT_TTY_ROWS, MAX_JOB_READ_BYTES, MAX_TTY_DIM, MAX_TTY_WRITE_BYTES,
    MIN_IDLE_MS, MIN_NOTIFY_INTERVAL_MS,
};
use crate::tools::BuiltinTools;

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

        // Bound one subagent's process tree; the session itself is
        // uncapped here (its jobs end with the turn or the session).
        if let Some(owner) = &self.job_owner {
            if self.jobs.running_count_for(owner) >= MAX_JOBS_PER_OWNER {
                return Ok(json!({
                    "ok": false,
                    "error": format!("subagent job cap reached ({MAX_JOBS_PER_OWNER} running)"),
                }));
            }
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
                    if nix::libc::ioctl(0, nix::libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else {
            // One shared file for both streams: the shared offset keeps
            // merged output in arrival order with no async plumbing. Stdin
            // is null: the TUI reader thread owns the terminal stdin, and
            // inheriting it lets the job win the read race and swallow
            // typed keys.
            let stderr_file = log_file.try_clone().map_err(Error::Io)?;
            command
                .stdin(std::process::Stdio::null())
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
                owner: self.job_owner.clone(),
                tty,
                cols,
                rows,
                idle_ms,
                idle: false,
                last_output_at: Some(started),
            }),
            cancel: std::sync::atomic::AtomicBool::new(false),
            on_release: Mutex::new(None),
            master: Mutex::new(handle_master),
            screen: Mutex::new(tty.then(|| JobScreen {
                cols,
                rows,
                lines: vec![JobScreenLine::default(); usize::from(rows)],
            })),
            application_cursor: std::sync::atomic::AtomicBool::new(false),
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

    /// List every job this session spawned, newest first. Each entry is the
    /// same shape as `jobStatus`. Session-scoped: jobs from other sessions
    /// are not visible.
    // Async for symmetry with the other job tools (and a uniform binding
    // shape), though the body is synchronous; the sandbox binding awaits it.
    /// # Errors
    /// Infallible in practice; the `Result` shape is for binding uniformity
    /// and the payload is always `{ ok: true, jobs: [...] }`.
    #[allow(clippy::unused_async)]
    pub async fn job_list(&self, _args: Value) -> Result<Value> {
        let entries: Vec<Value> = self
            .jobs
            .list()
            .into_iter()
            .map(|handle| {
                let job = handle
                    .data
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                job.to_json(&self.root)
            })
            .collect();
        Ok(json!({ "ok": true, "jobs": entries }))
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

    /// Read the latest visible screen of a PTY job after terminal control
    /// sequences have been applied. Display styles stay in the UI snapshot;
    /// the agent receives one redacted string per terminal row.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] when `id` is missing or invalid.
    #[allow(clippy::unused_async)]
    pub async fn job_screen(&self, args: Value) -> Result<Value> {
        let id = parse_id(&args)?;
        let Some(handle) = self.jobs.get(id) else {
            return Ok(no_such_job(id));
        };
        let screen = handle
            .screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let Some(screen) = screen else {
            return Ok(json!({
                "ok": false,
                "id": id.to_string(),
                "error": "job has no terminal screen",
            }));
        };
        let lines: Vec<String> = screen
            .lines
            .iter()
            .map(|line| {
                let mut text: String = line.spans.iter().map(|span| span.text.as_str()).collect();
                self.bash_env.redact(&mut text);
                text.trim_end().to_string()
            })
            .collect();
        Ok(json!({
            "ok": true,
            "id": id.to_string(),
            "cols": screen.cols,
            "rows": screen.rows,
            "lines": lines,
        }))
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
        let application_cursor = handle
            .application_cursor
            .load(std::sync::atomic::Ordering::Relaxed);
        let bytes = key_press_bytes(key, application_cursor)
            .ok_or_else(|| Error::Tool(format!("jobKeyPress: unknown key '{key}'")))?;
        let sent = write_to_master(&handle, bytes)?;
        if sent != bytes.len() {
            return Err(Error::Tool("jobKeyPress: partial PTY write".into()));
        }
        Ok(json!({ "ok": true, "id": id.to_string(), "key": key }))
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
