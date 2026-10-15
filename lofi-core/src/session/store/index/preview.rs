#[allow(clippy::wildcard_imports)]
use super::*;

pub(in crate::session::store) enum PreviewCandidate {
    Single(u64),
    AssistantRun(Vec<u64>),
}

pub(in crate::session::store) struct SummaryAccumulator {
    pub(in crate::session::store) message_count: usize,
    pub(in crate::session::store) in_assistant_run: bool,
    pub(in crate::session::store) candidates: Vec<PreviewCandidate>,
}

impl SummaryAccumulator {
    pub(in crate::session::store) fn new() -> Self {
        Self {
            message_count: 0,
            in_assistant_run: false,
            candidates: Vec::new(),
        }
    }

    pub(in crate::session::store) fn push(&mut self, offset: u64, kind: IndexKind) {
        match kind {
            IndexKind::Cursor
            | IndexKind::BackendUsage
            | IndexKind::JobLifecycle
            | IndexKind::SubagentUsage
            | IndexKind::Other => {}
            IndexKind::SystemMessage => {
                self.message_count = self.message_count.saturating_add(1);
            }
            IndexKind::AssistantMessage => {
                if !self.in_assistant_run {
                    self.message_count = self.message_count.saturating_add(1);
                    self.in_assistant_run = true;
                    self.candidates
                        .push(PreviewCandidate::AssistantRun(vec![offset]));
                } else if let Some(PreviewCandidate::AssistantRun(run)) = self.candidates.last_mut()
                {
                    run.push(offset);
                }
            }
            IndexKind::ToolResult | IndexKind::TurnEnd => {
                self.in_assistant_run = false;
                if is_message_kind(kind) {
                    self.message_count = self.message_count.saturating_add(1);
                }
            }
            _ => {
                self.in_assistant_run = false;
                if is_message_kind(kind) {
                    self.message_count = self.message_count.saturating_add(1);
                }
                if is_preview_kind(kind) {
                    self.candidates.push(PreviewCandidate::Single(offset));
                }
            }
        }
    }
}

pub(in crate::session::store) fn first_preview_at<T: serde::de::DeserializeOwned>(
    reader: &mut std::io::BufReader<std::fs::File>,
    candidates: Vec<PreviewCandidate>,
    mut preview_for: impl FnMut(&[T]) -> String,
) -> Result<String> {
    use std::io::{Seek, SeekFrom};

    let mut budget = ProjectionBudget::event();
    let mut buf = Vec::new();
    for candidate in candidates.into_iter().rev() {
        buf.clear();
        match candidate {
            PreviewCandidate::Single(offset) => {
                reader.seek(SeekFrom::Start(offset))?;
                if let Some((_, _, value)) = read_projected_value::<T, _>(reader, &mut budget)? {
                    buf.push(value);
                }
            }
            PreviewCandidate::AssistantRun(offsets) => {
                for offset in offsets {
                    reader.seek(SeekFrom::Start(offset))?;
                    if let Some((_, _, value)) = read_projected_value::<T, _>(reader, &mut budget)?
                    {
                        buf.push(value);
                    }
                }
            }
        }
        if !buf.is_empty() {
            let preview = preview_for(&buf);
            if !preview.is_empty() {
                return Ok(preview);
            }
        }
    }
    Ok(String::new())
}

pub(in crate::session::store) fn scan_entry_summary<T: serde::de::DeserializeOwned>(
    path: &Path,
    preview_for: impl FnMut(&[T]) -> String,
) -> Result<(SessionMeta, usize, String)> {
    use std::io::{BufReader, Seek};

    let mut reader = BufReader::with_capacity(64 * 1024, std::fs::File::open(path)?);
    let Some((_start, _event_start, header)) = read_jsonl_value::<Header, _>(&mut reader)? else {
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

    let mut summary = SummaryAccumulator::new();
    let mut previous: Option<IndexId> = None;
    let mut latest_cursor: Option<Option<IndexId>> = None;
    let mut linear = true;
    let mut line = Vec::with_capacity(4 * 1024);
    loop {
        let record_start = reader.stream_position()?;
        let event = read_index_skeleton(&mut reader, &mut line);
        let Some((line_start, _line_end, skel)) = (match event {
            Ok(event) => event,
            Err(error) => {
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
        if skel.kind == IndexKind::Cursor {
            latest_cursor = Some(skel.cursor_leaf);
            continue;
        }
        latest_cursor = None;
        let linked = match (&skel.parent_id, &previous) {
            (None, None) => true,
            (Some(parent), Some(prev)) => parent == prev,
            _ => false,
        };
        if !linked {
            linear = false;
            break;
        }
        summary.push(line_start, skel.kind);
        previous = Some(skel.id);
    }

    if linear {
        if latest_cursor == Some(None) {
            return Ok((header.meta, 0, String::new()));
        }
        if latest_cursor
            .as_ref()
            .is_none_or(|leaf| leaf.as_ref() == previous.as_ref())
        {
            let last_message = first_preview_at(&mut reader, summary.candidates, preview_for)?;
            return Ok((header.meta, summary.message_count, last_message));
        }
    }

    let (meta, index, _size) = load_index(path)?;
    let leaf = index
        .iter()
        .rev()
        .find(|e| e.kind == IndexKind::Cursor)
        .map(|e| e.id.to_event_id())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            index
                .iter()
                .rev()
                .find(|e| e.kind != IndexKind::Cursor)
                .map(|e| e.id.to_event_id())
        });
    let lineage = crate::session::store::cursor::lineage_indices(&index, leaf.as_deref())?;
    let mut branched = SummaryAccumulator::new();
    for i in lineage {
        branched.push(index[i].offset, index[i].kind);
    }
    let last_message = first_preview_at(&mut reader, branched.candidates, preview_for)?;
    Ok((meta, branched.message_count, last_message))
}

pub(in crate::session::store) fn load_event_values<T: serde::de::DeserializeOwned>(
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
