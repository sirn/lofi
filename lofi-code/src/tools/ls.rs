#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use std::fmt::Write as _;
use serde_json::{json, Value};

/// Default entry cap for `ls`.
const DEFAULT_LS_LIMIT: usize = 500;

impl BuiltinTools {
    /// List directory entries under `dir` (empty/`.` means the root).
    ///
    /// `limit` caps the number of entries returned (default 500). Entries are
    /// returned as paths relative to the root, sorted, newline-joined. Output
    /// is head-truncated to 50 KB / 2000 lines.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root.
    #[allow(clippy::unused_async)]
    pub async fn ls(&self, dir: &str, limit: Option<u64>) -> Result<Value> {
        let resolved = resolve_under(&self.root, dir)?;
        let root = self.root.clone();
        let label = dir.to_string();
        let max = limit.map_or(DEFAULT_LS_LIMIT, |l| l as usize);
        let entries =
            tokio::task::spawn_blocking(move || -> std::io::Result<Vec<String>> {
                let mut entries = Vec::new();
                // Collect one past the cap so truncation is detectable without
                // traversing the whole (potentially huge) directory.
                for entry in std::fs::read_dir(&resolved)? {
                    let entry = entry?;
                    let rel = entry
                        .path()
                        .strip_prefix(&root)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    entries.push(rel);
                    if entries.len() > max {
                        break;
                    }
                }
                entries.sort();
                entries.truncate(max);
                Ok(entries)
            })
            .await
            .map_err(|e| Error::Tool(format!("ls {label}: {e}")))?
            .map_err(|e| Error::Tool(format!("ls {label}: {e}")))?;
        let joined = entries.join("\n");
        let t = truncate_head(&joined);
        let mut out = t.content;
        if t.truncated {
            let _ = write!(
                out,
                "\n\n[Showing {} of {} entries ({} limit).]",
                t.output_lines,
                entries.len(),
                format_size(50 * 1024)
            );
        }
        Ok(json!(out))
    }
}