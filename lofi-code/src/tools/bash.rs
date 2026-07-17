use super::truncate::{format_size, truncate_tail_with};

/// Bash results stay deliberately compact because the same value is shown to
/// the user and sent back to the model. The complete output remains pageable
/// through the per-session log named in the truncation notice.
const BASH_MAX_LINES: usize = 20;
const BASH_MAX_BYTES: usize = 4 * 1024;
const AUTO_MODE_UI_GRACE: Duration = Duration::from_secs(3);
use super::util::{read_capped, PgrpKillGuard};
#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

impl BuiltinTools {
    /// Output is tail-truncated to 4 KB / 20 lines (whichever is hit
    /// first), keeping the end where errors and final results land. When
    /// truncated, the full captured output is written to a temp file under
    /// the session tmp dir and its absolute path is included in the notice
    /// so the model can `lofi.read` it in pages (the tmp dir is a read root).
    /// Decision flow for `Ask`:
    /// 1. Auto-mode gets a three-second head start with no UI.
    /// 2. If it is still running, show a live confirmation dialog and race
    ///    the evaluator against the user's override.
    /// 3. An auto-mode `ask` or failure leaves the dialog open with its
    ///    reason. An `allow` dismisses it and proceeds.
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
                let approved = if let Some(auto_mode) = &self.auto_mode {
                    self.auto_mode_decision_after(cmd, auto_mode, AUTO_MODE_UI_GRACE)
                        .await
                } else {
                    self.confirm_decision(cmd, crate::ConfirmReason::Policy)
                        .await
                };
                if approved {
                    return None;
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

    async fn auto_mode_decision_after(
        &self,
        cmd: &str,
        auto_mode: &crate::AutoModeFn,
        ui_grace: Duration,
    ) -> bool {
        let started_at = Instant::now();
        let mut evaluation = auto_mode(cmd.to_string());
        match tokio::time::timeout(ui_grace, &mut evaluation).await {
            Ok(outcome) => return self.finish_auto_mode(cmd, outcome).await,
            Err(_) if self.confirm.is_none() => {
                return matches!(evaluation.await, crate::AutoModeOutcome::Allow { .. });
            }
            Err(_) => {}
        }

        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reason = std::sync::Arc::new(std::sync::Mutex::new(
            crate::ConfirmReason::AutoEvaluating { started_at },
        ));
        let prompt = crate::ConfirmPrompt {
            command: cmd.to_string(),
            reason: reason.clone(),
            active: active.clone(),
        };
        let Some(confirm) = &self.confirm else {
            return false;
        };
        let mut response = confirm(prompt);

        tokio::select! {
            outcome = &mut evaluation => {
                match outcome {
                    crate::AutoModeOutcome::Allow { .. } => {
                        active.store(false, std::sync::atomic::Ordering::Relaxed);
                        true
                    }
                    crate::AutoModeOutcome::Ask { reason: ask_reason } => {
                        *reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            crate::ConfirmReason::AutoAsk { reason: ask_reason };
                        let approved = response.await;
                        active.store(false, std::sync::atomic::Ordering::Relaxed);
                        approved
                    }
                    crate::AutoModeOutcome::Failed { reason: failure } => {
                        *reason.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                            crate::ConfirmReason::AutoFailed { reason: failure };
                        let approved = response.await;
                        active.store(false, std::sync::atomic::Ordering::Relaxed);
                        approved
                    }
                }
            }
            approved = &mut response => {
                active.store(false, std::sync::atomic::Ordering::Relaxed);
                approved
            }
        }
    }

    async fn finish_auto_mode(&self, cmd: &str, outcome: crate::AutoModeOutcome) -> bool {
        match outcome {
            crate::AutoModeOutcome::Allow { .. } => true,
            crate::AutoModeOutcome::Ask { reason } => {
                self.confirm_decision(cmd, crate::ConfirmReason::AutoAsk { reason })
                    .await
            }
            crate::AutoModeOutcome::Failed { reason } => {
                self.confirm_decision(cmd, crate::ConfirmReason::AutoFailed { reason })
                    .await
            }
        }
    }

    async fn confirm_decision(&self, cmd: &str, reason: crate::ConfirmReason) -> bool {
        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let Some(confirm) = &self.confirm else {
            return false;
        };
        let approved = confirm(crate::ConfirmPrompt {
            command: cmd.to_string(),
            reason: std::sync::Arc::new(std::sync::Mutex::new(reason)),
            active: active.clone(),
        })
        .await;
        active.store(false, std::sync::atomic::Ordering::Relaxed);
        approved
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
                guard.disarm();
                let (out_bytes, out_truncated) = out;
                let (err_bytes, err_truncated) = err;
                let mut merged = out_bytes;
                merged.extend_from_slice(&err_bytes);
                let pipe_capped = out_truncated || err_truncated;
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
                drop(guard);
                let _ = child.wait().await;
                Err(Error::Io(e))
            }
            Err(_) => {
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

    /// Tail-truncate `full` to 4 KB / 20 lines and, when truncation occurs,
    /// write the full output to a temp file under the session tmp dir and append
    /// a notice pointing at it. `pipe_capped` indicates the pipe-level safety cap
    /// (8 MiB) was hit, in which case the temp file holds only what was captured.
    fn format_bash_output(&self, full: &str, pipe_capped: bool) -> String {
        // A command's conventional final newline terminates its last line; it
        // is not an additional blank line and must not consume one tail slot.
        let truncation_input = full.strip_suffix('\n').unwrap_or(full);
        let t = truncate_tail_with(truncation_input, BASH_MAX_LINES, BASH_MAX_BYTES);
        if !t.truncated && !pipe_capped {
            return full.to_string();
        }
        let mut out = t.content;
        let start_line = t.total_lines.saturating_sub(t.output_lines) + 1;
        let end_line = t.total_lines;
        let path = match self.write_bash_log(full) {
            Ok(p) => p,
            Err(_) => "<temp file unavailable>".to_string(),
        };
        if t.output_lines == 0 {
            let _ = write!(
            out,
            "\n\n[Showing 0 lines; first line exceeds {} limit. Full output: {path}. Use lofi.read(\"{path}\") to page through.]",
            format_size(BASH_MAX_BYTES),
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
            format_size(BASH_MAX_BYTES)
        );
        }
        out
    }

    fn write_bash_log(&self, content: &str) -> std::io::Result<String> {
        use std::io::Write;
        let id = temp_id();
        let path = self.tmp_dir.join(format!("lofi-bash-{id}.log"));
        let mut f = std::fs::File::create(&path)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::File::open(&self.tmp_dir)?.sync_all()?;
        Ok(path.to_string_lossy().into_owned())
    }
}

fn temp_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    format!("{nanos:016x}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::future;
    use std::sync::{Arc, Mutex};

    fn tools_with_auto(
        auto_mode: crate::AutoModeFn,
        confirm: crate::ConfirmFn,
    ) -> (tempfile::TempDir, BuiltinTools) {
        let dir = tempfile::tempdir().unwrap();
        let policy = crate::policy::ResolvedPolicy {
            allow: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
            wrappers: std::collections::HashMap::new(),
            redirects: lofi_types::RedirectPolicy::default(),
            heredocs: lofi_types::HeredocPolicy::default(),
            yolo: false,
            allow_by_default: false,
        };
        let tools = BuiltinTools::with_skills_dir(
            dir.path().to_path_buf(),
            None,
            super::default_tmp_dir(),
            crate::BashEnv::default(),
            policy,
            Some(confirm),
            Some(auto_mode),
            None,
        );
        (dir, tools)
    }

    #[tokio::test]
    async fn fast_auto_approval_does_not_open_confirmation() {
        let confirmed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let confirm: crate::ConfirmFn = {
            let confirmed = confirmed.clone();
            Arc::new(move |_| {
                confirmed.store(true, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { false })
            })
        };
        let auto: crate::AutoModeFn = Arc::new(|_| {
            Box::pin(async {
                crate::AutoModeOutcome::Allow {
                    reason: "safe".to_string(),
                }
            })
        });
        let (_dir, tools) = tools_with_auto(auto.clone(), confirm);

        assert!(
            tools
                .auto_mode_decision_after("echo ok", &auto, Duration::from_secs(1))
                .await
        );
        assert!(!confirmed.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn slow_auto_evaluation_can_be_overridden() {
        let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let confirm: crate::ConfirmFn = {
            let seen = seen.clone();
            Arc::new(move |prompt| {
                let evaluating = matches!(
                    *prompt
                        .reason
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    crate::ConfirmReason::AutoEvaluating { .. }
                );
                seen.store(evaluating, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async { true })
            })
        };
        let auto: crate::AutoModeFn = Arc::new(|_| Box::pin(future::pending()));
        let (_dir, tools) = tools_with_auto(auto.clone(), confirm);

        assert!(
            tools
                .auto_mode_decision_after("echo ok", &auto, Duration::from_millis(1))
                .await
        );
        assert!(seen.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test]
    async fn auto_ask_reason_is_forwarded_to_confirmation() {
        let seen = Arc::new(Mutex::new(String::new()));
        let confirm: crate::ConfirmFn = {
            let seen = seen.clone();
            Arc::new(move |prompt| {
                let seen = seen.clone();
                Box::pin(async move {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let reason = prompt
                        .reason
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    if let crate::ConfirmReason::AutoAsk { reason } = reason {
                        *seen
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) = reason;
                    }
                    false
                })
            })
        };
        let auto: crate::AutoModeFn = Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(5)).await;
                crate::AutoModeOutcome::Ask {
                    reason: "writes outside the workspace".to_string(),
                }
            })
        });
        let (_dir, tools) = tools_with_auto(auto.clone(), confirm);

        assert!(
            !tools
                .auto_mode_decision_after("cp x /tmp/x", &auto, Duration::from_millis(1))
                .await
        );
        assert_eq!(
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "writes outside the workspace"
        );
    }
}
