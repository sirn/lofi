//! Direct, user-invoked shell execution for interactive `!command` input.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use lofi_code::tools::PgrpKillGuard;
use lofi_error::{Error, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const CAPTURE_TAIL_BYTES: usize = 64 * 1024;
const CONTEXT_MAX_BYTES: usize = 16 * 1024;
const CONTEXT_MAX_LINES: usize = 40;

/// Completed direct shell command, suitable for display and model context.
#[derive(Debug, Clone)]
pub struct UserBashResult {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub cancelled: bool,
}

impl UserBashResult {
    #[must_use]
    pub fn from_session(
        command: String,
        output: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        duration_ms: u64,
        truncated: bool,
        cancelled: bool,
    ) -> Self {
        Self {
            command,
            output,
            exit_code,
            signal,
            duration_ms,
            truncated,
            cancelled,
        }
    }

    /// Pi-compatible textual representation injected into subsequent model context.
    #[must_use]
    pub fn context_text(&self) -> String {
        let mut text = format!("Ran `{}`\n", self.command);
        if self.output.is_empty() {
            text.push_str("(no output)");
        } else {
            let compact = lofi_code::tools::truncate_tail_with(
                self.output.trim_end_matches('\n'),
                CONTEXT_MAX_LINES,
                CONTEXT_MAX_BYTES,
            );
            text.push_str("```\n");
            text.push_str(&compact.content);
            text.push_str("\n```");
            if compact.truncated {
                text.push_str("\n[Output truncated for model context]");
            }
        }
        if self.cancelled {
            text.push_str("\n(command cancelled)");
        } else if let Some(code) = self.exit_code.filter(|code| *code != 0) {
            text.push_str(&format!("\nCommand exited with code {code}"));
        } else if let Some(signal) = self.signal {
            text.push_str(&format!("\nCommand terminated by signal {signal}"));
        }
        if self.truncated {
            text.push_str("\n[Output truncated]");
        }
        text
    }
}

/// Run a command via `sh -c` in `root`, with no wall-clock timeout.
/// Dropping/aborting the future kills the command's entire process group.
pub async fn run_user_bash(root: &Path, command_text: String) -> Result<UserBashResult> {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(&command_text)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let started = Instant::now();
    let mut child = command.spawn()?;
    let mut guard = PgrpKillGuard::new(child.id());
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| Error::State("bash: stdout pipe unavailable".into()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| Error::State("bash: stderr pipe unavailable".into()))?;
    let (out, err, status) = tokio::try_join!(
        read_tail(&mut stdout, CAPTURE_TAIL_BYTES),
        read_tail(&mut stderr, CAPTURE_TAIL_BYTES),
        child.wait(),
    )?;
    guard.disarm();

    let mut bytes = out.0;
    bytes.extend_from_slice(&err.0);
    let clean = strip_ansi(&String::from_utf8_lossy(&bytes));
    let captured = lofi_code::tools::truncate_tail_with(
        clean.trim_end_matches('\n'),
        usize::MAX,
        CAPTURE_TAIL_BYTES,
    );
    Ok(UserBashResult {
        command: command_text,
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
    async fn runs_in_root_and_formats_context() {
        let dir = tempfile::tempdir().unwrap();
        let result = run_user_bash(dir.path(), "printf 'ok'; pwd".into())
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.starts_with("ok"));
        assert!(result.output.contains(dir.path().to_str().unwrap()));
        assert!(result.context_text().starts_with("Ran `printf 'ok'; pwd`"));
    }

    #[test]
    fn strips_terminal_control_sequences() {
        assert_eq!(strip_ansi("a\x1b[31mred\x1b[0m b"), "ared b");
    }
}
