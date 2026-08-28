use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lofi_error::{Error, Result};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::tools::{truncate_tail_with, PgrpKillGuard};

const CAPTURE_TAIL_BYTES: usize = 64 * 1024;

pub type OutputSender = tokio::sync::mpsc::Sender<String>;

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
/// When `output_tx` is set, every read emits an ANSI-stripped, UTF-8-safe
/// chunk. The returned [`UserShellOutput`] still carries the captured tail.
///
/// # Errors
/// Returns an error if the shell cannot be spawned, its pipes are unavailable,
/// or command output/status cannot be read.
pub async fn run_user_shell(
    root: &Path,
    command_text: &str,
    cancel: Arc<AtomicBool>,
    output_tx: Option<OutputSender>,
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
    let out_tail = OutputTail::default();
    let err_tail = OutputTail::default();
    let cancelled = async {
        while !cancel.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    };
    let completed = async {
        tokio::try_join!(
            read_stream(&mut stdout, &out_tail, output_tx.clone()),
            read_stream(&mut stderr, &err_tail, output_tx),
            child.wait(),
        )
    };
    tokio::select! {
        result = completed => {
            let ((), (), status) = result?;
            guard.disarm();
            Ok(finish(&out_tail, &err_tail, started, Some(status), false))
        }
        () = cancelled => {
            drop(guard);
            let _ = child.wait().await;
            Ok(finish(&out_tail, &err_tail, started, None, true))
        }
    }
}

fn finish(
    out_tail: &OutputTail,
    err_tail: &OutputTail,
    started: Instant,
    status: Option<std::process::ExitStatus>,
    cancelled: bool,
) -> UserShellOutput {
    let (out_bytes, out_truncated) = out_tail.take();
    let (err_bytes, err_truncated) = err_tail.take();
    let mut bytes = out_bytes;
    bytes.extend_from_slice(&err_bytes);
    let clean = strip_ansi(&String::from_utf8_lossy(&bytes));
    let captured = truncate_tail_with(clean.trim_end_matches('\n'), usize::MAX, CAPTURE_TAIL_BYTES);
    UserShellOutput {
        output: captured.content,
        exit_code: status.and_then(|status| status.code()),
        signal: status.and_then(|status| status.signal()),
        duration_ms: started.elapsed().as_millis() as u64,
        truncated: out_truncated || err_truncated || captured.truncated,
        cancelled,
    }
}

#[derive(Default, Clone)]
struct OutputTail {
    inner: Arc<Mutex<(VecDeque<u8>, bool)>>,
}

impl OutputTail {
    fn push(&self, bytes: &[u8]) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tail, truncated) = &mut *guard;
        for &byte in bytes {
            if tail.len() == CAPTURE_TAIL_BYTES {
                tail.pop_front();
                *truncated = true;
            }
            tail.push_back(byte);
        }
    }

    fn take(&self) -> (Vec<u8>, bool) {
        let mut guard = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tail, truncated) = &mut *guard;
        let bytes = tail.drain(..).collect();
        (bytes, *truncated)
    }
}

async fn read_stream<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    tail: &OutputTail,
    mut output_tx: Option<OutputSender>,
) -> std::io::Result<()> {
    let mut stripper = AnsiStreamStripper::default();
    let mut buf = [0u8; 8192];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        tail.push(&buf[..n]);
        if let Some(tx) = &output_tx {
            let chunk = stripper.feed(&buf[..n]);
            // A dropped receiver only means the consumer went away; keep
            // draining so the child cannot deadlock on a full pipe.
            if !chunk.is_empty() && tx.send(chunk).await.is_err() {
                output_tx = None;
            }
        }
    }
    if let Some(tx) = output_tx {
        let chunk = stripper.flush();
        if !chunk.is_empty() {
            let _ = tx.send(chunk).await;
        }
    }
    Ok(())
}

#[derive(Default)]
struct AnsiStreamStripper {
    state: AnsiState,
    utf8: Vec<u8>,
}

impl AnsiStreamStripper {
    fn feed(&mut self, bytes: &[u8]) -> String {
        let mut plain = std::mem::take(&mut self.utf8);
        plain.reserve(bytes.len());
        for &byte in bytes {
            match self.state {
                AnsiState::Ground => {
                    if byte == 0x1b {
                        self.state = AnsiState::Escape;
                    } else {
                        plain.push(byte);
                    }
                }
                AnsiState::Escape => {
                    self.state = match byte {
                        b'[' => AnsiState::Csi,
                        b']' => AnsiState::Osc,
                        0x1b => AnsiState::Escape,
                        _ => AnsiState::Ground,
                    };
                }
                AnsiState::Csi => {
                    if (0x40..=0x7e).contains(&byte) {
                        self.state = AnsiState::Ground;
                    } else if byte == 0x1b {
                        self.state = AnsiState::Escape;
                    }
                }
                AnsiState::Osc => match byte {
                    0x07 => self.state = AnsiState::Ground,
                    0x1b => self.state = AnsiState::OscEscape,
                    _ => {}
                },
                AnsiState::OscEscape => {
                    self.state = match byte {
                        b'\\' => AnsiState::Ground,
                        0x1b => AnsiState::OscEscape,
                        _ => AnsiState::Osc,
                    };
                }
            }
        }
        decode_utf8(&plain, &mut self.utf8)
    }

