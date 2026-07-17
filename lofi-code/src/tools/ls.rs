#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// List every directory entry under `dir` (empty/`.` means the root).
    ///
    /// Entries are returned as sorted paths relative to the root. The result
    /// is complete or the call errors when the hard safety ceiling is
    /// exceeded.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root or the directory
    /// exceeds [`MAX_LS_ENTRIES`].
    #[allow(clippy::unused_async)]
    pub async fn ls(&self, dir: &str) -> Result<Value> {
        let resolved = self.resolve_for_read(dir)?;
        let strip_root = self.root_for(&resolved).to_path_buf();
        let label = dir.to_string();
        let mut entries = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut entries = Vec::new();
            for entry in std::fs::read_dir(&resolved)? {
                let entry = entry?;
                let rel = entry
                    .path()
                    .strip_prefix(&strip_root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                entries.push(rel);
            }
            if entries.len() > MAX_LS_ENTRIES {
                return Err(Error::Tool(format!(
                    "ls: exceeded {MAX_LS_ENTRIES}-entry limit; narrow the directory or page with lofi.bash"
                )));
            }
            Ok(entries)
        })
        .await
        .map_err(|e| Error::Tool(format!("ls {label}: {e}")))??;
        entries.sort();
        Ok(json!({ "ok": true, "entries": entries }))
    }
}
