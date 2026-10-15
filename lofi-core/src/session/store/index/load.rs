#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::session::store) fn load_index(
    path: &Path,
) -> Result<(SessionMeta, Vec<EventIndex>, u64)> {
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
            latest_cursor = None;
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
pub(in crate::session::store) fn session_index_from_lineage(
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

pub(in crate::session::store) fn load_index_range(
    path: &Path,
    start: u64,
    end: u64,
) -> Result<Vec<EventIndex>> {
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
pub(in crate::session::store) fn load_event_at(path: &Path, offset: u64) -> Result<SessionEvent> {
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
pub(in crate::session::store) fn is_message_kind(kind: IndexKind) -> bool {
    matches!(
        kind,
        IndexKind::UserPrompt
            | IndexKind::AssistantMessage
            | IndexKind::ToolResult
            | IndexKind::SystemMessage
    )
}

pub(in crate::session::store) fn is_preview_kind(kind: IndexKind) -> bool {
    matches!(
        kind,
        IndexKind::UserPrompt
            | IndexKind::UserShell
            | IndexKind::AssistantMessage
            | IndexKind::TurnFailed
            | IndexKind::TurnCancelled
            | IndexKind::Compaction
            | IndexKind::NativeTool
    )
}
