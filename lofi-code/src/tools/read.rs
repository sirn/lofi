#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the root, exceeds the file
    /// size limit, cannot be read, or `offset` is beyond the end of the file.
    pub async fn read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<Value> {
        // Symlinks are allowed: resolve_for_read canonicalizes the path
        // (following symlinks) and checks the result is under an allowed
        // root — that is the security boundary, not symlink rejection.
        let resolved = self.resolve_for_read(path)?;
        reject_non_regular(&format!("read {path}"), &resolved)?;
        let label = path.to_string();
        let label_inner = label.clone();
        let offset = offset.unwrap_or(1).max(1);
        let text = tokio::task::spawn_blocking(move || -> Result<String> {
            let meta = std::fs::metadata(&resolved)?;
            if meta.len() > MAX_READ_BYTES as u64 {
                return Err(Error::Tool(format!(
                    "read {label_inner}: file is {} (exceeds {} limit); use bash to inspect with head/sed/grep",
                    format_size(meta.len() as usize),
                    format_size(MAX_READ_BYTES)
                )));
            }
            let bytes = std::fs::read(&resolved)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))??;

        let all_lines: Vec<&str> = text.split('\n').collect();
        let total_file_lines = all_lines.len();
        let start = (offset as usize).saturating_sub(1);
        if start >= all_lines.len() {
            return Err(Error::Tool(format!(
                "read {label}: offset {offset} is beyond end of file ({total_file_lines} lines total)"
            )));
        }
        let selected = if let Some(lim) = limit {
            let end = (start + lim as usize).min(all_lines.len());
            all_lines[start..end].join("\n")
        } else {
            all_lines[start..].join("\n")
        };
        let start_line = start + 1;
        let cap = self.truncate;
        let t = truncate_head_with(&selected, cap.max_lines, cap.max_bytes);
        let limit_remaining = limit.is_some() && start + t.output_lines < all_lines.len();
        Ok(json!({
            "ok": true,
            "content": t.content,
            "start_line": start_line,
            "total_lines": total_file_lines,
            "truncated": t.truncated || limit_remaining,
        }))
    }
}
