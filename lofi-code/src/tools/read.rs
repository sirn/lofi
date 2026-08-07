#[allow(clippy::wildcard_imports)]
use super::*;
use base64::Engine as _;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

/// Detect an image file by extension and return its media type. The read
/// boundary is JSON-only, so an image cannot cross as bytes; instead `read`
/// returns a tagged `{type:"image"}` payload that the host upgrades into a
/// real `ContentBlock::Image` on a user-role message (the only role whose
/// image blocks providers serialize for vision).
fn image_media_type(path: &str) -> Option<&'static str> {
    let lower = path.rsplit('/').next()?.to_ascii_lowercase();
    let ext = lower.rsplit('.').next()?;
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

impl BuiltinTools {
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the root, exceeds the file
    /// size limit, cannot be read, or `offset` is beyond the end of the file.
    pub async fn read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> Result<Value> {
        // Symlinks are allowed: resolve_for_read canonicalizes the path
        // (following symlinks) and checks the result is under an allowed
        // root — that is the security boundary, not symlink rejection.
        let resolved = self.resolve_for_read(path)?;
        reject_non_regular(&format!("read {path}"), &resolved)?;
        let label = path.to_string();
        let label_inner = label.clone();
        let offset = offset.unwrap_or(1).max(1);
        let media_type = image_media_type(path);

        // Image files short-circuit the text path entirely: return a tagged
        // base64 payload the host upgrades into a real vision image block.
        // Reading binary as lossy UTF-8 would corrupt it and waste tokens.
        if let Some(media_type) = media_type {
            let label_img = label.clone();
            let bytes = tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
                let meta = std::fs::metadata(&resolved)?;
                if meta.len() > MAX_READ_BYTES as u64 {
                    return Err(Error::Tool(format!(
                        "read {label_img}: image is {} (exceeds {} limit)",
                        format_size(meta.len() as usize),
                        format_size(MAX_READ_BYTES)
                    )));
                }
                Ok(std::fs::read(&resolved)?)
            })
            .await
            .map_err(|e| Error::Tool(format!("read {label}: {e}")))??;
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(json!({
                "ok": true,
                "type": "image",
                "media_type": media_type,
                "data_b64": data_b64,
            }));
        }

        let text = tokio::task::spawn_blocking(move || -> Result<String> {
            let meta = std::fs::metadata(&resolved)?;
            if meta.len() > MAX_READ_BYTES as u64 {
                return Err(Error::Tool(format!(
                    "read {label_inner}: file is {} (exceeds {} limit); use bash to inspect with head/sed/grep",
                    format_size(meta.len() as usize),
                    format_size(MAX_READ_BYTES)
                )));
            }
            let bytes = std::fs::read(&resolved)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        })
        .await
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))??;

        let all_lines: Vec<&str> = text.split('\n').collect();
        let total_file_lines = all_lines.len();
        let start = (offset as usize).saturating_sub(1);
        if start >= all_lines.len() {
            return Err(Error::Tool(format!(
                "read {label}: offset {offset} is beyond end of file ({total_file_lines} lines total)"
            )));
        }
        let selected = if let Some(lim) = limit {
            let end = (start + lim as usize).min(all_lines.len());
            all_lines[start..end].join("\n")
        } else {
            all_lines[start..].join("\n")
        };
        let start_line = start + 1;
        let cap = self.truncate;
        let t = truncate_head_with(&selected, cap.max_lines, cap.max_bytes);
        let limit_remaining = limit.is_some() && start + t.output_lines < all_lines.len();
        Ok(json!({
            "ok": true,
            "content": t.content,
            "start_line": start_line,
            "total_lines": total_file_lines,
            "truncated": t.truncated || limit_remaining,
        }))
    }
}
