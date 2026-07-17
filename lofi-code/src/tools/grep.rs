use super::truncate::{format_size, truncate_head, truncate_line};
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Grep files under `path` (default root, recursive) for `pattern`.
    ///
    /// `pattern` is either a string or an object `{ regex, ic?, ctx?, max? }`.
    /// Matching lines are emitted as `file:line:content`; `ctx` surrounding
    /// lines (if requested) get the same prefix and groups are separated by
    /// `--`.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] on an invalid pattern, an escaped path, or a
    /// read failure.
    #[allow(clippy::unused_async)]
    pub async fn grep(&self, pattern: Value, path: Option<&str>) -> Result<Value> {
        let (re_src, ic, ctx, max) = parse_grep_args(pattern)?;
        let mut builder = regex::RegexBuilder::new(&re_src);
        builder.case_insensitive(ic);
        let re = builder
            .build()
            .map_err(|e| Error::Tool(format!("invalid regex {re_src:?}: {e}")))?;
        let base = resolve_under(&self.root, path.unwrap_or(""))?;
        let root = self.root.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String> {
            // A file path scans just that file; a directory recurses with a
            // visited cap so a huge tree can't exhaust memory or hang.
            let mut files = Vec::new();
            let mut visited = 0usize;
            let truncated_walk = if base.is_file() {
                files.push(base.clone());
                false
            } else {
                walk_files_capped(&base, &mut files, &mut visited, MAX_GREP_VISITED)?
            };
            files.sort();

            let budget = MAX_GREP_OUTPUT_BYTES;
            let mut out = String::new();
            let mut emitted = 0usize;
            let max = if max == 0 { DEFAULT_GREP_MAX } else { max };
            let mut truncated_output = false;
            // Append a line only if it fits the remaining output budget;
            // otherwise mark truncated and stop. This bounds total output to
            // `budget` plus at most one line length, rather than overshooting
            // by a multi-MiB context group.
            let mut push_line = |out: &mut String, s: &str| -> bool {
                if out.len() + s.len() > budget {
                    truncated_output = true;
                    false
                } else {
                    out.push_str(s);
                    true
                }
            };
            'outer: for file in &files {
                // Skip oversized files so a single huge artifact can't blow
                // memory by being fully read into a String.
                let Ok(meta) = std::fs::metadata(file) else {
                    continue;
                };
                if meta.len() > MAX_GREP_FILE_BYTES {
                    continue;
                }
                let rel = file
                    .strip_prefix(&root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let Ok(text) = std::fs::read_to_string(file) else {
                    continue;
                };
                let lines: Vec<&str> = text.lines().collect();
                let mut i = 0;
                let mut last_group_end: Option<usize> = None;
                while i < lines.len() {
                    if re.is_match(lines[i]) {
                        let start = i.saturating_sub(ctx);
                        let end = i.saturating_add(ctx).min(lines.len().saturating_sub(1));
                        if let Some(prev_end) = last_group_end {
                            if prev_end + 1 < start && !push_line(&mut out, "--\n") {
                                break 'outer;
                            }
                        }
                        for (j, line) in lines.iter().enumerate().take(end + 1).skip(start) {
                            let capped = truncate_line(line);
                            let rendered = format!("{}:{}:{}\n", rel, j + 1, capped);
                            if !push_line(&mut out, &rendered) {
                                break 'outer;
                            }
                        }
                        last_group_end = Some(end);
                        emitted += 1;
                        if emitted >= max {
                            break 'outer;
                        }
                    }
                    i += 1;
                }
            }
            if truncated_output {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("<output truncated>");
            } else if truncated_walk {
                // The file list was truncated; signal that the search was
                // incomplete even when some matches were found.
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("<truncated>");
            }
            // Apply the user-visible head truncation (50 KB / 2000 lines)
            // on top of the match-count and byte budgets.
            let t = truncate_head(&out);
            let mut out = t.content;
            if t.truncated {
                out.push_str(&format!(
                    "\n\n[Showing {} of {} lines ({} limit).]",
                    t.output_lines,
                    t.total_lines,
                    format_size(50 * 1024)
                ));
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Tool(format!("grep {re_src}: {e}")))??;
        Ok(json!(out))
    }
}
