use super::truncate::{format_size, truncate_tail, DEFAULT_MAX_BYTES};
use super::util::{read_capped, PgrpKillGuard};
#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use std::fmt::Write as _;
use serde_json::{json, Value};
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

impl BuiltinTools {
    /// Run `cmd` via `sh -c` with cwd pinned to the root.
    ///
    /// stdout and stderr are merged. `timeoutMs` bounds the run (default
    /// 120s); on timeout the child is killed and `status: "timeout"` is returned
    /// (with `output: "<timeout>"`, `code: null`).
    ///
    /// The result is structured: `ok`, `output`, `code` (exit status, null on
    /// signal/timeout), `command`, `directory` (cwd), `signal` (Unix signal
    /// number, null unless killed by a signal), `duration_ms`, and `status`
    /// (`"exited"`, `"signaled"`, or `"timeout"`).
    ///
    /// Output is tail-truncated to 50 KB / 2000 lines (whichever is hit
    /// first), keeping the end where errors and final results land. When
    /// truncated, the full captured output is written to a temp file under
    /// the session tmp dir and its absolute path is included in the notice
    /// so the model can `lofi.read` it in pages (the tmp dir is a read root).
    ///
    /// Evaluate `cmd` against the shell policy. Returns `Some(json)` if the
    /// command is denied or needs confirmation, `None` if allowed.
    /// When `confirm` is set and the decision is `Ask`, awaits the callback;
    /// if the user approves, returns `None` (proceed).
    async fn check_policy(&self, cmd: &str) -> Option<Value> {
        let decision = self.shell_policy.evaluate(cmd);
        let suffix = decision
            .matched_command
            .as_deref()
            .map(|c| format!(" (command: {c})"))
            .unwrap_or_default();
        match decision.action {
            lofi_types::PolicyAction::Deny => Some(json!({
                "ok": false,
                "output": format!("blocked by shell policy: {}{}", decision.reason, suffix),
                "code": Value::Null,
                "command": cmd,
                "directory": self.root.display().to_string(),
                "signal": Value::Null,
                "duration_ms": 0,
                "status": "denied",
            })),
            lofi_types::PolicyAction::Ask => {
                // If a confirmation callback is available, ask the user.
                if let Some(confirm) = &self.confirm {
                    let approved = confirm(cmd.to_string()).await;
                    if approved {
                        return None;
                    }
                }
                Some(json!({
                    "ok": false,
                    "output": format!("requires confirmation: {}{}", decision.reason, suffix),
                    "code": Value::Null,
                    "command": cmd,
                    "directory": self.root.display().to_string(),
                    "signal": Value::Null,
                    "duration_ms": 0,
                    "status": "needs_confirmation",
                }))
            }
            lofi_types::PolicyAction::Allow => None,
        }
    }

