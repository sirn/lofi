#[allow(clippy::wildcard_imports)]
use super::*;
use globset::Glob;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Recursively find every file under `dir` (default root) matching `glob`.
    ///
    /// Relative paths are sorted. The result is complete or the call errors
    /// when a result or traversal safety ceiling is exceeded.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root, the glob is invalid,
    /// or a safety ceiling is exceeded.
    #[allow(clippy::unused_async)]
    pub async fn find(&self, glob: &str, dir: Option<&str>) -> Result<Value> {
        let base = self.resolve_for_read(dir.unwrap_or(""))?;
        let matcher = Glob::new(glob)
            .map_err(|e| Error::Tool(format!("invalid glob {glob:?}: {e}")))?
            .compile_matcher();
        let root = self.root_for(&base).to_path_buf();
        let glob_inner = glob.to_string();
        let paths = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
            let mut hits = Vec::new();
            let mut visited = 0usize;
            match find_walk(
                &base,
                &root,
                &matcher,
                &mut hits,
                MAX_FIND_RESULTS,
                &mut visited,
                MAX_FIND_VISITED,
            )? {
                WalkLimit::Complete => {
                    hits.sort();
                    Ok(hits)
                }
                WalkLimit::TooManyHits => Err(Error::Tool(format!(
                    "find {glob_inner}: exceeded {MAX_FIND_RESULTS}-result limit; narrow the glob or dir"
                ))),
                WalkLimit::TooManyVisited => Err(Error::Tool(format!(
                    "find {glob_inner}: exceeded {MAX_FIND_VISITED}-entry traversal limit; narrow the glob or dir"
                ))),
            }
        })
        .await
        .map_err(|e| Error::Tool(format!("find {glob}: {e}")))??;
        Ok(json!({ "ok": true, "matches": paths }))
    }
}
