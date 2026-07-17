use super::util::{read_capped, PgrpKillGuard};
#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

impl BuiltinTools {
    /// Run `cmd` via `sh -c` with cwd pinned to the root.
    ///
    /// stdout and stderr are merged. `timeoutMs` bounds the run (default
    /// 120s); on timeout the child is killed and `{ ok: false, output:
    /// "<timeout>", code: null }` is returned.
    ///
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

        // `bash` is intentionally host-level (mirrors Pi's `lofi.bash`): it
        // runs the user's project commands and is *not* a security sandbox.
        // The two real risks a model-controlled shell poses here — leaking
        // inherited credentials and leaving orphans on timeout — are handled
        // below: secret-like env vars are scrubbed from the child, and the
        // child runs in its own process group (`process_group(0)`) so a
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
        for (k, _) in std::env::vars() {
            if looks_secret(&k) {
                command.env_remove(&k);
            }
        }

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
                let mut output = String::from_utf8_lossy(&merged).into_owned();
                if out_truncated || err_truncated {
                    output.push_str("\n<output truncated>");
                }
                Ok(json!({
                    "ok": status.success(),
                    "output": output,
                    "code": status.code(),
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
                }))
            }
        }
    }
}