    /// # Errors
    /// Returns [`Error::Io`] only if the process cannot be spawned.
    pub async fn bash(&self, args: Value) -> Result<Value> {
        let cmd = args
            .get("cmd")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("bash: missing 'cmd'".into()))?
            .to_owned();
        let timeout_ms = args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BASH_TIMEOUT_MS);
        let dur = Duration::from_millis(timeout_ms);

        // Evaluate the command against the shell policy before spawning.
        if let Some(blocked) = self.check_policy(&cmd).await {
            return Ok(blocked);
        }

        // `bash` is intentionally host-level: it runs the user's project
        // commands and is *not* a security sandbox. The child env is resolved
        // up front from the `bash` config: by default a minimal baseline
        // (`PATH`/`HOME`/locale) so inherited credentials never reach a
        // model-run shell, with `pass_env`/`env_file` opting specific vars
        // back in — whose values are redacted from the captured output below.
        // The child runs in its own process group (`process_group(0)`) so a
        // timeout or cancellation can kill the *entire* tree — background
        // children and grandchildren included — rather than just the `sh`
        // leader. The `PgrpKillGuard` makes that robust against early return
        // or future cancellation.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&cmd)
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        self.bash_env.apply(&mut command);

        let started = Instant::now();
        let mut child = command.spawn()?;
        // `process_group(0)` makes the child its own session/group leader,
        // so its pid is the process-group id. Killing `-pgid` reaches every
        // descendant the shell spawned.
        let mut guard = PgrpKillGuard::new(child.id());
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tool("bash: stdout pipe unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Tool("bash: stderr pipe unavailable".into()))?;

        let result = tokio::time::timeout(
            dur,
            Box::pin(async {
                // Read both pipes AND wait for the child inside the timeout,
                // so a command that closes its pipes early but keeps running
                // (e.g. `exec >/dev/null 2>&1; sleep 1000`) can't outlive
                // `timeoutMs` by deferring the wait past the timed future.
                // `MAX_BASH_OUTPUT_BYTES` is a memory safety cap only; the
                // user-visible truncation is applied below via `truncate_tail`.
                let (out, err, status) = tokio::try_join!(
                    read_capped(&mut stdout, MAX_BASH_OUTPUT_BYTES),
                    read_capped(&mut stderr, MAX_BASH_OUTPUT_BYTES),
                    child.wait(),
                )?;
                Ok::<_, std::io::Error>((out, err, status))
            }),
        )
        .await;

        match result {
            Ok(Ok((out, err, status))) => {
                // The child exited and was reaped inside the timed future;
                // disarm the guard so the completed group isn't signaled.
                guard.disarm();
                let (out_bytes, out_truncated) = out;
                let (err_bytes, err_truncated) = err;
                let mut merged = out_bytes;
                merged.extend_from_slice(&err_bytes);
                let pipe_capped = out_truncated || err_truncated;
                // Scrub approved secret values before the output is truncated,
                // logged to the tmp file, or returned to the model — so a
                // command may *use* a secret without its value landing in the
                // transcript or the bash log.
                let mut full = String::from_utf8_lossy(&merged).into_owned();
                self.bash_env.redact(&mut full);
                let output = self.format_bash_output(&full, pipe_capped);
                #[allow(clippy::cast_possible_truncation)]
                let duration_ms = started.elapsed().as_millis() as u64;
                Ok(json!({
                    "ok": status.success(),
                    "output": output,
                    "code": status.code(),
                    "command": cmd,
                    "directory": self.root.display().to_string(),
                    "signal": status.signal(),
                    "duration_ms": duration_ms,
                    "status": if status.signal().is_some() { "signaled" } else { "exited" },
                }))
            }
            Ok(Err(e)) => {
                // A read error may leave the child running; kill the whole
                // group and reap before surfacing the error so we don't
                // orphan the shell or its descendants.
                drop(guard);
                let _ = child.wait().await;
                Err(Error::Io(e))
            }
            Err(_) => {
                // Timeout: drop the guard to SIGKILL the whole process group, then
                // reap the leader so we don't leave a zombie.
                drop(guard);
                let _ = child.wait().await;
                Ok(json!({
                    "ok": false,
                    "output": "<timeout>",
                    "code": Value::Null,
                    "command": cmd,
                    "directory": self.root.display().to_string(),
                    "signal": Value::Null,
                    "duration_ms": timeout_ms,
                    "status": "timeout",
                }))
            }
        }
    }

    /// Tail-truncate `full` to 50 KB / 2000 lines and, when truncation occurs,
    /// write the full output to a temp file under the session tmp dir and append
    /// a notice pointing at it. `pipe_capped` indicates the pipe-level safety cap
    /// (8 MiB) was hit, in which case the temp file holds only what was captured.
    fn format_bash_output(&self, full: &str, pipe_capped: bool) -> String {
    let t = truncate_tail(full);
    if !t.truncated && !pipe_capped {
        return full.to_string();
    }
    let mut out = t.content;
    let start_line = t.total_lines.saturating_sub(t.output_lines) + 1;
    let end_line = t.total_lines;
    // Write the full captured output to the session tmp dir so the model can
    // page through it with `lofi.read` (the tmp dir is a read root).
    let path = match self.write_bash_log(full) {
        Ok(p) => p,
        Err(_) => "<temp file unavailable>".to_string(),
    };
    if t.output_lines == 0 {
        // Single line exceeded the byte budget.
        let _ = write!(
            out,
            "\n\n[Showing 0 lines; first line exceeds {} limit. Full output: {path}. Use lofi.read(\"{path}\") to page through.]",
            format_size(DEFAULT_MAX_BYTES),
        );
    } else if pipe_capped && !t.truncated {
        let _ = write!(
            out,
            "\n\n[Output exceeded {} safety cap; truncated. Full output: {path}. Use lofi.read(\"{path}\") to page through.]",
            format_size(MAX_BASH_OUTPUT_BYTES)
        );
    } else {
        let _ = write!(
            out,
            "\n\n[Showing lines {start_line}-{end_line} of {} ({} limit). Full output: {path}. Use lofi.read(\"{path}\") to page through.]",
            t.total_lines,
            format_size(DEFAULT_MAX_BYTES)
        );
    }
    out
}

    /// Write `content` to `lofi-bash-<hex>.log` under the session tmp dir and
    /// return the path.
    fn write_bash_log(&self, content: &str) -> std::io::Result<String> {
        use std::io::Write;
        let id = temp_id();
        let path = self.tmp_dir.join(format!("lofi-bash-{id}.log"));
        let mut f = std::fs::File::create(&path)?;
        f.write_all(content.as_bytes())?;
        f.flush()?;
        Ok(path.to_string_lossy().into_owned())
    }
}

/// 16-hex-char random id for temp file names, without pulling in another crate.
fn temp_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:016x}")
}