use std::io::Write;
use std::path::Path;

use lofi_error::{Error, Result};
#[cfg(test)]
use lofi_types::SessionEventKind;
use lofi_types::{Message, SessionEvent};
use serde::Serialize;

#[cfg(test)]
use super::parse_event;
use super::short_id;

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

pub(super) enum AppendParent<'a> {
    Explicit(Option<&'a str>),
    #[cfg(test)]
    PhysicalEof,
}

impl AppendParent<'_> {
    #[cfg_attr(not(test), allow(unused_variables, clippy::unnecessary_wraps))]
    fn resolve(self, path: &Path) -> Result<Option<String>> {
        match self {
            Self::Explicit(parent) => Ok(parent.map(str::to_string)),
            #[cfg(test)]
            Self::PhysicalEof => last_event_id(path),
        }
    }
}

/// Append session events to a transcript file (one JSON line each). The
/// file is synchronized before returning so a crash after the turn still has
/// the data.
/// Each event is stamped with a fresh `id` (any incoming `id` is
/// overwritten) and chained to the previous one. A supplied `parent_hint`
/// selects an explicit branch parent; otherwise this low-level compatibility
/// helper continues from physical EOF. Active apps use [`SessionCursor`],
/// which always supplies its explicit logical parent instead.
/// # Errors
/// Returns [`Error::Io`] on open/write failure or [`Error::State`] on a
/// serialization failure.
#[cfg(test)]
pub(crate) fn append_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent_hint: Option<&str>,
) -> Result<(u64, u64)> {
    let parent = parent_hint.map_or(AppendParent::PhysicalEof, |id| {
        AppendParent::Explicit(Some(id))
    });
    append_events_from(path, events, parent)
}

#[cfg(test)]
fn append_events_from(
    path: &Path,
    events: &mut [SessionEvent],
    parent: AppendParent<'_>,
) -> Result<(u64, u64)> {
    if events.is_empty() {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        return Ok((len, len));
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    let mut parent = parent.resolve(path)?;
    // Assign a fresh id to each event. `parent_id` is auto-filled only when
    // the event did not set one explicitly, preserving callers that construct
    // a branch inside one append batch.
    for ev in events.iter_mut() {
        ev.id = short_id();
        if ev.parent_id.is_none() {
            ev.parent_id.clone_from(&parent);
        }
        parent = Some(ev.id.clone());
    }
    append_prepared_events(path, events)
}

#[derive(Debug, Clone, Copy)]
pub struct CompactionCounts {
    pub summarized: usize,
    pub represented: usize,
    pub kept: usize,
}

/// Append a compaction checkpoint as one batch: edited kept-tail messages
/// followed by the marker. A failed write is rolled back to the original file
/// length so the transcript cannot expose a partial checkpoint as its leaf.
/// # Errors
/// Propagates transcript read, serialization, and write failures.
#[cfg(test)]
pub(crate) fn append_compaction(
    path: &Path,
    kept_messages: &[Message],
    parent_hint: Option<&str>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: CompactionCounts,
) -> Result<(u64, u64, String)> {
    let parent = parent_hint.map_or(AppendParent::PhysicalEof, |id| {
        AppendParent::Explicit(Some(id))
    });
    append_compaction_from(
        path,
        kept_messages,
        parent,
        summary,
        summarized_range,
        counts,
    )
}

pub(super) fn append_compaction_from(
    path: &Path,
    kept_messages: &[Message],
    parent: AppendParent<'_>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: CompactionCounts,
) -> Result<(u64, u64, String)> {
    #[derive(Serialize)]
    struct MessageCheckpoint<'a> {
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<&'a str>,
        #[serde(rename = "type")]
        kind: &'static str,
        role: lofi_types::Role,
        blocks: &'a [lofi_types::ContentBlock],
    }
    #[derive(Serialize)]
    struct CompactionCheckpoint<'a> {
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<&'a str>,
        #[serde(rename = "type")]
        kind: &'static str,
        summary: &'a str,
        first_kept_entry_id: &'a str,
        summarized_range: &'a [String; 2],
        checkpointed_tail: bool,
        summarized: usize,
        represented: usize,
        kept: usize,
    }

    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    let parent = parent.resolve(path)?;
    let ids: Vec<String> = (0..=kept_messages.len()).map(|_| short_id()).collect();
    let (byte_start, byte_end) = append_records(path, |writer| {
        for (index, message) in kept_messages.iter().enumerate() {
            let event = MessageCheckpoint {
                id: &ids[index],
                parent_id: if index == 0 {
                    parent.as_deref()
                } else {
                    Some(&ids[index - 1])
                },
                kind: "message",
                role: message.role,
                blocks: &message.blocks,
            };
            writer.record(&event)?;
        }
        let marker_index = kept_messages.len();
        let marker = CompactionCheckpoint {
            id: &ids[marker_index],
            parent_id: if marker_index == 0 {
                parent.as_deref()
            } else {
                Some(&ids[marker_index - 1])
            },
            kind: "compaction",
            summary,
            first_kept_entry_id: ids
                .first()
                .filter(|_| marker_index > 0)
                .map_or("", String::as_str),
            summarized_range,
            checkpointed_tail: true,
            summarized: counts.summarized,
            represented: counts.represented,
            kept: counts.kept,
        };
        writer.record(&marker)?;
        Ok(())
    })?;
    Ok((byte_start, byte_end, ids[kept_messages.len()].clone()))
}

pub(super) fn append_prepared_events(path: &Path, events: &[SessionEvent]) -> Result<(u64, u64)> {
    append_records(path, |writer| {
        for event in events {
            writer.record(event)?;
        }
        Ok(())
    })
}

/// # Errors
/// Returns [`Error::Io`] on a read failure other than the file not existing.
#[cfg(test)]
fn last_event_id(path: &Path) -> Result<Option<String>> {
    use std::io::{BufRead, BufReader};
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut last: Option<String> = None;
    let mut first = true;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        if first {
            first = false;
            continue;
        }
        if let Ok(ev) = parse_event(trimmed) {
            if !matches!(ev.kind, SessionEventKind::Cursor { .. }) {
                last = Some(ev.id);
            }
        }
    }
    Ok(last)
}
