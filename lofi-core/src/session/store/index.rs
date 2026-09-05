//! Used by the `/tree` picker to avoid a full [`super::load`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::index_parser::{read_index_event, read_projected_value, ProjectionBudget};
use super::{Header, SessionMeta, SESSION_VERSION};
use lofi_error::{Error, Result};
use lofi_types::{SessionEvent, SessionEventKind};
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct IndexId(IndexIdRepr);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
enum IndexIdRepr {
    #[default]
    Empty,
    Uuid([u8; 16]),
    Other(Box<str>),
}

impl IndexId {
    /// Parse from an owned string, keeping the allocation for heap ids.
    #[must_use]
    pub fn parse(value: String) -> Self {
        if value.is_empty() {
            return Self(IndexIdRepr::Empty);
        }
        if value.len() == 32 {
            if let Ok(id) = u128::from_str_radix(&value, 16) {
                return Self(IndexIdRepr::Uuid(id.to_be_bytes()));
            }
        }
        Self(IndexIdRepr::Other(value.into_boxed_str()))
    }

    /// Parse from a borrowed str. UUID ids become `u128` with no allocation;
    /// heap ids allocate one boxed str. This is the scan path's zero-copy
    /// constructor: callers feed it the `&str` out of the line buffer.
    #[must_use]
    pub fn borrow(value: &str) -> Self {
        if value.is_empty() {
            return Self(IndexIdRepr::Empty);
        }
        if value.len() == 32 {
            if let Ok(id) = u128::from_str_radix(value, 16) {
                return Self(IndexIdRepr::Uuid(id.to_be_bytes()));
            }
        }
        Self(IndexIdRepr::Other(value.into()))
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
                value.len() == 32
                    && u128::from_str_radix(value, 16).is_ok_and(|value| value.to_be_bytes() == *id)
            }
            IndexIdRepr::Other(id) => id.as_ref() == value,
        }
    }

    #[must_use]
    pub fn to_event_id(&self) -> String {
        match &self.0 {
            IndexIdRepr::Empty => String::new(),
            IndexIdRepr::Uuid(id) => format!("{:032x}", u128::from_be_bytes(*id)),
            IndexIdRepr::Other(id) => id.to_string(),
        }
    }
}

/// Lightweight per-event index entry: just enough to build the event tree
/// structure (id, `parent_id`, offset) and identify tree-node kinds, without
/// deserializing message content. Used by `/tree` to avoid a full `load`.
///
/// The struct is kept small deliberately: a full-tree index holds one entry
/// per transcript event (tens of thousands on long sessions) and both `/tree`
/// and resume materialize lineage copies, so per-entry width directly scales
/// resident memory. A cursor record carries its selected leaf in the `id`
/// slot (cursor records have no event id of their own) rather than a separate
/// mostly-None `cursor_leaf: Option<IndexId>` field.
#[derive(Debug, Clone)]
pub struct EventIndex {
    pub id: IndexId,
    pub parent_id: Option<IndexId>,
    pub offset: u64,
    /// Byte offset immediately after this event line. Keeping the exact line
    /// end lets file-backed branch replay stop at the selected lineage event
    /// instead of reading later sibling branches from the append-only file.
    pub end_offset: u64,
    pub kind: IndexKind,
}

/// Selected-lineage metadata used by normal resume and branch restore.
/// Graph IDs are needed only while resolving a lineage. Keeping them out of
/// the durable-view projection makes resume memory scale with three words per
/// event instead of two IDs plus offsets.
#[derive(Debug, Clone, Copy)]
pub struct SessionIndexEntry {
    pub offset: u64,
    pub end_offset: u64,
    pub kind: IndexKind,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    UserPrompt,
    UserShell,
    AssistantMessage,
    SystemMessage,
    /// A tool-result message (`role: tool`). Distinguished from `UserPrompt`
    /// so the tree can show it as a `tool:` node and `find_turn_outcome` can
    /// follow it — the function follows non-`UserPrompt` children, so a
    /// tool-result turn's outcome is only reachable when this is a tree node.
    ToolResult,
    NativeTool,
    TurnEnd,
    TurnFailed,
    TurnCancelled,
    Compaction,
    Cursor,
    /// `JobStarted` / `JobFinished` markers; lets reconciliation skip the
    /// full event slice.
    JobLifecycle,
    Other,
}

