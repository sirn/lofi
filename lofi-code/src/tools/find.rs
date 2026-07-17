#[allow(clippy::wildcard_imports)]
use super::*;
use globset::Glob;
use lofi_error::{Error, Result};
use std::fmt::Write as _;
use serde_json::{json, Value};

/// Default result cap for `find`.
const DEFAULT_FIND_LIMIT: usize = 1000;

impl BuiltinTools {
    /// Recursively find files under `dir` (default root) matching `glob`.
    ///
    /// `limit` caps the number of results (default 1000). Returns newline-
    /// joined relative paths, sorted. Output is head-truncated to 50 KB /
    /// 2000 lines.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root or the glob is
    /// invalid.
    #[allow(clippy::unused_async)]
    pub async fn find(&self, glob: &str, dir: Option<&str>, limit: Option<u64>) -> Result<Value> {
        let base = resolve_under(&self.root, dir.unwrap_or(""))?;
        let matcher = Glob::new(glob)
            .map_err(|e| Error::Tool(format!("invalid glob {glob:?}: {e}")))?
            .compile_matcher();
        let root = self.root.clone();
        let max = limit.map_or(DEFAULT_FIND_LIMIT, |l| l as usize);
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
                max,
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
            // Apply the user-visible head truncation on top of the result cap.
            let t = truncate_head(&out);
            let mut out = t.content;
            if t.truncated {
                let _ = write!(
                    out,
                    "\n\n[Showing {} of {} results ({} limit).]",
                    t.output_lines,
                    hits.len(),
                    format_size(50 * 1024)
                );
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Tool(format!("find {glob}: {e}")))??;
        Ok(json!(out))
    }
}