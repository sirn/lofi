#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Read a file under the per-session tmp dir as a UTF-8 string.
    ///
    /// This is the paging companion to `lofi.bash`'s full-output logs: when
    /// bash output is tail-truncated, the full text is written to the session
    /// tmp dir and `lofi.read_tmp` is how the model retrieves the rest. It
    /// has the same `offset`/`limit` semantics and head-truncation as
    /// `lofi.read`, just rooted at the tmp dir instead of the workspace.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the tmp dir, the file
    /// cannot be read, or `offset` is beyond the end of the file.
    pub async fn read_tmp(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<Value> {
        let resolved = resolve_under(&self.tmp_dir, path)?;
        reject_non_regular(&format!("read_tmp {path}"), &resolved)?;
        let label = path.to_string();
        let offset = offset.unwrap_or(1).max(1);
        let text = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use std::io::Read as _;
            let file = std::fs::File::open(&resolved)?;
            let mut buf = Vec::new();
            file.take(MAX_READ_BYTES as u64 + 1).read_to_end(&mut buf)?;
            let truncated = buf.len() > MAX_READ_BYTES;
            let slice = if truncated { &buf[..MAX_READ_BYTES] } else { &buf[..] };
            Ok(String::from_utf8_lossy(slice).into_owned())
        })
        .await
        .map_err(|e| Error::Tool(format!("read_tmp {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("read_tmp {label}: {e}")))?;

        let all_lines: Vec<&str> = text.split('\n').collect();
        let total_file_lines = all_lines.len();
        let start = (offset as usize).saturating_sub(1);
        if start >= all_lines.len() {
            return Err(Error::Tool(format!(
                "read_tmp {label}: offset {offset} is beyond end of file ({total_file_lines} lines total)"
            )));
        }
        let selected = if let Some(lim) = limit {
            let end = (start + lim as usize).min(all_lines.len());
            all_lines[start..end].join("\n")
        } else {
            all_lines[start..].join("\n")
        };
        let start_display = start + 1;
        let t = truncate_head(&selected);
        let mut output = t.content;
        if t.truncated {
            let end_display = start_display + t.output_lines - 1;
            let next = end_display + 1;
            output.push_str(&format!(
                "\n\n[Showing lines {start_display}-{end_display} of {total_file_lines}. Use offset={next} to continue.]"
            ));
        } else if limit.is_some() {
            let used = start + t.output_lines;
            if used < all_lines.len() {
                let remaining = all_lines.len() - used;
                let next = used + 1;
                output.push_str(&format!(
                    "\n\n[{remaining} more lines in file. Use offset={next} to continue.]"
                ));
            }
        }
        Ok(json!(output))
    }
}