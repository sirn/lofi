//! Lightweight session-file index: scan an event log for just the tree
//! shape (ids, parent ids, kinds, byte offsets) without deserializing
//! message content, and random-access a single event by offset.
//!
//! Used by the `/tree` picker to avoid a full [`super::load`].

use std::path::Path;

use lofi_error::{Error, Result};
use lofi_types::{SessionEvent, SessionEventKind};
use serde::Deserialize;

use super::{Header, SessionMeta, SESSION_MIN_VERSION, SESSION_VERSION};
/// Compact event identifier used by the file index. Current transcript IDs are
/// 32 hexadecimal UUID digits, so keeping them as inline integers avoids two
/// heap allocations per event (id + parent id). Legacy and unusual external
/// IDs remain supported without changing the on-disk format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct IndexId(IndexIdRepr);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum IndexIdRepr {
    Empty,
    Uuid(u128),
    Legacy(u64),
    Other(Box<str>),
}

impl IndexId {
    pub fn parse(value: String) -> Self {
        if value.is_empty() {
            return Self(IndexIdRepr::Empty);
        }
        if let Some(hex) = value.strip_prefix("legacy-") {
            if let Ok(offset) = u64::from_str_radix(hex, 16) {
                return Self(IndexIdRepr::Legacy(offset));
            }
        }
        if value.len() == 32 {
            if let Ok(id) = u128::from_str_radix(&value, 16) {
                return Self(IndexIdRepr::Uuid(id));
            }
        }
        Self(IndexIdRepr::Other(value.into_boxed_str()))
    }

    fn legacy(offset: u64) -> Self {
        Self(IndexIdRepr::Legacy(offset))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.0, IndexIdRepr::Empty)
    }

    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match &self.0 {
            IndexIdRepr::Empty => value.is_empty(),
            IndexIdRepr::Uuid(id) => {
                value.len() == 32 && u128::from_str_radix(value, 16).is_ok_and(|value| value == *id)
            }
            IndexIdRepr::Legacy(offset) => {
                value
                    .strip_prefix("legacy-")
                    .and_then(|hex| u64::from_str_radix(hex, 16).ok())
                    == Some(*offset)
            }
            IndexIdRepr::Other(id) => id.as_ref() == value,
        }
    }

    #[must_use]
    pub fn to_event_id(&self) -> String {
        match &self.0 {
            IndexIdRepr::Empty => String::new(),
            IndexIdRepr::Uuid(id) => format!("{id:032x}"),
            IndexIdRepr::Legacy(offset) => format!("legacy-{offset:016x}"),
            IndexIdRepr::Other(id) => id.to_string(),
        }
    }
}

/// Lightweight per-event index entry: just enough to build the event tree
/// structure (id, `parent_id`, offset) and identify tree-node kinds, without
/// deserializing message content. Used by `/tree` to avoid a full `load`.
#[derive(Debug, Clone)]
pub struct EventIndex {
    pub id: IndexId,
    pub parent_id: Option<IndexId>,
    /// Byte offset of this event's line in the file — for random-access
    /// label loading via [`load_event_at`].
    pub offset: u64,
    /// Byte offset immediately after this event line. Keeping the exact line
    /// end lets file-backed branch replay stop at the selected lineage event
    /// instead of reading later sibling branches from the append-only file.
    pub end_offset: u64,
    pub kind: IndexKind,
    /// Selected head carried only by cursor records.
    pub cursor_leaf: Option<IndexId>,
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
    /// Durable logical-head metadata. Not a conversation-tree node.
    Cursor,
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
    #[serde(default)]
    leaf_id: Option<String>,
}