struct IndexedSkeleton {
    id: IndexId,
    parent_id: Option<IndexId>,
    kind: IndexKind,
    cursor_leaf: Option<IndexId>,
    checkpointed_tail: bool,
    first_kept_entry_id: IndexId,
}

fn read_index_skeleton<R>(
    reader: &mut std::io::BufReader<R>,
    line: &mut Vec<u8>,
) -> Result<Option<(u64, u64, IndexedSkeleton)>>
where
    R: std::io::Read + std::io::Seek,
{
    read_index_event(reader, line, |start, end, event| {
        let kind = index_kind(event.kind_type, event.role);
        (
            start,
            end,
            IndexedSkeleton {
                id: IndexId::borrow(event.id),
                parent_id: event.parent_id.map(IndexId::borrow),
                kind,
                cursor_leaf: event
                    .leaf_id
                    .filter(|id| !id.is_empty())
                    .map(IndexId::borrow),
                checkpointed_tail: event.checkpointed_tail,
                first_kept_entry_id: IndexId::borrow(event.first_kept_entry_id),
            },
        )
    })
}

pub(super) fn read_jsonl_value<T, R>(
    reader: &mut std::io::BufReader<R>,
) -> Result<Option<(u64, u64, T)>>
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

fn read_session_event<R>(
    reader: &mut std::io::BufReader<R>,
) -> Result<Option<(u64, u64, SessionEvent)>>
where
    R: std::io::Read + std::io::Seek,
{
    // Seek is retained in the bound because callers operate on seekable
    // files; the read itself only advances the cursor.
    read_jsonl_value::<SessionEvent, _>(reader)
}

fn count_newlines(reader: &mut impl std::io::Read) -> Result<usize> {
    let mut count = 0usize;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            return Ok(count);
        }
        count = count.saturating_add(memchr::memchr_iter(b'\n', &buf[..read]).count());
    }
}

fn malformed_record_is_final<R>(reader: &mut std::io::BufReader<R>, start: u64) -> Result<bool>
where
    R: std::io::Read + std::io::Seek,
{
    use std::io::{Read, Seek};

    reader.seek(std::io::SeekFrom::Start(start))?;
    let mut after_line = false;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            return Ok(true);
        }
        for &byte in &buf[..read] {
            if after_line && !byte.is_ascii_whitespace() {
                return Ok(false);
            }
            after_line |= byte == b'\n';
        }
    }
}

pub(super) struct LinearSessionIndex {
    pub meta: SessionMeta,
    pub entries: Vec<SessionIndexEntry>,
    pub file_size: u64,
    pub leaf_id: Option<String>,
    pub history_start: usize,
    pub compaction_suffix: Vec<EventIndex>,
}

struct LinearScan {
    count: usize,
    leaf: Option<IndexId>,
    boundaries: HashSet<IndexId>,
    latest_compaction_start: Option<IndexId>,
    latest_compaction_marker: Option<IndexId>,
    file_size: u64,
}

fn scan_linear_session(
    reader: &mut std::io::BufReader<std::fs::File>,
    path: &Path,
    event_start: u64,
) -> Result<Option<LinearScan>> {
    use std::io::Seek;

    let mut count = 0usize;
    let mut previous: Option<IndexId> = None;
    let mut latest_cursor: Option<Option<IndexId>> = None;
    let mut boundaries = HashSet::new();
    let mut latest_compaction_start: Option<IndexId> = None;
    let mut latest_compaction_marker: Option<IndexId> = None;
    let mut linear = true;
    let mut file_size = event_start;
    let mut line = Vec::with_capacity(4 * 1024);
    loop {
        let record_start = reader.stream_position()?;
        let event = read_index_skeleton(reader, &mut line);
        let Some((_line_start, line_end, skel)) = (match event {
            Ok(event) => event,
            Err(error) => {
                if malformed_record_is_final(reader, record_start)? {
                    break;
                }
                return Err(Error::State(format!(
                    "parse index event in {}: {error}",
                    path.display()
                )));
            }
        }) else {
            break;
        };
        file_size = line_end;
        let kind = skel.kind;
        if kind == IndexKind::Cursor {
            latest_cursor = Some(skel.cursor_leaf);
            continue;
        }
        let id = skel.id;
        let parent = skel.parent_id;
        linear &= match (&parent, &previous) {
            (None, None) => true,
            (Some(parent), Some(previous)) => parent == previous,
            _ => false,
        };
        if kind == IndexKind::Compaction {
            let start = if skel.first_kept_entry_id.is_empty() {
                id.clone()
            } else {
                boundaries.insert(skel.first_kept_entry_id.clone());
                skel.first_kept_entry_id
            };
            latest_compaction_start = Some(start);
            latest_compaction_marker = Some(id.clone());
        }
        previous = Some(id);
        count = count.saturating_add(1);
    }

    let leaf = match latest_cursor {
        Some(None) => None,
        Some(Some(leaf)) if linear && previous.as_ref() == Some(&leaf) => Some(leaf),
        None if linear => previous,
        Some(Some(_)) | None => return Ok(None),
    };
    Ok(Some(LinearScan {
        count,
        leaf,
        boundaries,
        latest_compaction_start,
        latest_compaction_marker,
        file_size,
    }))
}

