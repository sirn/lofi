use std::fmt::Write as _;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use lofi_code::user_shell::{run_user_shell, UserShellOutput};
use lofi_error::Result;

const CONTEXT_MAX_BYTES: usize = 16 * 1024;
const CONTEXT_MAX_LINES: usize = 40;

#[derive(Debug, Clone)]
pub struct UserShellResult {
    pub command: String,
    pub output: String,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub duration_ms: u64,
    pub truncated: bool,
    pub cancelled: bool,
}

impl UserShellResult {
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
            let _ = write!(text, "\nCommand exited with code {code}");
        } else if let Some(signal) = self.signal {
            let _ = write!(text, "\nCommand terminated by signal {signal}");
        }
        if self.truncated {
            text.push_str("\n[Output truncated]");
        }
        text
    }
}

/// Run a direct `!` shell command and adapt the process output into the
/// session result recorded by core.
///
/// # Errors
/// Returns an error if process execution fails.
pub async fn run_user_shell_command(
    root: &Path,
    command: String,
    cancel: Arc<AtomicBool>,
    deltas: Option<lofi_code::user_shell::DeltaSender>,
) -> Result<UserShellResult> {
    let UserShellOutput {
        output,
        exit_code,
        signal,
        duration_ms,
        truncated,
        cancelled,
    } = Box::pin(run_user_shell(root, &command, cancel, deltas)).await?;
    Ok(UserShellResult {
        command,
        output,
        exit_code,
        signal,
        duration_ms,
        truncated,
        cancelled,
    })
}
