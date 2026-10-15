#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::session::store) struct IndexedSkeleton {
    pub(in crate::session::store) id: IndexId,
    pub(in crate::session::store) parent_id: Option<IndexId>,
    pub(in crate::session::store) kind: IndexKind,
    pub(in crate::session::store) cursor_leaf: Option<IndexId>,
    pub(in crate::session::store) checkpointed_tail: bool,
    pub(in crate::session::store) first_kept_entry_id: IndexId,
}

pub(in crate::session::store) fn read_index_skeleton<R>(
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

pub(in crate::session::store) fn read_jsonl_value<T, R>(
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

pub(in crate::session::store) fn read_session_event<R>(
    reader: &mut std::io::BufReader<R>,
) -> Result<Option<(u64, u64, SessionEvent)>>
where
    R: std::io::Read + std::io::Seek,
{
    // Seek is retained in the bound because callers operate on seekable
    // files; the read itself only advances the cursor.
    read_jsonl_value::<SessionEvent, _>(reader)
}

pub(in crate::session::store) fn count_newlines(reader: &mut impl std::io::Read) -> Result<usize> {
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

pub(in crate::session::store) fn malformed_record_is_final<R>(
    reader: &mut std::io::BufReader<R>,
    start: u64,
) -> Result<bool>
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

pub(in crate::session::store) struct LinearSessionIndex {
    pub meta: SessionMeta,
    pub entries: Vec<SessionIndexEntry>,
    pub file_size: u64,
    pub leaf_id: Option<String>,
    pub history_start: usize,
    pub compaction_suffix: Vec<EventIndex>,
}

pub(in crate::session::store) struct LinearScan {
    pub(in crate::session::store) count: usize,
    pub(in crate::session::store) leaf: Option<IndexId>,
    pub(in crate::session::store) boundaries: HashSet<IndexId>,
    pub(in crate::session::store) latest_compaction_start: Option<IndexId>,
    pub(in crate::session::store) latest_compaction_marker: Option<IndexId>,
    pub(in crate::session::store) file_size: u64,
}

pub(in crate::session::store) fn scan_linear_session(
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
        latest_cursor = None;
        let id = skel.id;
        let parent = skel.parent_id;
        linear &= match (&parent, &previous) {
            (None, None) => true,
            (Some(parent), Some(previous)) => parent == previous,
            _ => false,
        };
        if !linear {
            return Ok(None);
        }
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

pub(in crate::session::store) fn project_linear_session(
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
pub(in crate::session::store) fn load_linear_session_index(
    path: &Path,
) -> Result<Option<LinearSessionIndex>> {
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