fn project_linear_session(
    reader: &mut std::io::BufReader<std::fs::File>,
    event_start: u64,
    scan: LinearScan,
) -> Result<(Vec<SessionIndexEntry>, usize, Vec<EventIndex>)> {
    use std::io::{Seek, SeekFrom};

    reader.seek(SeekFrom::Start(event_start))?;
    let mut entries = Vec::with_capacity(scan.count);
    let mut boundary_positions: HashMap<IndexId, usize> = scan
        .boundaries
        .into_iter()
        .map(|id| (id, usize::MAX))
        .collect();
    let mut history_start = 0usize;
    let mut compaction_suffix = Vec::new();
    let mut retain_compaction_suffix = scan.latest_compaction_start.is_none();
    let mut line = Vec::with_capacity(4 * 1024);
    while reader.stream_position()? < scan.file_size {
        let Some((line_start, line_end, skel)) = read_index_skeleton(reader, &mut line)? else {
            break;
        };
        let kind = skel.kind;
        if kind == IndexKind::Cursor {
            continue;
        }
        let first_kept_entry_id = skel.first_kept_entry_id;
        let id = skel.id;
        let parent_id = skel.parent_id;
        let position = entries.len();
        if let Some(found) = boundary_positions.get_mut(&id) {
            if *found == usize::MAX {
                *found = position;
            }
        }
        if scan.latest_compaction_start.as_ref() == Some(&id)
            || scan.latest_compaction_marker.as_ref() == Some(&id)
        {
            retain_compaction_suffix = true;
        }
        if kind == IndexKind::Compaction {
            let start = if first_kept_entry_id.is_empty() {
                position
            } else {
                boundary_positions
                    .get(&first_kept_entry_id)
                    .copied()
                    .filter(|start| *start != usize::MAX && *start < position)
                    .unwrap_or(position)
            };
            history_start = start;
            if skel.checkpointed_tail && start < position {
                entries[start..]
                    .iter_mut()
                    .for_each(|entry: &mut SessionIndexEntry| entry.visible = false);
            }
        }
        entries.push(SessionIndexEntry {
            offset: line_start,
            end_offset: line_end,
            kind,
            visible: true,
        });
        if retain_compaction_suffix {
            compaction_suffix.push(EventIndex {
                id,
                parent_id,
                offset: line_start,
                end_offset: line_end,
                kind,
            });
        }
    }
    Ok((entries, history_start, compaction_suffix))
}

/// Build the normal resume projection without retaining the transcript graph.
/// Returns `None` when the selected lineage is not the physical event prefix;
/// callers then use the general graph loader for branch semantics.
pub(super) fn load_linear_session_index(path: &Path) -> Result<Option<LinearSessionIndex>> {
    use std::io::BufReader;

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let Some((_start, event_start, header)) = read_jsonl_value::<Header, _>(&mut reader)? else {
        return Err(Error::State(format!(
            "session file has no header: {}",
            path.display()
        )));
    };
    if header.meta.version != SESSION_VERSION {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }

    let Some(scan) = scan_linear_session(&mut reader, path, event_start)? else {
        return Ok(None);
    };
    let file_size = scan.file_size;
    let Some(leaf) = scan.leaf.clone() else {
        return Ok(Some(LinearSessionIndex {
            meta: header.meta,
            entries: Vec::new(),
            file_size,
            leaf_id: None,
            history_start: 0,
            compaction_suffix: Vec::new(),
        }));
    };
    let (entries, history_start, compaction_suffix) =
        project_linear_session(&mut reader, event_start, scan)?;

    Ok(Some(LinearSessionIndex {
        meta: header.meta,
        entries,
        file_size,
        leaf_id: Some(leaf.to_event_id()),
        history_start,
        compaction_suffix,
    }))
}

