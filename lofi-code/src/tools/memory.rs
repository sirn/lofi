use std::path::Path;

use lofi_error::{Error, Result};

/// Character budget for the workspace memory. The file tools do not enforce
/// it; the compaction hook clips at the same limit, so the model is told to
/// stay within it.
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn read_treats_a_missing_file_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read(&dir.path().join("memory.md")).unwrap(), "");
    }
}
