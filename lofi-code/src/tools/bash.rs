use super::bash_util::{read_capped, PgrpKillGuard};
use super::truncate::{format_size, truncate_tail_with};
#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Outcome the race produces: either both pipes drained and `wait` returned,
/// or the join itself failed (pipe error or spawn-side I/O).
type RaceInner = std::result::Result<
    ((Vec<u8>, bool), (Vec<u8>, bool), std::process::ExitStatus),
    std::io::Error,
>;
/// Outer race result: `Err` when the timeout or cancel fired first, `Ok`
/// wrapping the inner join outcome otherwise.
type RaceOutcome = std::result::Result<RaceInner, tokio::time::error::Elapsed>;

impl BuiltinTools {
    /// Output is tail-truncated to 4 KB / 20 lines (whichever is hit
    /// first), keeping the end where errors and final results land. When
    /// truncated, the full captured output is written to a temp file under
    /// the session tmp dir and its absolute path is included in the notice
    /// so the model can `lofi.read` it in pages (the tmp dir is a read root).
    /// Auto-mode emits an evaluating confirmation request immediately and
    /// races its decision against a manual response. Presentation and timing
    /// policy belong to consumers of that event.
    pub(super) async fn check_policy(&self, cmd: &str) -> Option<Value> {
        // Allow-all auto-passes allow and ask decisions but explicit deny
        // rules still block; deny-all is absolute and overrides even the
        // allow list.
        let mode = self.policy_override.effective(self.auto_mode.is_some());
        if mode == lofi_types::BashApprovalMode::DenyAll {
            return Some(json!({
                "ok": false,
                "output": "blocked by session policy: deny all commands".to_string(),
                "code": Value::Null,
                "command": cmd,
                "directory": self.root.display().to_string(),
                "signal": Value::Null,
                "duration_ms": 0,
                "status": "denied",
            }));
        }
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
                // Allow-all auto-passes ask decisions.
                if mode == lofi_types::BashApprovalMode::AllowAll {
                    return None;
                }
                // An explicit `ask (manual)` pick bypasses an available auto
                // mode; `ask (auto)` falls back to the manual prompt when no
                // auto mode is configured.
                let use_auto =
                    mode == lofi_types::BashApprovalMode::AskAuto && self.auto_mode.is_some();
                let approved = match (use_auto, &self.auto_mode) {
                    (true, Some(auto_mode)) => self.auto_mode_decision(cmd, auto_mode).await,
                    _ => {
                        self.confirm_decision(cmd, crate::ConfirmReason::Policy)
                            .await
                    }
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

    async fn auto_mode_decision(&self, cmd: &str, auto_mode: &crate::AutoModeFn) -> bool {
        let mut evaluation = auto_mode(cmd.to_string());
        let Some(confirm) = &self.confirm else {
            return matches!(evaluation.await, crate::AutoModeOutcome::Allow { .. });
        };

        let active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let reason = std::sync::Arc::new(std::sync::Mutex::new(
            crate::ConfirmReason::AutoEvaluating {
                started_at: Instant::now(),
            },
        ));
        let mut response = confirm(crate::ConfirmPrompt {
            command: cmd.to_string(),
            reason: reason.clone(),
            active: active.clone(),
        });

        tokio::select! {
            biased;
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

    /// Build the host-level `sh -c` invocation: stdin null (the TUI reader
    /// thread owns the terminal stdin; inheriting it lets the tool win the
    /// read race and swallow typed keys), stdout/stderr piped, own process
    /// group so the whole tree can be killed on timeout/cancel, env resolved
    /// from the `bash` config (minimal baseline by default; specific vars
    /// opted back in via `pass_env`/`env_file`).
    fn command_for(&self, cmd: &str) -> Command {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        self.bash_env.apply(&mut command);
        command
    }

    /// Map the race outcome into the JSON shape returned to the guest. The
    /// three arms correspond to (a) child exited and both pipes drained, (b)
    /// the join failed (pipe error or spawn-time I/O), and (c) the
    /// outer timeout/cancel fired before either finished.
    fn bash_result_json(
        &self,
        cmd: &str,
        timeout_ms: u64,
        started: Instant,
        mut guard: PgrpKillGuard,
        result: RaceOutcome,
    ) -> Result<Value> {
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
                Err(Error::Io(e))
            }
            Err(_) => {
                drop(guard);
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
        let mut command = self.command_for(&cmd);

        let started = Instant::now();
        let mut child = command.spawn()?;
        let guard = PgrpKillGuard::new(child.id());
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tool("bash: stdout pipe unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Tool("bash: stderr pipe unavailable".into()))?;

        // Run reads and the child wait inside the timeout so a command that
        // closes its pipes early but keeps running (`exec >/dev/null 2>&1;
        // sleep 1000`) can't outlive `timeoutMs`. `MAX_BASH_OUTPUT_BYTES` is
        // the pipe-level memory safety cap; user-visible truncation is below.
        let timed = Box::pin(tokio::time::timeout(dur, async move {
            let (out, err, status) = tokio::try_join!(
                read_capped(&mut stdout, MAX_BASH_OUTPUT_BYTES),
                read_capped(&mut stderr, MAX_BASH_OUTPUT_BYTES),
                child.wait(),
            )?;
            Ok::<_, std::io::Error>((out, err, status))
        }));

        // Race the run against user cancellation: while the guest awaits the
        // process, no QuickJS bytecode ticks, so the sandbox interrupt handler
        // can't observe `cancel`. Map cancel onto the same elapsed-timeout
        // outcome — that tears down identically (kill the process group, which
        // EOFs the pipes) and reports a non-ok result.
        let result = match &self.cancel {
            Some(flag) => {
                let cancel_wait = crate::tools::wait_for_cancel(flag);
                tokio::pin!(cancel_wait);
                tokio::select! {
                    biased;
                    r = timed => r,
                    () = &mut cancel_wait => {
                        // `timeout(dur, pending())` always elapses; borrow the
                        // error shape rather than name its type.
                        match tokio::time::timeout(
                            Duration::ZERO,
                            std::future::pending::<RaceInner>(),
                        )
                        .await
                        {
                            Err(elapsed) => Err(elapsed),
                            Ok(_) => unreachable!(),
                        }
                    }
                }
            }
            None => timed.await,
        };

        self.bash_result_json(&cmd, timeout_ms, started, guard, result)
    }

    /// Tail-truncate `full` to 4 KB / 20 lines and, when truncation occurs,
    /// write the full output to a temp file under the session tmp dir and append
    /// a notice pointing at it. `pipe_capped` indicates the pipe-level safety cap
    /// (8 MiB) was hit, in which case the temp file holds only what was captured.
    fn format_bash_output(&self, full: &str, pipe_capped: bool) -> String {
        // A command's conventional final newline terminates its last line; it
        // is not an additional blank line and must not consume one tail slot.
        let truncation_input = full.strip_suffix('\n').unwrap_or(full);
        let cap = self.truncate;
        let t = truncate_tail_with(truncation_input, cap.max_lines, cap.max_bytes);
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
            format_size(cap.max_bytes),
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
            format_size(cap.max_bytes)
        );
        }
        out
    }

    fn write_bash_log(&self, content: &str) -> std::io::Result<String> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt as _;

        let id = temp_id();
        let path = self.tmp_dir.join(format!("lofi-bash-{id}.log"));
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
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
    use std::sync::atomic::AtomicBool;
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
        let tmp = dir.path().join("lofi-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let tools = BuiltinTools::with_skills_dir(
            dir.path().to_path_buf(),
            None,
            tmp,
            crate::BashEnv::default(),
            policy,
            Some(confirm),
            Some(auto_mode),
            None,
        );
        (dir, tools)
    }

    #[tokio::test]
    async fn fast_auto_approval_retires_confirmation_event() {
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

        assert!(tools.auto_mode_decision("echo ok", &auto).await);
        assert!(confirmed.load(std::sync::atomic::Ordering::Relaxed));
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

        assert!(tools.auto_mode_decision("echo ok", &auto).await);
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

        assert!(!tools.auto_mode_decision("cp x /tmp/x", &auto).await);
        assert_eq!(
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            "writes outside the workspace"
        );
    }

    fn tools_with_cancel(cancel: Arc<AtomicBool>) -> (tempfile::TempDir, BuiltinTools) {
        let dir = tempfile::tempdir().unwrap();
        let auto: crate::AutoModeFn =
            Arc::new(|_| Box::pin(async { crate::AutoModeOutcome::Allow { reason: "t".into() } }));
        let tmp = dir.path().join("lofi-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let tools = BuiltinTools::with_skills_dir(
            dir.path().to_path_buf(),
            None,
            tmp,
            crate::BashEnv::default(),
            crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
            None,
            Some(auto),
            None,
        )
        .with_cancel(Some(cancel));
        (dir, tools)
    }

    #[tokio::test]
    async fn cancel_terminates_a_long_running_command_promptly() {
        let marker_dir = tempfile::tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel.clone());
        let marker = marker_dir.path().to_path_buf();
        let bash = tokio::spawn(async move {
            tools
                .bash(json!({
                    "cmd": format!("sleep 30; touch {}/ran", marker.display()),
                    "timeoutMs": 60_000
                }))
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.store(true, Ordering::Relaxed);
        let settled = tokio::time::timeout(Duration::from_secs(5), bash).await;
        assert!(settled.is_ok(), "bash must settle promptly on cancel");
        let res = settled.unwrap().unwrap();
        assert_eq!(res["ok"], json!(false), "got: {res}");
        assert!(!marker_dir.path().join("ran").exists());
    }

    #[tokio::test]
    async fn bash_completes_normally_without_cancel() {
        let cancel = Arc::new(AtomicBool::new(false));
        let (_dir, tools) = tools_with_cancel(cancel);
        let res = tools.bash(json!({ "cmd": "echo hi" })).await.unwrap();
        assert_eq!(res["ok"], json!(true), "got: {res}");
        assert_eq!(res["status"], json!("exited"));
    }
    fn tools_for_policy(
        config: &lofi_types::ShellPolicyConfig,
        confirm: Option<crate::ConfirmFn>,
        auto_mode: Option<crate::AutoModeFn>,
    ) -> (tempfile::TempDir, BuiltinTools) {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("lofi-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let tools = BuiltinTools::with_skills_dir(
            dir.path().to_path_buf(),
            None,
            tmp,
            crate::BashEnv::default(),
            crate::policy::defaults::resolve(config),
            confirm,
            auto_mode,
            None,
        );
        (dir, tools)
    }

    fn mode_override(mode: lofi_types::BashApprovalMode) -> crate::policy::PolicyOverride {
        let override_handle = crate::policy::PolicyOverride::default();
        override_handle.set(Some(mode));
        override_handle
    }

    fn command_entry(cmd: &str, mode: lofi_types::MatchMode) -> lofi_types::CommandEntry {
        lofi_types::CommandEntry {
            match_str: cmd.to_string(),
            mode,
        }
    }

    #[tokio::test]
    async fn override_allow_all_auto_passes_ask_but_respects_deny() {
        let config = lofi_types::ShellPolicyConfig {
            deny: vec![command_entry("curl", lofi_types::MatchMode::Prefix)],
            ..Default::default()
        };
        let (_dir, tools) = tools_for_policy(&config, None, None);
        let tools =
            tools.with_policy_override(mode_override(lofi_types::BashApprovalMode::AllowAll));
        // Unmatched commands fail closed to ask; allow-all passes them
        // without prompting.
        assert!(tools.check_policy("echo clean").await.is_none());
        // An explicit deny rule still blocks.
        let res = tools
            .check_policy("curl https://example.com")
            .await
            .unwrap();
        assert_eq!(res["status"], json!("denied"));
    }

    #[tokio::test]
    async fn override_deny_all_blocks_an_allowed_command() {
        let config = lofi_types::ShellPolicyConfig {
            allow: vec![command_entry("echo", lofi_types::MatchMode::Prefix)],
            ..Default::default()
        };
        let (_dir, tools) = tools_for_policy(&config, None, None);
        let tools =
            tools.with_policy_override(mode_override(lofi_types::BashApprovalMode::DenyAll));
        let res = tools.check_policy("echo hi").await.unwrap();
        assert_eq!(res["status"], json!("denied"));
    }

    #[tokio::test]
    async fn override_ask_manual_skips_an_available_auto_mode() {
        // Everything unmatched fails closed to ask under the default policy.
        let auto_called = Arc::new(AtomicBool::new(false));
        let auto: crate::AutoModeFn = {
            let auto_called = auto_called.clone();
            Arc::new(move |_| {
                auto_called.store(true, Ordering::Relaxed);
                Box::pin(async {
                    crate::AutoModeOutcome::Allow {
                        reason: "safe".to_string(),
                    }
                })
            })
        };
        let confirm: crate::ConfirmFn = Arc::new(|_| Box::pin(async { true }));
        let (_dir, tools) = tools_for_policy(
            &lofi_types::ShellPolicyConfig::default(),
            Some(confirm),
            Some(auto),
        );
        let tools =
            tools.with_policy_override(mode_override(lofi_types::BashApprovalMode::AskManual));
        assert!(tools.check_policy("special-cmd").await.is_none());
        assert!(!auto_called.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn override_ask_auto_without_auto_mode_falls_back_to_the_prompt() {
        let confirm: crate::ConfirmFn = Arc::new(|_| Box::pin(async { true }));
        let (_dir, tools) = tools_for_policy(
            &lofi_types::ShellPolicyConfig::default(),
            Some(confirm),
            None,
        );
        let tools =
            tools.with_policy_override(mode_override(lofi_types::BashApprovalMode::AskAuto));
        assert!(tools.check_policy("special-cmd").await.is_none());
    }

    #[tokio::test]
    async fn override_ask_auto_uses_auto_mode_when_configured() {
        let auto: crate::AutoModeFn = Arc::new(|_| {
            Box::pin(async {
                crate::AutoModeOutcome::Allow {
                    reason: "safe".to_string(),
                }
            })
        });
        let confirm: crate::ConfirmFn = Arc::new(|_| Box::pin(async { false }));
        let (_dir, tools) = tools_for_policy(
            &lofi_types::ShellPolicyConfig::default(),
            Some(confirm),
            Some(auto),
        );
        let tools =
            tools.with_policy_override(mode_override(lofi_types::BashApprovalMode::AskAuto));
        // Auto allowed it; the manual prompt answer (false) must not win.
        assert!(tools.check_policy("special-cmd").await.is_none());
    }
}