/// Deserialize one JSONL value directly from a buffered stream. Unknown fields
/// are skipped by serde while they are read, so indexing a record with a huge
/// tool-result body does not first copy that complete record into a String.
fn read_jsonl_value<T, R>(reader: &mut std::io::BufReader<R>) -> Result<Option<(u64, u64, T)>>
where
    T: serde::de::DeserializeOwned,
    R: std::io::Read + std::io::Seek,
{
    use std::io::{BufRead, Seek};

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        let whitespace = available
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if whitespace == 0 {
            break;
        }
        reader.consume(whitespace);
    }
    let start = reader.stream_position()?;
    let value = serde_json::Deserializer::from_reader(&mut *reader)
        .into_iter::<T>()
        .next()
        .transpose()
        .map_err(|error| Error::State(format!("json: {error}")))?
        .ok_or_else(|| Error::State("missing JSON value".to_string()))?;

    // JSONL records may contain only whitespace after a value. Consume the
    // separator so the exact end offset includes its trailing newline.
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let consumed = available
            .iter()
            .take_while(|byte| byte.is_ascii_whitespace())
            .count();
        if consumed == 0 {
            break;
        }
        reader.consume(consumed);
    }
    let end = reader.stream_position()?;
    Ok(Some((start, end, value)))
}

