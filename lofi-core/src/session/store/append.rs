use std::io::Write;
use std::path::Path;

use lofi_error::{Error, Result};
use serde::Serialize;

pub(super) struct RecordWriter(std::fs::File);

impl RecordWriter {
    pub(super) fn record(&mut self, record: &impl Serialize) -> Result<()> {
        serde_json::to_writer(&mut self.0, record)
            .map_err(|error| Error::State(format!("json: {error}")))?;
        self.0.write_all(b"\n")?;
        Ok(())
    }
}

// Callers keep the transcript lock while selecting parents and writing records.
pub(super) fn append_records(
    path: &Path,
    write: impl FnOnce(&mut RecordWriter) -> Result<()>,
) -> Result<(u64, u64)> {
    let byte_start = std::fs::metadata(path).map_or(0, |m| m.len());
    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    let mut writer = RecordWriter(file);
    if let Err(error) = write(&mut writer).and_then(|()| Ok(writer.0.sync_data()?)) {
        let _ = writer.0.set_len(byte_start);
        return Err(error);
    }
    Ok((byte_start, writer.0.metadata()?.len()))
}