/// # Errors
/// Returns the underlying IO error if the session file cannot be read or
/// an event line cannot be parsed.
pub(super) fn index_kind(kind_type: &str, role: Option<&str>) -> IndexKind {
    match kind_type {
        "message" | "" => match role {
            Some("user") => IndexKind::UserPrompt,
            Some("assistant") => IndexKind::AssistantMessage,
            Some("tool") => IndexKind::ToolResult,
            Some("system") => IndexKind::SystemMessage,
            _ => IndexKind::Other,
        },
        "user_shell" => IndexKind::UserShell,
        "turn_end" => IndexKind::TurnEnd,
        "turn_failed" => IndexKind::TurnFailed,
        "turn_cancelled" => IndexKind::TurnCancelled,
        "compaction" => IndexKind::Compaction,
        "native_tool" => IndexKind::NativeTool,
        "cursor" => IndexKind::Cursor,
        "job_started" | "job_finished" => IndexKind::JobLifecycle,
        _ => IndexKind::Other,
    }
}

/// Classify a materialized (in-memory) event into its [`IndexKind`]. This is
/// the in-memory analogue of [`index_kind`]: the file index walks serialized
/// `type`/`role` strings, while a session-view index derived from memory walks
/// the typed [`SessionEventKind`]. Routing both through one classifier keeps
/// the transcript and memory representations on the same lineage walk.
#[must_use]
pub(super) fn index_kind_for_event(kind: &SessionEventKind) -> IndexKind {
    match kind {
        SessionEventKind::Message(message) => match message.role {
            lofi_types::Role::User => IndexKind::UserPrompt,
            lofi_types::Role::Assistant => IndexKind::AssistantMessage,
            lofi_types::Role::Tool => IndexKind::ToolResult,
            lofi_types::Role::System => IndexKind::SystemMessage,
        },
        SessionEventKind::UserShell { .. } => IndexKind::UserShell,
        SessionEventKind::TurnEnd { .. } => IndexKind::TurnEnd,
        SessionEventKind::TurnFailed { .. } => IndexKind::TurnFailed,
        SessionEventKind::TurnCancelled { .. } => IndexKind::TurnCancelled,
        SessionEventKind::Compaction { .. } => IndexKind::Compaction,
        SessionEventKind::NativeTool(_) => IndexKind::NativeTool,
        SessionEventKind::JobStarted { .. } | SessionEventKind::JobFinished { .. } => {
            IndexKind::JobLifecycle
        }
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
    if header.meta.version != SESSION_VERSION {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    // Count records with a fixed scratch buffer before indexing. File-size
    // estimates over-allocate badly when tool results are large, while normal
    // Vec growth briefly retains both the 131K and 262K entry buffers. An exact
    // capacity keeps peak index memory proportional to the actual event count.
    let event_start = reader.stream_position()?;
    let event_capacity = count_newlines(&mut reader)?;
    reader.seek(std::io::SeekFrom::Start(event_start))?;
    let mut indices = Vec::with_capacity(event_capacity);
    // Cursor records are append-only head metadata. Only the latest one can
    // affect a read; retaining one per committed batch would make index memory
    // grow with writes rather than conversation events.
    let mut latest_cursor = None;
    let mut line = Vec::with_capacity(4 * 1024);
    loop {
        let record_start = reader.stream_position()?;
        let event = read_index_skeleton(&mut reader, &mut line);
        let Some((line_start, line_end, skel)) = (match event {
            Ok(event) => event,
            Err(error) => {
                // Only the final malformed record is ignored. A process can
                // die during a write, so the committed prefix must remain
                // resumable. Earlier malformed records remain errors.
                if malformed_record_is_final(&mut reader, record_start)? {
                    break;
                }
                return Err(Error::State(format!(
                    "parse index event in {}: {error}",
                    path.display()
                )));
            }
        }) else {
            break;
        };
        let parent_id = skel.parent_id;
        let kind = skel.kind;
        // Cursor records have no event id of their own; carry the selected
        // leaf in `id` so downstream readers find it alongside the record.
        let id = if kind == IndexKind::Cursor {
            skel.cursor_leaf.unwrap_or_default()
        } else {
            skel.id
        };
        let entry = EventIndex {
            id,
            parent_id,
            offset: line_start,
            end_offset: line_end,
            kind,
        };
        if kind == IndexKind::Cursor {
            latest_cursor = Some(entry);
        } else {
            indices.push(entry);
        }
    }
    if let Some(cursor) = latest_cursor {
        indices.push(cursor);
    }
    let pos = indices.last().map_or(0, |event| event.end_offset);
    Ok((header.meta, indices, pos))
}

/// Index only a known append range. Active cursors use this after each durable
/// write, so compaction can retain a small suffix index instead of rebuilding
/// an index for the complete append-only transcript.
pub(super) fn session_index_from_lineage(
    path: &Path,
    lineage: &[EventIndex],
) -> Result<(Vec<SessionIndexEntry>, usize)> {
    let mut entries: Vec<SessionIndexEntry> = lineage
        .iter()
        .map(|entry| SessionIndexEntry {
            offset: entry.offset,
            end_offset: entry.end_offset,
            kind: entry.kind,
            visible: true,
        })
        .collect();
    let mut history_start = 0usize;
    for (position, event) in lineage.iter().enumerate() {
        if event.kind != IndexKind::Compaction {
            continue;
        }
        let marker = load_event_at(path, event.offset)?;
        let SessionEventKind::Compaction {
            checkpointed_tail,
            first_kept_entry_id,
            ..
        } = marker.kind
        else {
            continue;
        };
        let start = if first_kept_entry_id.is_empty() {
            position
        } else {
            lineage[..position]
                .iter()
                .position(|entry| entry.id.matches(&first_kept_entry_id))
                .unwrap_or(position)
        };
        history_start = start;
        if checkpointed_tail && start < position {
            entries[start..position]
                .iter_mut()
                .for_each(|entry| entry.visible = false);
        }
    }
    Ok((entries, history_start))
}

pub(super) fn load_index_range(path: &Path, start: u64, end: u64) -> Result<Vec<EventIndex>> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    reader.seek(SeekFrom::Start(start))?;
    let mut indices = Vec::new();
    let mut line = Vec::with_capacity(4 * 1024);
    while reader.stream_position()? < end {
        let Some((line_start, line_end, skel)) = read_index_skeleton(&mut reader, &mut line)?
        else {
            break;
        };
        if line_end > end {
            return Err(Error::State(format!(
                "index event extends beyond committed range {start}..{end} in {}",
                path.display()
            )));
        }
        if skel.kind == IndexKind::Cursor {
            continue;
        }
        indices.push(EventIndex {
            id: skel.id,
            parent_id: skel.parent_id,
            offset: line_start,
            end_offset: line_end,
            kind: skel.kind,
        });
    }
    Ok(indices)
}

/// # Errors
/// Returns the underlying IO error if the session file cannot be read or the
/// event at `offset` cannot be parsed.
pub(super) fn load_event_at(path: &Path, offset: u64) -> Result<SessionEvent> {
    let mut events = load_events_at(path, &[offset])?;
    events
        .pop()
        .ok_or_else(|| Error::State(format!("no event at offset {offset} in {}", path.display())))
}

/// # Errors
/// Returns an error when the transcript cannot be indexed or the selected
/// event cannot be read or parsed.
/// Deserialize small caller-defined projections at selected event offsets.
/// Unknown JSON fields are skipped directly from the buffered file stream, so
/// a projection does not allocate a complete backing line merely because an
/// unrelated field (such as a native-tool result) is huge.
/// # Errors
/// Returns an error when the transcript/offset cannot be read or projected
/// JSON is invalid.
pub(super) fn load_event_values<T: serde::de::DeserializeOwned>(
    path: &Path,
    offsets: &[u64],
) -> Result<Vec<T>> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut budget = ProjectionBudget::collection();
    let mut values = Vec::with_capacity(offsets.len());
    for &offset in offsets.iter().rev() {
        reader.seek(SeekFrom::Start(offset))?;
        let Some((_start, _end, value)) = read_projected_value::<T, _>(&mut reader, &mut budget)?
        else {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        };
        values.push(value);
    }
    values.reverse();
    Ok(values)
}

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

