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
        // The memory file is the one host-owned path a write can reach: its
        // directory belongs to the private state tree, not to the workspace,
        // so it is created private and only here, on the first write.
        let private = self.memory.as_deref() == Some(resolved.as_path());
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = resolved.parent() {
                if private {
                    ensure_private_dir(parent)?;
                } else {
                    std::fs::create_dir_all(parent)?;
                }
            }
            atomic_write(&resolved, text.as_bytes())?;
            if private {
                set_private_file(&resolved)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?;
        Ok(json!({ "ok": true, "content": content }))
    }
}
