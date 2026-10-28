use std::path::Path;

use lofi_error::{Error, Result};

/// Character budget for the workspace memory. The compaction hook clips at
/// the same limit, so a write within it is never clipped on read.
pub const MEMORY_LIMIT: usize = 3_000;

/// Read the workspace memory. A missing file is the normal "no memory yet"
/// case and reads as an empty string; any other failure must surface, so
/// the model does not rewrite over state it never saw.
/// # Errors
/// Returns [`Error::Tool`] on a read failure other than a missing file.
pub(super) fn read(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(Error::Tool(format!(
            "cannot read the workspace memory: {err}"
        ))),
    }
}

/// Replace the workspace memory. The whole file is rewritten, never
/// appended to: the memory carries current state, and an append-only file
/// grows past the limit and loses the oldest state first.
/// # Errors
/// Returns [`Error::Tool`] when the text exceeds [`MEMORY_LIMIT`]
/// characters, or on a filesystem failure.
pub(super) fn write(path: &Path, text: &str) -> Result<usize> {
    let count = text.chars().count();
    if count > MEMORY_LIMIT {
        return Err(Error::Tool(format!(
            "workspace memory is {count} characters; rewrite it within {MEMORY_LIMIT}"
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| Error::Tool("workspace memory path has no parent".to_string()))?;
    std::fs::create_dir_all(parent)
        .map_err(|err| Error::Tool(format!("cannot create the workspace memory dir: {err}")))?;
    super::fs::atomic_write(path, text.as_bytes())
        .map_err(|err| Error::Tool(format!("cannot write the workspace memory: {err}")))?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn read_treats_a_missing_file_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(&dir.path().join("memory.md")).unwrap(), "");
    }

    #[test]
    fn write_replaces_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("memory.md");
        write(&path, "first").unwrap();
        write(&path, "second").unwrap();
        assert_eq!(read(&path).unwrap(), "second");
    }

    #[test]
    fn write_rejects_text_past_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        let err = write(&path, &"x".repeat(MEMORY_LIMIT + 1)).unwrap_err();
        assert!(err.to_string().contains("rewrite it within"));
        assert!(!path.exists());
    }
}