/// Deserialize a current event directly from the stream, falling back to the
/// pre-event-log bare Message shape only when the tagged form fails. Current
/// transcripts stay single-pass and never retain a complete raw JSON line.
fn read_session_event<R>(
    reader: &mut std::io::BufReader<R>,
) -> Result<Option<(u64, u64, SessionEvent)>>
where
    R: std::io::Read + std::io::Seek,
{
    use std::io::{Seek, SeekFrom};

    let start = reader.stream_position()?;
    match read_jsonl_value::<SessionEvent, _>(reader) {
        Ok(event) => Ok(event),
        Err(tagged_error) => {
            reader.seek(SeekFrom::Start(start))?;
            read_jsonl_value::<lofi_types::Message, _>(reader)
                .map(|legacy| {
                    legacy.map(|(start, end, message)| {
                        (
                            start,
                            end,
                            SessionEvent {
                                id: String::new(),
                                parent_id: None,
                                kind: SessionEventKind::Message(message),
                            },
                        )
                    })
                })
                .map_err(|_| tagged_error)
        }
    }
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
fn index_kind(kind_type: &str, role: Option<&str>) -> IndexKind {
    match kind_type {
        "message" | "" => match role {
            Some("user") => IndexKind::UserPrompt,
            Some("assistant") => IndexKind::AssistantMessage,
            Some("tool") => IndexKind::ToolResult,
            Some("system") => IndexKind::SystemMessage,
            _ => IndexKind::Other,
        },
        "turn_end" => IndexKind::TurnEnd,
        "turn_failed" => IndexKind::TurnFailed,
        "compaction" => IndexKind::Compaction,
        "native_tool" => IndexKind::NativeTool,
        "cursor" => IndexKind::Cursor,
        _ => IndexKind::Other,
    }
}

pub(super) fn load_index(path: &Path) -> Result<(SessionMeta, Vec<EventIndex>, u64)> {
    use std::io::{BufReader, Seek};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let Some((_header_start, _header_end, header)) = read_jsonl_value::<Header, _>(&mut reader)?
    else {
        return Err(Error::State(format!(
            "session file has no header: {}",
            path.display()
        )));
    };
    if !(SESSION_MIN_VERSION..=SESSION_VERSION).contains(&header.meta.version) {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let legacy_v1 = header.meta.version == 1;
    let mut prev_id: Option<IndexId> = None;
    let mut indices = Vec::new();
    // Cursor records are append-only head metadata. Only the latest one can
    // affect a read; retaining one per committed batch would make index memory
    // grow with writes rather than conversation events.
    let mut latest_cursor = None;
    while let Some((line_start, line_end, skel)) = read_jsonl_value::<EventSkeleton, _>(&mut reader)
        .map_err(|error| {
            Error::State(format!("parse index event in {}: {error}", path.display()))
        })?
    {
        let is_cursor = skel.kind_type == "cursor";
        let migrated = !is_cursor && (legacy_v1 || skel.id.is_empty());
        let (id, parent_id) = if migrated {
            (IndexId::legacy(line_start), prev_id.clone())
        } else {
            (IndexId::parse(skel.id), skel.parent_id.map(IndexId::parse))
        };
        let kind = index_kind(&skel.kind_type, skel.role.as_deref());
        let entry = EventIndex {
            id,
            parent_id,
            offset: line_start,
            end_offset: line_end,
            kind,
            cursor_leaf: skel.leaf_id.map(IndexId::parse),
        };
        if kind == IndexKind::Cursor {
            latest_cursor = Some(entry);
        } else {
            prev_id = Some(entry.id.clone());
            indices.push(entry);
        }
    }
    if let Some(cursor) = latest_cursor {
        indices.push(cursor);
    }
    let pos = reader.stream_position()?;
    Ok((header.meta, indices, pos))
}

/// Index only a known append range. Active cursors use this after each durable
/// write, so compaction can retain a small suffix index instead of rebuilding
/// an index for the complete append-only transcript.
pub(super) fn load_index_range(path: &Path, start: u64, end: u64) -> Result<Vec<EventIndex>> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    reader.seek(SeekFrom::Start(start))?;
    let mut indices = Vec::new();
    while reader.stream_position()? < end {
        let Some((line_start, line_end, skel)) = read_jsonl_value::<EventSkeleton, _>(&mut reader)?
        else {
            break;
        };
        if line_end > end {
            return Err(Error::State(format!(
                "index event extends beyond committed range {start}..{end} in {}",
                path.display()
            )));
        }
        if skel.kind_type == "cursor" {
            continue;
        }
        indices.push(EventIndex {
            id: IndexId::parse(skel.id),
            parent_id: skel.parent_id.map(IndexId::parse),
            offset: line_start,
            end_offset: line_end,
            kind: index_kind(&skel.kind_type, skel.role.as_deref()),
            cursor_leaf: None,
        });
    }
    Ok(indices)
}

/// Parse a single event at a known byte offset. Used for lazy label loading
/// after [`load_index`] has built the tree structure.
///
/// # Errors
///
/// Returns the underlying IO error if the session file cannot be read or the
/// event at `offset` cannot be parsed.
pub(super) fn load_event_at(path: &Path, offset: u64) -> Result<SessionEvent> {
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
pub(super) fn load_event_by_id(path: &Path, id: &str) -> Result<Option<SessionEvent>> {
    let (_meta, index, _size) = load_index(path)?;
    let Some(entry) = index.iter().find(|entry| entry.id.matches(id)) else {
        return Ok(None);
    };
    let mut event = load_event_at(path, entry.offset)?;
    event.id = entry.id.to_event_id();
    event.parent_id = entry.parent_id.as_ref().map(IndexId::to_event_id);
    Ok(Some(event))
}

/// Deserialize a small caller-defined projection at selected event offsets.
/// Unknown JSON fields are skipped directly from the buffered file stream, so
/// a projection does not allocate a complete backing line merely because an
/// unrelated field (such as a native-tool result) is huge.
///
/// # Errors
/// Returns an error when the transcript/offset cannot be read, projected JSON
/// is invalid, or the visitor rejects a value.
pub(super) fn visit_event_values<T: serde::de::DeserializeOwned>(
    path: &Path,
    offsets: &[u64],
    mut visit: impl FnMut(T) -> Result<()>,
) -> Result<()> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    for &offset in offsets {
        reader.seek(SeekFrom::Start(offset))?;
        let Some((_start, _end, value)) = read_jsonl_value::<T, _>(&mut reader)? else {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        };
        visit(value)?;
    }
    Ok(())
}

/// Visit selected complete events one at a time without retaining their raw
/// JSON lines or collecting the complete selected set.
pub(super) fn visit_events(
    path: &Path,
    offsets: &[u64],
    mut visit: impl FnMut(SessionEvent) -> Result<()>,
) -> Result<()> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    for &offset in offsets {
        reader.seek(SeekFrom::Start(offset))?;
        let Some((_start, _end, event)) = read_session_event(&mut reader)? else {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        };
        if !matches!(event.kind, SessionEventKind::Cursor { .. }) {
            visit(event)?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct CollapsedEventMeta {
    #[serde(default, rename = "type")]
    kind_type: String,
    #[serde(default)]
    role: Option<lofi_types::Role>,
    #[serde(default)]
    parent: String,
    #[serde(default)]
    call_id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: String,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    blocks: Vec<CollapsedBlockMeta>,
    #[serde(default)]
    summarized: usize,
    #[serde(default)]
    represented: usize,
    #[serde(default)]
    kept: usize,
}

#[derive(Deserialize)]
struct CollapsedBlockMeta {
    #[serde(default, rename = "type")]
    kind_type: String,
    #[serde(default)]
    tool_use_id: String,
    #[serde(default)]
    is_error: bool,
}

/// Load selected events for collapsed transcript rendering without allocating
/// payloads that the collapsed renderer never reads. Full errors and visible
/// mutating-tool previews remain lossless; verbose rendering uses the ordinary
/// complete-event loader instead.
pub(super) fn load_collapsed_events_at(path: &Path, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
    use std::collections::HashSet;
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut events = Vec::with_capacity(offsets.len());
    let mut exec_ids = HashSet::new();
    for &offset in offsets {
        reader.seek(SeekFrom::Start(offset))?;
        let Some((_start, _end, meta)) = read_jsonl_value::<CollapsedEventMeta, _>(&mut reader)?
        else {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        };
        let hidden_native = meta.kind_type == "native_tool"
            && !meta.is_error
            && !matches!(meta.name.as_str(), "bash" | "write" | "edit" | "agent");
        let hidden_exec_result = meta.kind_type == "message"
            && matches!(
                meta.role,
                Some(lofi_types::Role::Tool | lofi_types::Role::User)
            )
            && !meta.blocks.is_empty()
            && meta.blocks.iter().all(|block| {
                block.kind_type == "tool_result"
                    && !block.is_error
                    && exec_ids.contains(&block.tool_use_id)
            });
        let event = if hidden_native {
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::NativeTool(lofi_types::NativeToolRecord {
                    parent: meta.parent,
                    call_id: meta.call_id,
                    name: meta.name,
                    args: meta.args,
                    result: String::new(),
                    is_error: false,
                }),
            }
        } else if hidden_exec_result {
            let role = meta.role.unwrap_or(lofi_types::Role::Tool);
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(lofi_types::Message {
                    role,
                    blocks: meta
                        .blocks
                        .into_iter()
                        .map(|block| lofi_types::ContentBlock::ToolResult {
                            tool_use_id: block.tool_use_id,
                            content: String::new(),
                            is_error: false,
                        })
                        .collect(),
                }),
            }
        } else if meta.kind_type == "compaction" {
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Compaction {
                    summary: String::new(),
                    first_kept_entry_id: String::new(),
                    summarized_range: [String::new(), String::new()],
                    checkpointed_tail: false,
                    summarized: meta.summarized,
                    represented: meta.represented,
                    kept: meta.kept,
                },
            }
        } else {
            reader.seek(SeekFrom::Start(offset))?;
            let Some((_start, _end, event)) = read_session_event(&mut reader)? else {
                return Err(Error::State(format!(
                    "no event at offset {offset} in {}",
                    path.display()
                )));
            };
            event
        };
        if let SessionEventKind::Message(message) = &event.kind {
            if message.role == lofi_types::Role::Assistant {
                exec_ids.extend(message.blocks.iter().filter_map(|block| match block {
                    lofi_types::ContentBlock::ToolUse { id, name, .. } if name == "exec" => {
                        Some(id.clone())
                    }
                    _ => None,
                }));
            }
        }
        if !matches!(event.kind, SessionEventKind::Cursor { .. }) {
            events.push(event);
        }
    }
    Ok(events)
}

