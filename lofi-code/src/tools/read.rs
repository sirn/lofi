#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

impl BuiltinTools {
    /// Read a file under the root as a UTF-8 string.
    ///
    /// The blocking read runs on `spawn_blocking` so a huge file can't freeze
    /// the TUI event loop, and is truncated at [`MAX_READ_BYTES`].
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the root or the file
    /// cannot be read.
    pub async fn read(&self, path: &str) -> Result<Value> {
        reject_symlink_leaf(&self.root, path, &format!("read {path}"))?;
        let resolved = resolve_under(&self.root, path)?;
        reject_non_regular(&format!("read {path}"), &resolved)?;
        let label = path.to_string();
        let text = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use std::io::Read as _;
            // Stream at most MAX+1 bytes so truncation is detectable without
            // reading an entire huge file into memory.
            let file = std::fs::File::open(&resolved)?;
            let mut buf = Vec::new();
            file.take(MAX_READ_BYTES as u64 + 1).read_to_end(&mut buf)?;
            let truncated = buf.len() > MAX_READ_BYTES;
            let slice = if truncated {
                &buf[..MAX_READ_BYTES]
            } else {
                &buf[..]
            };
            let mut text = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                text.push_str("\n<output truncated>");
            }
            Ok(text)
        })
        .await
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))?;
        Ok(json!(text))
    }
}
