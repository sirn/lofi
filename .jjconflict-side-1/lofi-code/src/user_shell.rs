use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use lofi_error::{Error, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::tools::{truncate_tail_with, PgrpKillGuard};

const CAPTURE_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct UserShellOutput {
    pub output: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub cancelled: bool,
}

/// Run a user-entered shell command outside the model tool sandbox.
///
/// The user-shell path intentionally uses the interactive environment. The
/// model-run `bash` tool resolves a separate policy-controlled environment.
///
/// # Errors
/// Returns an error if the shell cannot be spawned, its pipes are unavailable,
/// or command output/status cannot be read.
pub async fn run_user_shell(
    root: &Path,
    command_text: &str,
    cancel: Arc<AtomicBool>,
) -> Result<UserShellOutput> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(command_text)
        .current_dir(root)
        // The TUI reader thread owns the terminal stdin. Inheriting it here
        // lets the shell win the read race and swallow typed keys; a null
        // stdin gives the shell EOF instead.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let started = Instant::now();
    let mut child = command.spawn()?;
    let mut guard = PgrpKillGuard::new(child.id());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::State("user shell: stdout pipe unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::State("user shell: stderr pipe unavailable".into()))?;
    let cancelled = async {
        while !cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let completed = async {
        tokio::try_join!(
            read_tail(&mut stdout, CAPTURE_TAIL_BYTES),
            read_tail(&mut stderr, CAPTURE_TAIL_BYTES),
            child.wait(),
        )
    };
    let (out, err, status) = tokio::select! {
        result = completed => result?,
        () = cancelled => {
            drop(guard);
            let _ = child.wait().await;
            return Ok(UserShellOutput {
                output: String::new(),
                exit_code: None,
                signal: None,
                duration_ms: started.elapsed().as_millis() as u64,
                truncated: false,
                cancelled: true,
            });
        }
    };
    guard.disarm();

    let mut bytes = out.0;
    bytes.extend_from_slice(&err.0);
    let clean = strip_ansi(&String::from_utf8_lossy(&bytes));
    let captured = truncate_tail_with(clean.trim_end_matches('\n'), usize::MAX, CAPTURE_TAIL_BYTES);
    Ok(UserShellOutput {
        output: captured.content,
        exit_code: status.code(),
        signal: status.signal(),
        duration_ms: started.elapsed().as_millis() as u64,
        truncated: out.1 || err.1 || captured.truncated,
        cancelled: false,
    })
}

async fn read_tail<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut tail = VecDeque::with_capacity(cap);
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        for &byte in &buf[..n] {
            if tail.len() == cap {
                tail.pop_front();
                truncated = true;
            }
            tail.push_back(byte);
        }
    }
    Ok((tail.into(), truncated))
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= bytes.len() {
            break;
        }
        match bytes[i] {
            b'[' => {
                i += 1;
                while i < bytes.len() {
                    let byte = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                }
            }
            b']' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runs_in_root_and_captures_output() -> Result<()> {
        let dir = tempfile::tempdir().map_err(Error::Io)?;
        let result = Box::pin(run_user_shell(
            dir.path(),
            "printf 'ok'; pwd",
            Arc::new(AtomicBool::new(false)),
        ))
        .await?;
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.starts_with("ok"));
        assert!(result
            .output
            .contains(&dir.path().to_string_lossy().into_owned()));
        Ok(())
    }

    #[test]
    fn strips_terminal_control_sequences() {
        assert_eq!(strip_ansi("a\x1b[31mred\x1b[0m b"), "ared b");
    }
}