pub(super) fn visit_display_events(
    path: &Path,
    offsets: &[u64],
    mut visit: impl FnMut(SessionEvent) -> Result<()>,
) -> Result<()> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    for &offset in offsets {
        reader.seek(SeekFrom::Start(offset))?;
        let Some((_start, _end, event)) =
            read_projected_value::<SessionEvent, _>(&mut reader, &mut ProjectionBudget::event())?
        else {
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

/// Parse selected event values in one file pass. Offsets must be in ascending
/// order. Values are deserialized directly from the file, so peak memory is
/// the returned event payload rather than payload plus a complete JSON line.
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

pub(super) fn load_display_events_at(path: &Path, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
    let mut budget = ProjectionBudget::collection();
    let mut out = Vec::with_capacity(offsets.len());
    for &offset in offsets.iter().rev() {
        reader.seek(SeekFrom::Start(offset))?;
        let Some((_start, _end, event)) =
            read_projected_value::<SessionEvent, _>(&mut reader, &mut budget)?
        else {
            return Err(Error::State(format!(
                "no event at offset {offset} in {}",
                path.display()
            )));
        };
        if !matches!(event.kind, SessionEventKind::Cursor { .. }) {
            out.push(event);
        }
    }
    out.reverse();
    Ok(out)
}

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

pub(super) fn load_display_event_range(
    path: &Path,
    start: u64,
    end: u64,
) -> Result<Vec<SessionEvent>> {
    let offsets: Vec<u64> = load_index_range(path, start, end)?
        .into_iter()
        .map(|event| event.offset)
        .collect();
    load_display_events_at(path, &offsets)
}

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
/// # Errors
/// Returns an error when the requested leaf is absent, the lineage is cyclic,
/// or an indexed event cannot be read or parsed.
pub(super) fn load_compaction_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    let lineage = lineage_indices(index, leaf_id)?;

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
    load_display_index_entries(path, index, &lineage[start..])
}

/// Resolve the selected lineage of an index to event positions in root-to-leaf
/// order. Canonical walk shared by compaction and the picker scan; cursor
/// records reuse `id` for their selected leaf and are excluded from lookup.
/// # Errors
/// Returns an error when the lineage is cyclic or a named leaf is absent.
pub(super) fn lineage_indices(index: &[EventIndex], leaf_id: Option<&str>) -> Result<Vec<usize>> {
    use std::collections::HashMap;

    if index.is_empty() {
        return Ok(Vec::new());
    }
    let by_id: HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind != IndexKind::Cursor)
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
    Ok(lineage)
}

