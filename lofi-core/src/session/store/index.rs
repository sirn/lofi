//! Lightweight session-file index: scan an event log for just the tree
//! shape (ids, parent ids, kinds, byte offsets) without deserializing
//! message content, and random-access a single event by offset.
//!
//! Used by the `/tree` picker to avoid a full [`super::load`].

use std::path::Path;

use lofi_error::{Error, Result};
use lofi_types::SessionEvent;
use serde::Deserialize;

use super::{
    legacy_event_id, parse_event, Header, SessionMeta, SESSION_MIN_VERSION, SESSION_VERSION,
};
/// Lightweight per-event index entry: just enough to build the event tree
/// structure (id, `parent_id`, offset) and identify tree-node kinds, without
/// deserializing message content. Used by `/tree` to avoid a full `load`.
#[derive(Debug, Clone)]
pub struct EventIndex {
    pub id: String,
    pub parent_id: Option<String>,
    /// Byte offset of this event's line in the file — for random-access
    /// label loading via [`load_event_at`].
    pub offset: u64,
    /// Byte offset immediately after this event line. Keeping the exact line
    /// end lets file-backed branch replay stop at the selected lineage event
    /// instead of reading later sibling branches from the append-only file.
    pub end_offset: u64,
    pub kind: IndexKind,
}

/// The kind discriminant extracted by the lightweight scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    UserPrompt,
    AssistantMessage,
    SystemMessage,
    /// A tool-result message (`role: tool`). Distinguished from `UserPrompt`
    /// so the tree can show it as a `tool:` node and `find_turn_outcome` can
    /// follow it (it was previously `Other`, which caused `find_turn_outcome`
    /// to miss turn outcomes for turns with tool calls — the function follows
    /// non-`UserPrompt` children, but `Other` events were not tree nodes so the
    /// outcome was never displayed).
    ToolResult,
    /// A native tool call inside an `exec` block (e.g. `lofi.read`).
    /// Not a tree node — used to build the exec label's native-tool
    /// summary in `/tree`.
    NativeTool,
    TurnEnd,
    TurnFailed,
    /// An offline compaction marker. A tree node so `/tree` can revert to
    /// the pre-compaction state (selecting it rolls back to its parent).
    Compaction,
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
    let header: Header =
        serde_json::from_str(&header_line).map_err(|e| Error::State(format!("json: {e}")))?;
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
        let migrated = legacy_v1 || id.is_empty();
        if migrated {
            id = legacy_event_id(line_start);
            parent_id.clone_from(&prev_id);
        }
        let role_kind = |role: Option<&str>| -> IndexKind {
            match role {
                Some("user") => IndexKind::UserPrompt,
                Some("assistant") => IndexKind::AssistantMessage,
                Some("tool") => IndexKind::ToolResult,
                Some("system") => IndexKind::SystemMessage,
                _ => IndexKind::Other,
            }
        };
        let kind = match skel.kind_type.as_str() {
            "message" | "" => role_kind(skel.role.as_deref()),
            "turn_end" => IndexKind::TurnEnd,
            "turn_failed" => IndexKind::TurnFailed,
            "compaction" => IndexKind::Compaction,
            "native_tool" => IndexKind::NativeTool,
            _ => IndexKind::Other,
        };
        prev_id = Some(id.clone());
        indices.push(EventIndex {
            id,
            parent_id,
            offset: line_start,
            end_offset: pos,
            kind,
        });
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
    let mut events = load_events_at(path, &[offset])?;
    events
        .pop()
        .ok_or_else(|| Error::State(format!("no event at offset {offset} in {}", path.display())))
}

/// Find and parse one event by id without deserializing unrelated message
/// bodies. Resolution goes through the lightweight index so deterministic
/// IDs synthesized for legacy records work exactly like persisted v2 IDs.
///
/// # Errors
/// Returns an error when the transcript cannot be indexed or the selected
/// event cannot be read or parsed.
pub fn load_event_by_id(path: &Path, id: &str) -> Result<Option<SessionEvent>> {
    let (_meta, index, _size) = load_index(path)?;
    let Some(entry) = index.iter().find(|entry| entry.id == id) else {
        return Ok(None);
    };
    let mut event = load_event_at(path, entry.offset)?;
    event.id.clone_from(&entry.id);
    event.parent_id.clone_from(&entry.parent_id);
    Ok(Some(event))
}

/// Visit selected raw event lines while retaining at most one line at a time.
/// Offsets must be in ascending order. This is used by transcript-wide tools
/// that need multiple streaming passes without materializing every event.
///
/// # Errors
/// Returns an error when the transcript or an offset cannot be read, or when
/// the visitor rejects a selected line.
pub fn visit_event_lines(
    path: &Path,
    offsets: &[u64],
    mut visit: impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut buf = String::new();
    for &offset in offsets {
        reader.seek(SeekFrom::Start(offset))?;
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        }
        visit(buf.trim_end_matches(['\n', '\r']))?;
    }
    Ok(())
}

