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
        // A host-owned target carries the modes its first write applies, so
        // it is created private and only here; a workspace path keeps the
        // modes the workspace gives it.
        let host_file = self
            .memory
            .as_ref()
            .filter(|file| file.path == resolved)
            .map(|file| (file.dir_mode, file.file_mode));
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = resolved.parent() {
                match host_file {
                    Some((dir_mode, _)) => create_dir_with_mode(parent, dir_mode)?,
                    None => std::fs::create_dir_all(parent)?,
                }
            }
            atomic_write(&resolved, text.as_bytes())?;
            if let Some((_, file_mode)) = host_file {
                set_file_mode(&resolved, file_mode)?;
            }
            Ok(())
        })
        .await
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?;
        Ok(json!({ "ok": true, "content": content }))
    }
}