/// # Errors
/// Returns an error when the requested leaf is absent, the lineage is cyclic,
/// or an indexed event cannot be read or parsed.
pub(super) fn load_indexed_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    let lineage = lineage_indices(index, leaf_id)?;
    load_index_entries(path, index, &lineage)
}

fn load_index_entries(
    path: &Path,
    index: &[EventIndex],
    selected: &[usize],
) -> Result<Vec<SessionEvent>> {
    load_index_entries_with(path, index, selected, load_events_at)
}

fn load_display_index_entries(
    path: &Path,
    index: &[EventIndex],
    selected: &[usize],
) -> Result<Vec<SessionEvent>> {
    load_index_entries_with(path, index, selected, load_display_events_at)
}

fn load_index_entries_with(
    path: &Path,
    index: &[EventIndex],
    selected: &[usize],
    load: impl FnOnce(&Path, &[u64]) -> Result<Vec<SessionEvent>>,
) -> Result<Vec<SessionEvent>> {
    let offsets: Vec<u64> = selected.iter().map(|&i| index[i].offset).collect();
    let mut events = load(path, &offsets)?;
    for (event, &i) in events.iter_mut().zip(selected) {
        event.id = index[i].id.to_event_id();
        event.parent_id = index[i].parent_id.as_ref().map(IndexId::to_event_id);
    }
    Ok(events)
}