/// Parse selected event values in one file pass. Offsets must be in ascending
/// order. Values are deserialized directly from the file, so peak memory is
/// the returned event payload rather than payload plus a complete JSON line.
///
/// # Errors
/// Returns an error when the transcript/offset cannot be read or a selected
/// event cannot be parsed.
pub(super) fn load_events_at(path: &Path, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
    let mut out = Vec::with_capacity(offsets.len());
    visit_events(path, offsets, |event| {
        out.push(event);
        Ok(())
    })?;
    Ok(out)
}

/// Parse every non-cursor event whose line starts inside `[start, end)`.
/// Used for lazy materialization of a committed turn range.
pub(super) fn load_event_range(path: &Path, start: u64, end: u64) -> Result<Vec<SessionEvent>> {
    use std::io::{Seek, SeekFrom};

    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    reader.seek(SeekFrom::Start(start))?;
    let mut out = Vec::new();
    while reader.stream_position()? < end {
        let Some((_event_start, event_end, event)) = read_session_event(&mut reader)? else {
            break;
        };
        if event_end > end {
            return Err(Error::State(format!(
                "event extends beyond committed range {start}..{end} in {}",
                path.display()
            )));
        }
        if !matches!(event.kind, SessionEventKind::Cursor { .. }) {
            out.push(event);
        }
    }
    Ok(out)
}

