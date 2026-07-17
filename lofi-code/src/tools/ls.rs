#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// List directory entries under `dir` (empty/`.` means the root).
    ///
    /// Entries are returned as paths relative to the root, sorted,
    /// newline-joined.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root.
    #[allow(clippy::unused_async)]
    pub async fn ls(&self, dir: &str) -> Result<Value> {
        let resolved = resolve_under(&self.root, dir)?;
        let root = self.root.clone();
        let label = dir.to_string();
        let entries =
            tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<String>, bool)> {
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
                    if entries.len() > MAX_FIND_RESULTS {
                        break;
                    }
                }
                let truncated = entries.len() > MAX_FIND_RESULTS;
                if truncated {
                    entries.truncate(MAX_FIND_RESULTS);
                }
                entries.sort();
                Ok((entries, truncated))
            })
            .await
            .map_err(|e| Error::Tool(format!("ls {label}: {e}")))?
            .map_err(|e| Error::Tool(format!("ls {label}: {e}")))?;
        let mut out = entries.0.join("\n");
        if entries.1 {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("<truncated>");
        }
        Ok(json!(out))
    }
}
