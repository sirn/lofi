#[allow(clippy::wildcard_imports)]
use super::*;
use globset::Glob;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Recursively find files under `dir` (default root) matching `glob`.
    ///
    /// Returns newline-joined relative paths, sorted.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root or the glob is
    /// invalid.
    #[allow(clippy::unused_async)]
    pub async fn find(&self, glob: &str, dir: Option<&str>) -> Result<Value> {
        let base = resolve_under(&self.root, dir.unwrap_or(""))?;
        let matcher = Glob::new(glob)
            .map_err(|e| Error::Tool(format!("invalid glob {glob:?}: {e}")))?
            .compile_matcher();
        let root = self.root.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut hits = Vec::new();
            let mut visited = 0usize;
            // Match during traversal and stop once we have enough hits, so a
            // matching file is not missed because an earlier non-matching
            // region filled a candidate cap. A separate visited cap bounds
            // runtime; `truncated` signals the result is incomplete.
            let truncated = find_walk(
                &base,
                &root,
                &matcher,
                &mut hits,
                MAX_FIND_RESULTS,
                &mut visited,
                MAX_FIND_VISITED,
            )?;
            hits.sort();
            let mut out = hits.join("\n");
            if truncated {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("<truncated>");
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Tool(format!("find {glob}: {e}")))??;
        Ok(json!(out))
    }
}
