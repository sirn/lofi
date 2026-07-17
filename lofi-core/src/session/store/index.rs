//! Lightweight session-file index: scan an event log for just the tree
//! shape (ids, parent ids, kinds, byte offsets) without deserializing
//! message content, and random-access a single event by offset.
//!
//! Used by the `/tree` picker to avoid a full [`super::load`].

use std::path::Path;

use lofi_error::{Error, Result};
use lofi_types::SessionEvent;
use serde::Deserialize;

use super::{parse_event, short_id, Header, SessionMeta, SESSION_MIN_VERSION, SESSION_VERSION};/// Lightweight per-event index entry: just enough to build the event tree
/// structure (id, `parent_id`, offset) and identify tree-node kinds, without
/// deserializing message content. Used by `/tree` to avoid a full `load`.
#[derive(Debug, Clone)]
pub struct EventIndex {
    pub id: String,
    pub parent_id: Option<String>,
    /// Byte offset of this event's line in the file — for random-access
    /// label loading via [`load_event_at`].
    pub offset: u64,
    pub kind: IndexKind,
}

/// The kind discriminant extracted by the lightweight scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    UserPrompt,
    AssistantMessage,
    TurnEnd,
    TurnFailed,
    Other,
}

/// Skeleton for the lightweight scan: serde ignores all fields except these
/// (and skips `blocks`/`label`/`usage` content without allocating it).
#[derive(Deserialize)]
struct EventSkeleton {
    #[serde(default)]
    id: String,
    #[serde(default)]
    parent_id: Option<String>,
    #[serde(default, rename = "type")]
    kind_type: String,
    #[serde(default)]
    role: Option<String>,
}

/// Lightweight scan: reads the session file and extracts only `id`,
/// `parent_id`, and the kind discriminant per event, skipping all
/// `ContentBlock` deserialization. This is much cheaper than [`load`] for
/// tree-structure purposes (the `/tree` picker only needs the shape, not
/// message content).
///
/// # Errors
///
/// Returns the underlying IO error if the session file cannot be read or
/// an event line cannot be parsed.
pub fn load_index(path: &Path) -> Result<(SessionMeta, Vec<EventIndex>, u64)> {
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
    let header: Header = serde_json::from_str(&header_line)
        .map_err(|e| Error::State(format!("json: {e}")))?;
    if !(SESSION_MIN_VERSION..=SESSION_VERSION).contains(&header.meta.version) {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let legacy_v1 = header.meta.version == 1;
    let mut prev_id: Option<String> = None;
    let mut indices = Vec::new();
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
        let skel: EventSkeleton = serde_json::from_str(line)
            .map_err(|e| Error::State(format!("parse index event in {}: {e}", path.display())))?;
        let mut id = skel.id;
        let mut parent_id = skel.parent_id;
        if legacy_v1 || id.is_empty() {
            id = short_id();
        }
        if legacy_v1 || parent_id.is_none() {
            parent_id.clone_from(&prev_id);
        }
        let kind = match skel.kind_type.as_str() {
            "message" => match skel.role.as_deref() {
                Some("user") => IndexKind::UserPrompt,
                Some("assistant") => IndexKind::AssistantMessage,
                _ => IndexKind::Other,
            },
            "turn_end" => IndexKind::TurnEnd,
            "turn_failed" => IndexKind::TurnFailed,
            _ => IndexKind::Other,
        };
        prev_id = Some(id.clone());
        indices.push(EventIndex { id, parent_id, offset: line_start, kind });
    }
    Ok((header.meta, indices, pos))
}

/// Parse a single event at a known byte offset. Used for lazy label loading
/// after [`load_index`] has built the tree structure.
///
/// # Errors
///
/// Returns the underlying IO error if the session file cannot be read or the
/// event at `offset` cannot be parsed.
pub fn load_event_at(path: &Path, offset: u64) -> Result<SessionEvent> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    reader.seek(SeekFrom::Start(offset))?;
    let mut buf = String::new();
    reader.read_line(&mut buf)?;
    parse_event(buf.trim_end_matches(['\n', '\r']))
}