/// Parse selected event lines in one file pass. Offsets must be in ascending
/// order. This is used by resume to materialize only the active branch, one
/// turn at a time, without ever constructing the full transcript in memory.
///
/// # Errors
/// Returns an error when the transcript or an offset cannot be read, or when
/// a selected event cannot be parsed.
pub fn load_events_at(path: &Path, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
    use std::io::{BufRead, Seek, SeekFrom};
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut out = Vec::with_capacity(offsets.len());
    let mut buf = String::new();
    for &offset in offsets {
        reader.seek(SeekFrom::Start(offset))?;
        buf.clear();
        if reader.read_line(&mut buf)? == 0 {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        }
        out.push(parse_event(buf.trim_end_matches(['\n', '\r']))?);
    }
    Ok(out)
}

/// Materialize only the lineage suffix needed by compaction. Once a
/// compaction marker exists, everything before its checkpointed kept tail is
/// represented by the marker summary and must not be deserialized again.
/// A `None` leaf is the explicit root cursor and therefore yields no events.
///
/// # Errors
/// Returns an error when the requested leaf is absent, the lineage is cyclic,
/// or an indexed event cannot be read or parsed.
pub fn load_compaction_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    use std::collections::HashMap;

    if index.is_empty() {
        return Ok(Vec::new());
    }
    let by_id: HashMap<&str, usize> = index
        .iter()
        .enumerate()
        .map(|(i, event)| (event.id.as_str(), i))
        .collect();
    let mut current = leaf_id.and_then(|id| by_id.get(id).copied());
    let mut lineage = Vec::new();
    while let Some(i) = current {
        lineage.push(i);
        if lineage.len() > index.len() {
            return Err(Error::State("cycle in session event lineage".to_string()));
        }
        current = index[i]
            .parent_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
    }
    if leaf_id.is_some() && lineage.is_empty() {
        return Err(Error::State("session branch leaf not found".to_string()));
    }
    lineage.reverse();

    let mut start = 0;
    if let Some((marker_pos, &marker_index)) = lineage
        .iter()
        .enumerate()
        .rev()
        .find(|(_, i)| index[**i].kind == IndexKind::Compaction)
    {
        let marker = load_event_at(path, index[marker_index].offset)?;
        if let super::SessionEventKind::Compaction {
            first_kept_entry_id,
            ..
        } = marker.kind
        {
            // Include the marker in both cases: compact() needs its previous
            // summary. For a non-empty checkpoint include the kept messages
            // immediately before it as well.
            start = if first_kept_entry_id.is_empty() {
                marker_pos
            } else {
                lineage[..marker_pos]
                    .iter()
                    .position(|&i| index[i].id == first_kept_entry_id)
                    .unwrap_or(marker_pos)
            };
        }
    }
    load_index_entries(path, index, &lineage[start..])
}

/// Materialize only one indexed lineage from a session file. The lightweight
/// index owns the tree shape; large event bodies are parsed only for nodes on
/// the selected path, avoiding a full transcript-sized allocation. A `None`
/// leaf is the explicit root cursor and therefore yields no events.
///
/// # Errors
/// Returns an error when the requested leaf is absent, the lineage is cyclic,
/// or an indexed event cannot be read or parsed.
pub fn load_indexed_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    use std::collections::HashMap;

    if index.is_empty() {
        return Ok(Vec::new());
    }
    let by_id: HashMap<&str, usize> = index
        .iter()
        .enumerate()
        .map(|(i, event)| (event.id.as_str(), i))
        .collect();
    let mut current = leaf_id.and_then(|id| by_id.get(id).copied());
    let mut lineage = Vec::new();
    while let Some(i) = current {
        lineage.push(i);
        if lineage.len() > index.len() {
            return Err(Error::State("cycle in session event lineage".to_string()));
        }
        current = index[i]
            .parent_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
    }
    if leaf_id.is_some() && lineage.is_empty() {
        return Err(Error::State("session branch leaf not found".to_string()));
    }
    lineage.reverse();
    load_index_entries(path, index, &lineage)
}

fn load_index_entries(
    path: &Path,
    index: &[EventIndex],
    selected: &[usize],
) -> Result<Vec<SessionEvent>> {
    let offsets: Vec<u64> = selected.iter().map(|&i| index[i].offset).collect();
    let mut events = load_events_at(path, &offsets)?;
    for (event, &i) in events.iter_mut().zip(selected) {
        event.id.clone_from(&index[i].id);
        event.parent_id.clone_from(&index[i].parent_id);
    }
    Ok(events)
}
