//! Job driver: poll a spawned child to completion, drain its PTY into the
//! log, and publish idle, progress, and completion notices.

use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::pty::{drain_pty, observe_idle, IdleTracker};
use super::{
    kill_pgrp, Inner, Job, JobHandle, JobNotice, State, JOB_POLL_INTERVAL, MAX_TERMINAL_JOBS,
};
use crate::tools::PgrpKillGuard;

/// Drive a spawned child to completion. Polls `try_wait` so cancellation,
/// the (optional) timeout, and the log-size cap are observed on one clock; each
/// terminal transition updates the job record and queues the completion notice
/// for the host agent.
// The driver takes only the JobRegistry it needs to publish notices. Taking
// the whole BuiltinTools would leak its tool callback (an UnboundedSender per
// execute_tools round) into the spawned task's lifetime, keeping the
// engine-to-UI channel — and therefore the run — alive until the job exits.
#[allow(clippy::too_many_lines)]
pub(super) async fn run_job(
    jobs: std::sync::Weak<Inner>,
    handle: Arc<JobHandle>,
    mut child: tokio::process::Child,
    timeout_ms: Option<u64>,
    generation: u64,
    master: Option<OwnedFd>,
    bash_env: crate::BashEnv,
) {
    let (id, tty, cols, rows, log_path) = {
        let job = handle
            .data
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (job.id, job.tty, job.cols, job.rows, job.log_path.clone())
    };
    // The screen parser is driver-local. Writer tools only need the master fd.
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
            drain_pty(&handle, master, &mut parser, &mut log_out);
        }
        if let Some(text) = observe_idle(
            &handle,
            parser.as_ref(),
            &log_path,
            &mut idle_tracker,
            &bash_env,
        ) {
            push_notice(
                &jobs,
                generation,
                JobNotice {
                    id,
                    terminal: false,
                    text,
                },
            );
        }
        if let Some(text) = progress_tick(&handle, &mut last_tick, &mut last_bytes) {
            push_notice(
                &jobs,
                generation,
                JobNotice {
                    id,
                    terminal: false,
                    text,
                },
            );
        }
        tokio::time::sleep(JOB_POLL_INTERVAL).await;
    }

    // The child may have written its final bytes into the PTY buffer before
    // exiting; drain them before the log handle closes so `jobRead` sees the
    // complete transcript.
    if let Some(master) = &master {
        drain_pty(&handle, master, &mut parser, &mut log_out);
    }
    if let Some(mut out) = log_out {
        let _ = out.flush();
    }
    handle
        .master
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    // Take the notice queue before the job record: the notification area
    // shows a finished job only while its terminal notice waits in the
    // queue, so the end state and the notice must become visible together.
    // A frame between them would drop the running line before the notice
    // that replaces it exists.
    let inner = jobs.upgrade();
    let mut pending = inner.as_ref().map(|inner| {
        inner
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    });
    {
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
        let fresh = inner
            .as_ref()
            .is_some_and(|inner| inner.generation.load(Ordering::SeqCst) == generation);
        if job.notify.enabled && fresh {
            let text = format_notice(&job);
            if let Some(pending) = pending.as_mut() {
                pending.push(JobNotice {
                    id,
                    terminal: true,
                    text,
                });
            }
        }
    }
    // Release the queue before the release hook: a hook may spawn or kill
    // jobs, and those paths take the same locks.
    drop(pending);
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
    if let Some(inner) = inner {
        evict_terminal_excess(&inner);
    }
}

/// Keep at most [`MAX_TERMINAL_JOBS`] terminal records, evicting the oldest
/// first. Running jobs are never evicted. An evicted record's log file is
/// removed: the record no longer exists, so nothing can page the log.
pub(super) fn evict_terminal_excess(inner: &Inner) {
    let mut jobs = inner
        .jobs
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut terminal: Vec<u64> = jobs
        .iter()
        .filter(|(_, h)| {
            h.data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .state
                .is_terminal()
        })
        .map(|(id, _)| *id)
        .collect();
    if terminal.len() <= MAX_TERMINAL_JOBS {
        return;
    }
    // Job ids mint from a monotonic counter, so ascending id is oldest first.
    terminal.sort_unstable();
    for id in &terminal[..terminal.len() - MAX_TERMINAL_JOBS] {
        if let Some(handle) = jobs.remove(id) {
            let log_path = handle
                .data
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .log_path
                .clone();
            let _ = std::fs::remove_file(log_path);
        }
    }
}

/// Queue a notice for the host to take. The queue is the only state: while
/// a terminal notice waits here, the job keeps its line in the notification
/// area, and taking the notice removes both together.
pub(super) fn push_notice(registry: &std::sync::Weak<Inner>, generation: u64, notice: JobNotice) {
    let Some(inner) = registry.upgrade() else {
        return;
    };
    if inner.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    let mut pending = inner
        .pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Reset increments the generation before it clears the queue. Check again
    // under the queue lock so a publisher that paused after its first check
    // cannot enqueue an old-session notice after the clear.
    if inner.generation.load(Ordering::SeqCst) != generation {
        return;
    }
    pending.push(notice);
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

pub(super) fn progress_tick(
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
