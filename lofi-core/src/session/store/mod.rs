#[cfg(test)]
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use lofi_error::{Error, Result};
use lofi_types::{Message, RunModel, SessionEvent, SessionEventKind};
use serde::{Deserialize, Serialize};

mod append;
mod cursor;
mod index;
mod index_parser;
mod listing;

pub use append::CompactionCounts;
#[cfg(test)]
pub(crate) use append::{append_compaction, append_events};
pub use cursor::{lineage_path, SessionCursor, SessionSnapshot, SessionTreeSnapshot};
pub use index::{EventIndex, IndexId, IndexKind, SessionIndexEntry};
pub use listing::{SessionEntry, SessionFile, SessionStore};

use index::index_kind_for_event;

#[cfg(test)]
mod tests;

pub enum RecoveryMessage {
    Message(Message),
    NotMessage,
}

/// Pre-release: there is one transcript format and it is versioned 1.
pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub created: u64,
    pub cwd: String,
    pub model: RunModel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    meta: SessionMeta,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// # Errors
/// Returns [`Error::State`] if the file is missing a header, has an
/// unsupported version, or an event line fails to parse; [`Error::Io`] on a
/// read failure.
#[cfg(test)]
fn load(path: &Path) -> Result<(SessionMeta, Vec<SessionEvent>, Vec<u64>, u64)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = String::new();
    let mut pos: u64 = 0;
    let header_line = loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(Error::State(format!(
                "session file has no header: {}",
                path.display()
            )));
        }
        pos += n as u64;
        let line = buf.trim_end_matches(['\n', '\r']);
        if !line.is_empty() {
            break line.to_string();
        }
    };
    let header: Header =
        serde_json::from_str(&header_line).map_err(|e| Error::State(format!("json: {e}")))?;
    if header.meta.version != SESSION_VERSION {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let mut events = Vec::new();
    let mut offsets = Vec::new();
    let mut i = 0usize;
    loop {
        buf.clear();
        let line_start = pos;
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        pos += n as u64;
        let line = buf.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let ev = parse_event(line).map_err(|e| {
            Error::State(format!("parse event {} in {}: {e}", i + 1, path.display()))
        })?;
        if matches!(ev.kind, SessionEventKind::Cursor { .. }) {
            continue;
        }
        events.push(ev);
        offsets.push(line_start);
        i += 1;
    }
    Ok((header.meta, events, offsets, pos))
}

/// # Errors
/// Returns [`Error::State`] if the line is not a serialised [`SessionEvent`].
pub fn parse_event(line: &str) -> Result<SessionEvent> {
    serde_json::from_str::<SessionEvent>(line)
        .map_err(|e| Error::State(format!("parse event: {e}")))
}

#[must_use]
pub fn leaf_id(events: &[SessionEvent]) -> Option<&str> {
    events.last().map(|e| e.id.as_str())
}

/// Derive a structural [`EventIndex`] for an in-memory event slice. Both the
/// transcript view (index-backed) and the memory view (synthesized events) must
/// walk lineage through [`lineage_path`]; deriving the index here lets the
/// memory view use that same walk instead of a second algorithm.
#[must_use]
fn index_for_events(events: &[SessionEvent]) -> Vec<EventIndex> {
    events
        .iter()
        .map(|event| EventIndex {
            id: IndexId::parse(event.id.clone()),
            parent_id: event.parent_id.clone().map(IndexId::parse),
            offset: 0,
            end_offset: 0,
            kind: index_kind_for_event(&event.kind),
        })
        .collect()
}

#[must_use]
pub fn active_path(events: &[SessionEvent], leaf_id: &str) -> Vec<usize> {
    let index = index_for_events(events);
    let by_id: std::collections::HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.id.is_empty())
        .map(|(i, entry)| (&entry.id, i))
        .collect();
    lineage_path(&index, &by_id, leaf_id)
}

#[must_use]
pub fn active_path_from_leaf(events: &[SessionEvent]) -> Vec<usize> {
    leaf_id(events).map_or_else(Vec::new, |id| active_path(events, id))
}

#[must_use]
/// The model recorded on a terminal turn marker, or None for any other event.
/// Shared by the event-slice and index projections so both agree on which
/// markers report a run's model.
pub fn turn_outcome_model(kind: &SessionEventKind) -> Option<&RunModel> {
    match kind {
        SessionEventKind::TurnEnd { model, .. }
        | SessionEventKind::TurnFailed { model, .. }
        | SessionEventKind::TurnCancelled { model, .. } => Some(model),
        _ => None,
    }
}

#[must_use]
pub fn last_run_model(events: &[SessionEvent]) -> Option<RunModel> {
    active_path_from_leaf(events)
        .into_iter()
        .rev()
        .find_map(|i| turn_outcome_model(&events[i].kind).cloned())
}
