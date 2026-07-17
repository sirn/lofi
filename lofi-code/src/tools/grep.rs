use super::{
    format_size, parse_grep_args, resolve_under, walk_files_capped, BuiltinTools,
    MAX_GREP_FILE_BYTES, MAX_GREP_OUTPUT_BYTES, MAX_GREP_ROWS, MAX_GREP_VISITED,
};
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Grep files under `path` (default root, recursive) for `pattern`.
    ///
    /// `pattern` is either a string or an object `{ regex, ic?, ctx? }`.
    /// Results contain full matching and context lines. The result is complete
    /// or the call errors when an output or traversal ceiling is exceeded.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] on an invalid pattern, an escaped path, or when
    /// a safety ceiling is exceeded.
    #[allow(clippy::unused_async)]
    #[allow(clippy::too_many_lines)]
    pub async fn grep(&self, pattern: Value, path: Option<&str>) -> Result<Value> {
        let (re_src, ic, ctx) = parse_grep_args(pattern)?;
        let mut builder = regex::RegexBuilder::new(&re_src);
        builder.case_insensitive(ic);
        let re = builder
            .build()
            .map_err(|e| Error::Tool(format!("invalid regex {re_src:?}: {e}")))?;
        let base = resolve_under(&self.root, path.unwrap_or(""))?;
        let root = self.root.clone();
        let re_src_inner = re_src.clone();
        let (matches, oversize, unreadable) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<Value>, u64, u64)> {
                let mut files = Vec::new();
                let mut visited = 0usize;
                let traversal_capped = if base.is_file() {
                    files.push(base.clone());
                    false
                } else {
                    walk_files_capped(&base, &mut files, &mut visited, MAX_GREP_VISITED)?
                };
                if traversal_capped {
                    return Err(Error::Tool(format!(
                        "grep {re_src_inner}: exceeded {MAX_GREP_VISITED}-file traversal limit; narrow the path"
                    )));
                }
                files.sort();

                let mut matches = Vec::new();
                let mut bytes = 0usize;
                let mut oversize = 0u64;
                let mut unreadable = 0u64;
                for file in &files {
                    let Ok(meta) = std::fs::metadata(file) else {
                        unreadable += 1;
                        continue;
                    };
                    if meta.len() > MAX_GREP_FILE_BYTES {
                        oversize += 1;
                        continue;
                    }
                    let rel = file
                        .strip_prefix(&root)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let Ok(text) = std::fs::read_to_string(file) else {
                        unreadable += 1;
                        continue;
                    };
                    let lines: Vec<&str> = text.lines().collect();
                    for (i, candidate) in lines.iter().copied().enumerate() {
                        if !re.is_match(candidate) {
                            continue;
                        }
                        let start = i.saturating_sub(ctx);
                        let end = i.saturating_add(ctx).min(lines.len().saturating_sub(1));
                        for (j, line) in lines.iter().copied().enumerate().take(end + 1).skip(start) {
                            if matches.len() >= MAX_GREP_ROWS {
                                return Err(Error::Tool(format!(
                                    "grep {re_src_inner}: exceeded {MAX_GREP_ROWS}-row limit; narrow the regex or path"
                                )));
                            }
                            let entry = line.len() + rel.len() + 16;
                            if bytes.saturating_add(entry) > MAX_GREP_OUTPUT_BYTES {
                                return Err(Error::Tool(format!(
                                    "grep {re_src_inner}: exceeded {} output limit; narrow the regex or path",
                                    format_size(MAX_GREP_OUTPUT_BYTES)
                                )));
                            }
                            bytes += entry;
                            matches.push(json!({
                                "file": rel.as_str(),
                                "line": j + 1,
                                "content": line,
                                "matched": j == i,
                            }));
                        }
                    }
                }
                Ok((matches, oversize, unreadable))
            })
            .await
            .map_err(|e| Error::Tool(format!("grep {re_src}: {e}")))??;
        Ok(json!({
            "ok": true,
            "matches": matches,
            "skipped": { "oversize": oversize, "unreadable": unreadable },
        }))
    }
}