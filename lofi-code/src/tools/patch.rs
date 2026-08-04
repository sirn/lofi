use super::{atomic_write, reject_non_regular, reject_symlink_leaf, resolve_under, BuiltinTools};
use lofi_error::{Error, Result};
use serde_json::{json, Value};

/// One hunk from a unified diff: the context+removed lines that anchor it
/// (`old`) and the replacement lines (`new`).
#[derive(Debug)]
struct Hunk {
    old: Vec<String>,
    new: Vec<String>,
}

impl BuiltinTools {
    /// Apply a unified-diff patch to `path`. Accepts conventional `--- a/ /
    /// +++ b/` headers, bare `@@` hunks, and `*** Begin/Update File` markers;
    /// the path header is advisory. Hunk context is matched with a fuzz window
    /// so a modest line-number drift does not reject a otherwise-correct patch.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the patch is malformed, a hunk's context
    /// cannot be matched in the file, or the write fails.
    #[allow(clippy::unused_async)]
    pub async fn patch(&self, args: Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("patch: missing 'path'".into()))?
            .to_owned();
        let diff_text = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("patch: missing 'patch'".into()))?
            .to_owned();
        reject_symlink_leaf(&self.root, &path, &format!("patch {path}"))?;
        let resolved = resolve_under(&self.root, &path)?;
        reject_non_regular(&format!("patch {path}"), &resolved)?;
        let label = path.clone();
        let hunks = parse_unified_patch(&diff_text)
            .map_err(|e| Error::Tool(format!("patch {label}: {e}")))?;
        if hunks.is_empty() {
            return Err(Error::Tool(format!("patch {label}: no hunks in patch")));
        }
        let read_label = label.clone();
        let content = tokio::task::spawn_blocking(move || -> Result<String> {
            let buf = std::fs::read(&resolved)
                .map_err(|e| Error::Tool(format!("patch {read_label}: {e}")))?;
            String::from_utf8(buf).map_err(|e| Error::Tool(format!("patch {read_label}: {e}")))
        })
        .await
        .map_err(|e| Error::Tool(format!("patch {label}: {e}")))??;
        let updated = apply_hunks(&content, &hunks)
            .map_err(|e| Error::Tool(format!("patch {label}: {e}")))?;
        let resolved2 = resolve_under(&self.root, &path)?;
        tokio::task::spawn_blocking(move || {
            atomic_write(&resolved2, updated.as_bytes())
                .map_err(|e| Error::Tool(format!("patch: {e}")))
        })
        .await
        .map_err(|e| Error::Tool(format!("patch {label}: {e}")))??;
        Ok(json!({
            "ok": true,
            "path": label,
            "hunks": hunks.len(),
        }))
    }
}

fn parse_unified_patch(patch: &str) -> std::result::Result<Vec<Hunk>, String> {
    let mut hunks = Vec::new();
    let mut cur_old: Vec<String> = Vec::new();
    let mut cur_new: Vec<String> = Vec::new();
    let mut in_hunk = false;

    for line in patch.lines() {
        if line.starts_with("@@") {
            if in_hunk {
                hunks.push(Hunk {
                    old: std::mem::take(&mut cur_old),
                    new: std::mem::take(&mut cur_new),
                });
            }
            in_hunk = true;
            continue;
        }
        if !in_hunk {
            continue;
        }
        let (marker, body) = line.split_at(line.len().min(1));
        match marker {
            " " => {
                cur_old.push(body.to_string());
                cur_new.push(body.to_string());
            }
            "-" => cur_old.push(body.to_string()),
            "+" => cur_new.push(body.to_string()),
            "\\" => {}
            _ => return Err(format!("unrecognized patch line: {line:?}")),
        }
    }
    if in_hunk {
        hunks.push(Hunk {
            old: cur_old,
            new: cur_new,
        });
    }
    Ok(hunks)
}

fn find_hunk(lines: &[&str], old: &[String], near: usize) -> Option<usize> {
    if old.is_empty() {
        return Some(near.min(lines.len()));
    }
    let n = old.len();
    for offset in 0..=lines.len().saturating_sub(1).max(n) {
        for pos in [near.wrapping_sub(offset), near.wrapping_add(offset)] {
            if pos >= lines.len() || pos.saturating_add(n) > lines.len() {
                continue;
            }
            let exact = lines[pos..pos + n]
                .iter()
                .zip(old.iter())
                .all(|(a, b)| a == &b.as_str());
            if exact {
                return Some(pos);
            }
            let trimmed = lines[pos..pos + n]
                .iter()
                .zip(old.iter())
                .all(|(a, b)| a.trim_end() == b.trim_end());
            if trimmed {
                return Some(pos);
            }
        }
    }
    None
}

fn apply_hunks(content: &str, hunks: &[Hunk]) -> std::result::Result<String, String> {
    let mut lines: Vec<&str> = content.lines().collect();
    let mut cursor = 0usize;
    for h in hunks {
        let pos = find_hunk(&lines, &h.old, cursor)
            .ok_or_else(|| format!("hunk context not found near line {cursor}"))?;
        if pos < cursor && !h.old.is_empty() {
            return Err(format!("hunks applied out of order at line {pos}"));
        }
        let remove = h.old.len();
        let insert: Vec<&str> = h.new.iter().map(String::as_str).collect();
        lines.splice(pos..pos + remove, insert);
        cursor = pos + h.new.len();
    }
    let mut out = lines.join("\n");
    if content.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn parses_hunk() {
        let h = parse_unified_patch("@@ -1,3 +1,3 @@\n keep\n-old\n+new\n keep\n").unwrap();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].old, vec!["keep", "old", "keep"]);
        assert_eq!(h[0].new, vec!["keep", "new", "keep"]);
    }

    #[test]
    fn applies_in_order() {
        let content = "a\nb\nc\n";
        let h = parse_unified_patch("@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n").unwrap();
        assert_eq!(apply_hunks(content, &h).unwrap(), "a\nB\nc\n");
    }

    #[test]
    fn rejects_missing_context() {
        let content = "x\ny\n";
        let h = parse_unified_patch("@@ -2,1 +2,1 @@\n q\n").unwrap();
        assert!(apply_hunks(content, &h).is_err());
    }
}
