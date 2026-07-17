#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Replace the single occurrence of `old` with `new` in `path`.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `path` escapes the root, if `old` is absent,
    /// or if `old` occurs more than once (an ambiguous edit).
    #[allow(clippy::unused_async)]
    pub async fn edit(&self, args: Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("edit: missing 'path'".into()))?
            .to_owned();
        let old = args
            .get("old")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("edit: missing 'old'".into()))?
            .to_owned();
        let new = args
            .get("new")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("edit: missing 'new'".into()))?
            .to_owned();
        // Echo the replaced text back so the renderer can show a diff.
        let old_echo = old.clone();
        let new_echo = new.clone();
        reject_symlink_leaf(&self.root, &path, &format!("edit {path}"))?;
        let resolved = resolve_under(&self.root, &path)?;
        reject_non_regular(&format!("edit {path}"), &resolved)?;
        let label = path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::io::Read as _;
            let file = std::fs::File::open(&resolved)
                .map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            let mut buf = Vec::new();
            file.take(MAX_EDIT_BYTES as u64 + 1)
                .read_to_end(&mut buf)
                .map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            if buf.len() > MAX_EDIT_BYTES {
                return Err(Error::Tool(format!(
                    "edit {label}: file exceeds {MAX_EDIT_BYTES} bytes"
                )));
            }
            let content =
                String::from_utf8(buf).map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            let count = content.matches(&old).count();
            if count == 0 {
                return Err(Error::Tool(format!("edit {label}: 'old' not found")));
            }
            if count > 1 {
                return Err(Error::Tool(format!(
                    "edit {label}: 'old' found {count} times; expected exactly one"
                )));
            }
            let updated = content.replacen(&old, &new, 1);
            atomic_write(&resolved, updated.as_bytes())
                .map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Tool(format!("edit {path}: {e}")))??;
        Ok(json!({ "ok": true, "old": old_echo, "new": new_echo }))
    }
}
