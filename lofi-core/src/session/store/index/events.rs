#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::session::store) fn visit_events(
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

pub(in crate::session::store) fn visit_display_events(
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
pub(in crate::session::store) fn load_events_at(
    path: &Path,
    offsets: &[u64],
) -> Result<Vec<SessionEvent>> {
    let mut out = Vec::with_capacity(offsets.len());
    visit_events(path, offsets, |event| {
        out.push(event);
        Ok(())
    })?;
    Ok(out)
}

pub(in crate::session::store) fn load_display_events_at(
    path: &Path,
    offsets: &[u64],
) -> Result<Vec<SessionEvent>> {
    load_projected_events_at(path, offsets, ProjectionBudget::collection())
}

pub(in crate::session::store) fn load_projected_events_at(
    path: &Path,
    offsets: &[u64],
    mut budget: ProjectionBudget,
) -> Result<Vec<SessionEvent>> {
    use std::io::{BufReader, Seek, SeekFrom};

    let mut reader = BufReader::new(std::fs::File::open(path)?);
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

pub(in crate::session::store) fn load_event_range(
    path: &Path,
    start: u64,
    end: u64,
) -> Result<Vec<SessionEvent>> {
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

pub(in crate::session::store) fn load_display_event_range(
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

pub(in crate::session::store) fn compaction_index_suffix(
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
pub(in crate::session::store) fn load_compaction_path(
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
    load_compaction_index_entries(path, index, &lineage[start..])
}

/// Resolve the selected lineage of an index to event positions in root-to-leaf
/// order. Canonical walk shared by compaction and the picker scan; cursor
/// records reuse `id` for their selected leaf and are excluded from lookup.
/// # Errors
/// Returns an error when the lineage is cyclic or a named leaf is absent.
pub(in crate::session::store) fn lineage_indices(
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<usize>> {
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
pub(in crate::session::store) fn load_indexed_path(
    path: &Path,
    index: &[EventIndex],
    leaf_id: Option<&str>,
) -> Result<Vec<SessionEvent>> {
    let lineage = lineage_indices(index, leaf_id)?;
    load_index_entries(path, index, &lineage)
}

pub(in crate::session::store) fn load_index_entries(
    path: &Path,
    index: &[EventIndex],
    selected: &[usize],
) -> Result<Vec<SessionEvent>> {
    load_index_entries_with(path, index, selected, load_events_at)
}

pub(in crate::session::store) fn load_compaction_index_entries(
    path: &Path,
    index: &[EventIndex],
    selected: &[usize],
) -> Result<Vec<SessionEvent>> {
    load_index_entries_with(path, index, selected, |path, offsets| {
        load_projected_events_at(path, offsets, ProjectionBudget::compaction())
    })
}

pub(in crate::session::store) fn load_index_entries_with(
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
