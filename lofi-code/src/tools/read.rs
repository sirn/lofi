#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Read a file under the root as a UTF-8 string.
    ///
    /// `offset` is a 1-indexed line to start from; `limit` caps the number of
    /// lines returned. Output is head-truncated to 50 KB / 2000 lines
    /// (whichever is hit first), with a continuation hint pointing the model
    /// at the next offset — mirroring Pi's `read` tool so a large file can be
    /// paged through without dumping megabytes into the context.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the root, the file cannot
    /// be read, or `offset` is beyond the end of the file.
    pub async fn read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<Value> {
        reject_symlink_leaf(&self.root, path, &format!("read {path}"))?;
        let resolved = resolve_under(&self.root, path)?;
        reject_non_regular(&format!("read {path}"), &resolved)?;
        let label = path.to_string();
        let offset = offset.unwrap_or(1).max(1);
        let text = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use std::io::Read as _;
            // Stream at most MAX+1 bytes so truncation is detectable without
            // reading an entire huge file into memory.
            let file = std::fs::File::open(&resolved)?;
            let mut buf = Vec::new();
            file.take(MAX_READ_BYTES as u64 + 1).read_to_end(&mut buf)?;
            let truncated = buf.len() > MAX_READ_BYTES;
            let slice = if truncated {
                &buf[..MAX_READ_BYTES]
            } else {
                &buf[..]
            };
            Ok(String::from_utf8_lossy(slice).into_owned())
        })
        .await
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))?;

        let all_lines: Vec<&str> = text.split('\n').collect();
        let total_file_lines = all_lines.len();
        // Convert 1-indexed offset to 0-indexed array access.
        let start = (offset as usize).saturating_sub(1);
        if start >= all_lines.len() {
            return Err(Error::Tool(format!(
                "read {label}: offset {offset} is beyond end of file ({total_file_lines} lines total)"
            )));
        }
        // Honor a user-specified limit first; otherwise the head truncation
        // decides how many lines to keep.
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