/// Retain only the selected-lineage suffix required by the next compaction.
/// The input must already be projected root-to-leaf (as `SessionSnapshot` is).
pub(super) fn compaction_index_suffix(
    path: &Path,
    lineage: &[EventIndex],
) -> Result<Vec<EventIndex>> {
    let Some(marker_pos) = lineage
        .iter()
        .rposition(|event| event.kind == IndexKind::Compaction)
    else {
        return Ok(lineage.to_vec());
    };
    let marker = load_event_at(path, lineage[marker_pos].offset)?;
    let start = match marker.kind {
        SessionEventKind::Compaction {
            first_kept_entry_id,
            ..
        } if !first_kept_entry_id.is_empty() => lineage[..marker_pos]
            .iter()
            .position(|event| event.id.matches(&first_kept_entry_id))
            .unwrap_or(marker_pos),
        _ => marker_pos,
    };
    Ok(lineage[start..].to_vec())
}

/// Materialize only the lineage suffix needed by compaction. Once a
/// compaction marker exists, everything before its checkpointed kept tail is
/// represented by the marker summary and must not be deserialized again.
/// A `None` leaf is the explicit root cursor and therefore yields no events.
///
/// # Errors
/// Returns an error when the requested leaf is absent, the lineage is cyclic,
/// or an indexed event cannot be read or parsed.
pub(super) fn load_compaction_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    use std::collections::HashMap;

    if index.is_empty() {
        return Ok(Vec::new());
    }
    let by_id: HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .map(|(i, event)| (&event.id, i))
        .collect();
    let leaf = leaf_id.map(|id| IndexId::parse(id.to_string()));
    let mut current = leaf.as_ref().and_then(|id| by_id.get(id).copied());
    let mut lineage = Vec::new();
    while let Some(i) = current {
        lineage.push(i);
        if lineage.len() > index.len() {
            return Err(Error::State("cycle in session event lineage".to_string()));
        }
        current = index[i]
            .parent_id
            .as_ref()
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
                    .position(|&i| index[i].id.matches(&first_kept_entry_id))
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
pub(super) fn load_indexed_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    use std::collections::HashMap;

    if index.is_empty() {
        return Ok(Vec::new());
    }
    let by_id: HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .map(|(i, event)| (&event.id, i))
        .collect();
    let leaf = leaf_id.map(|id| IndexId::parse(id.to_string()));
    let mut current = leaf.as_ref().and_then(|id| by_id.get(id).copied());
    let mut lineage = Vec::new();
    while let Some(i) = current {
        lineage.push(i);
        if lineage.len() > index.len() {
            return Err(Error::State("cycle in session event lineage".to_string()));
        }
        current = index[i]
            .parent_id
            .as_ref()
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
        event.id = index[i].id.to_event_id();
        event.parent_id = index[i].parent_id.as_ref().map(IndexId::to_event_id);
    }
    Ok(events)
}
