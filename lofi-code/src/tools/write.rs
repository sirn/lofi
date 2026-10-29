#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// # Errors
    /// Returns an error for invalid arguments, unsafe paths, or filesystem failures.
    pub async fn write(&self, args: Value) -> Result<Value> {
        let path = require_str("write", &args, "path")?;
        let text = require_str("write", &args, "text")?;
        let content = text.clone();
        reject_symlink_leaf(&self.root, &path, &format!("write {path}"))?;
        let resolved = self.resolve_mutate(&path).await?;
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
