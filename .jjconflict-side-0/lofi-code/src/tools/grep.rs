use super::{
    format_size, parse_grep_args, walk_files_capped, BuiltinTools, MAX_GREP_FILE_BYTES,
    MAX_GREP_OUTPUT_BYTES, MAX_GREP_ROWS, MAX_GREP_VISITED,
};
use lofi_error::{Error, Result};
use serde_json::{json, Value};

/// Matches and skip counters from scanning one chunk of the file list.
struct ChunkScan {
    matches: Vec<Value>,
    oversize: u64,
    unreadable: u64,
    /// Whether this chunk gathered `MAX_GREP_ROWS` matches; merging then
    /// reports the row ceiling rather than a partial result.
    row_capped: bool,
}

/// Scan a slice of files for `re`, emitting context-bounded match rows in
/// file order. Stops early at `MAX_GREP_ROWS` so a match-heavy tree does not
/// buffer unbounded output before the merge re-applies the caps.
fn scan_chunk(
    files: &[std::path::PathBuf],
    root: &std::path::Path,
    re: &regex::Regex,
    ctx: usize,
) -> ChunkScan {
    let mut matches = Vec::new();
    let mut oversize = 0u64;
    let mut unreadable = 0u64;
    'files: for file in files {
        let Ok(meta) = std::fs::metadata(file) else {
            unreadable += 1;
            continue;
        };
        if meta.len() > MAX_GREP_FILE_BYTES {
            oversize += 1;
            continue;
        }
        let rel = file
            .strip_prefix(root)
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
                    break 'files;
                }
                matches.push(json!({
                    "file": rel.as_str(),
                    "line": j + 1,
                    "content": line,
                    "matched": j == i,
                }));
            }
        }
    }
    ChunkScan {
        row_capped: matches.len() >= MAX_GREP_ROWS,
        matches,
        oversize,
        unreadable,
    }
}

impl BuiltinTools {
    /// # Errors
    /// Returns [`Error::Tool`] on an invalid pattern, an escaped path, or when
    /// a safety ceiling is exceeded.
    #[allow(clippy::unused_async)]
    #[allow(clippy::too_many_lines)]
    pub async fn grep(&self, pattern: Value, path: Option<&str>) -> Result<Value> {
        let (re_src, ic, ctx, filtered) = parse_grep_args(pattern)?;
        let mut builder = regex::RegexBuilder::new(&re_src);
        builder.case_insensitive(ic);
        let re = builder
            .build()
            .map_err(|e| Error::Tool(format!("invalid regex {re_src:?}: {e}")))?;
        let base = self.resolve_for_read(path.unwrap_or(""))?;
        let root = self.root_for(&base).to_path_buf();
        let re_src_inner = re_src.clone();
        let (matches, oversize, unreadable) =
            tokio::task::spawn_blocking(move || -> Result<(Vec<Value>, u64, u64)> {
                let mut files = Vec::new();
                let mut visited = 0usize;
                let traversal_capped = if base.is_file() {
                    files.push(base.clone());
                    false
                } else {
                    walk_files_capped(
                        &base,
                        &root,
                        &mut files,
                        &mut visited,
                        MAX_GREP_VISITED,
                        filtered,
                    )?
                };
                if traversal_capped {
                    return Err(Error::Tool(format!(
                        "grep {re_src_inner}: exceeded {MAX_GREP_VISITED}-file traversal limit; narrow the path"
                    )));
                }
                files.sort();

                // Fan the sorted file list out across cores; each worker
                // scans its chunk independently, then chunks are merged in
                // file order so results stay deterministic regardless of
                // scheduling. The row/byte caps are re-applied at the merge.
                let workers = std::thread::available_parallelism()
                    .map_or(1, std::num::NonZero::get)
                    .min(files.len().max(1));
                let chunk_len = files.len().div_ceil(workers);
                let re_ref = &re;
                let root_ref = &root;
                let mut scans: Vec<ChunkScan> = Vec::new();
                std::thread::scope(|scope| {
                    let mut handles = Vec::new();
                    for chunk in files.chunks(chunk_len.max(1)) {
                        handles.push(scope.spawn(move || {
                            scan_chunk(chunk, root_ref, re_ref, ctx)
                        }));
                    }
                    for handle in handles {
                        if let Ok(scan) = handle.join() {
                            scans.push(scan);
                        }
                    }
                });

                let mut matches = Vec::new();
                let mut bytes = 0usize;
                let mut oversize = 0u64;
                let mut unreadable = 0u64;
                // A chunk that stopped at the row ceiling dropped matches, so
                // the merged result would be silently partial; surface the cap.
                let dropped_rows = scans.iter().any(|s| s.row_capped);
                for scan in &mut scans {
                    oversize += scan.oversize;
                    unreadable += scan.unreadable;
                    for m in scan.matches.drain(..) {
                        let entry = m["content"].as_str().map_or(0, str::len)
                            + m["file"].as_str().map_or(0, str::len)
                            + 16;
                        if bytes.saturating_add(entry) > MAX_GREP_OUTPUT_BYTES {
                            return Err(Error::Tool(format!(
                                "grep {re_src_inner}: exceeded {} output limit; narrow the regex or path",
                                format_size(MAX_GREP_OUTPUT_BYTES)
                            )));
                        }
                        bytes += entry;
                        matches.push(m);
                    }
                }
                if dropped_rows || matches.len() >= MAX_GREP_ROWS {
                    return Err(Error::Tool(format!(
                        "grep {re_src_inner}: exceeded {MAX_GREP_ROWS}-row limit; narrow the regex or path"
                    )));
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
