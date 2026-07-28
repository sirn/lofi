#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// # Errors
    /// Returns an error for invalid arguments, unsafe paths, or filesystem failures.
    #[allow(clippy::unused_async)]
    pub async fn write(&self, args: Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("write: missing 'path'".into()))?
            .to_owned();
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("write: missing 'text'".into()))?
            .to_owned();
        let content = text.clone();
        reject_symlink_leaf(&self.root, &path, &format!("write {path}"))?;
        let resolved = resolve_under(&self.root, &path)?;
        reject_non_regular(&format!("write {path}"), &resolved)?;
        let label = path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent)?;
            }
            atomic_write(&resolved, text.as_bytes())?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?;
        Ok(json!({ "ok": true, "content": content }))
    }
}