    fn flush(self) -> String {
        String::from_utf8_lossy(&self.utf8).into_owned()
    }
}

#[derive(Default)]
enum AnsiState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

fn decode_utf8(bytes: &[u8], pending: &mut Vec<u8>) -> String {
    let mut text = String::new();
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(valid) => {
                text.push_str(valid);
                return text;
            }
            Err(error) => {
                let valid_end = offset + error.valid_up_to();
                text.push_str(std::str::from_utf8(&bytes[offset..valid_end]).unwrap_or_default());
                offset = valid_end;
                let Some(error_len) = error.error_len() else {
                    pending.extend_from_slice(&bytes[offset..]);
                    return text;
                };
                text.push(char::REPLACEMENT_CHARACTER);
                offset += error_len;
            }
        }
    }
    text
}

fn strip_ansi(input: &str) -> String {
    let mut stripper = AnsiStreamStripper::default();
    let mut output = stripper.feed(input.as_bytes());
    output.push_str(&stripper.flush());
    output
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
            None,
        ))
        .await?;
        assert_eq!(result.exit_code, Some(0));
        assert!(result.output.starts_with("ok"));
        assert!(result
            .output
            .contains(&dir.path().to_string_lossy().into_owned()));
        Ok(())
    }

    #[tokio::test]
    async fn streams_output_as_deltas() -> Result<()> {
        let dir = tempfile::tempdir().map_err(Error::Io)?;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let result = Box::pin(run_user_shell(
            dir.path(),
            "sleep 0.2; printf 'first'; sleep 0.2; printf 'second'",
            Arc::new(AtomicBool::new(false)),
            Some(tx),
        ))
        .await?;
        drop(result);
        let mut chunks = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            chunks.push(chunk);
        }
        assert_eq!(chunks.concat(), "firstsecond");
        assert!(chunks.len() >= 2, "one chunk per .2s-separated write");
        Ok(())
    }

    #[tokio::test]
    async fn cancel_keeps_the_output_captured_so_far() -> Result<()> {
        let dir = tempfile::tempdir().map_err(Error::Io)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_run = Arc::clone(&cancel);
        let run = tokio::spawn(async move {
            Box::pin(run_user_shell(
                dir.path(),
                "printf 'kept-after-cancel'; sleep 60",
                cancel_for_run,
                None,
            ))
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.store(true, Ordering::Relaxed);
        let result = run.await.map_err(|e| Error::State(e.to_string()))??;
        assert!(result.cancelled);
        assert_eq!(result.output, "kept-after-cancel");
        Ok(())
    }

    #[test]
    fn stripper_handles_split_escapes_and_split_utf8() {
        let mut stripper = AnsiStreamStripper::default();
        assert_eq!(stripper.feed(b"a\x1b[3"), "a");
        assert_eq!(stripper.feed(b"1mred\x1b[0m "), "red ");
        let check = "✓".as_bytes();
        assert_eq!(stripper.feed(&check[..1]), "");
        assert_eq!(stripper.feed(&check[1..]), "✓");
        assert_eq!(stripper.flush().as_str(), "");
    }

    #[test]
    fn stripper_does_not_buffer_unterminated_escape_content() {
        let mut stripper = AnsiStreamStripper::default();
        let mut input = b"]".to_vec();
        input.resize(1024 * 1024, b'x');
        assert_eq!(stripper.feed(&input), "");
        assert!(stripper.utf8.is_empty());
    }

    #[test]
    fn stripper_consensus_with_strip_ansi() {
        for input in [
            "\x1b[31mred\x1b[0m plain",
            "osc \x1b]8;;http://x\x07link\x1b]8;;\x07 end",
            "\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\",
            "no escapes",
            "trailing \x1b[31",
        ] {
            let mut stripper = AnsiStreamStripper::default();
            let mut streamed = stripper.feed(input.as_bytes());
            streamed.push_str(&stripper.flush());
            assert_eq!(streamed, strip_ansi(input), "input {input:?}");
        }
    }

    #[test]
    fn strips_terminal_control_sequences() {
        assert_eq!(strip_ansi("a\x1b[31mred\x1b[0m b"), "ared b");
    }
}
