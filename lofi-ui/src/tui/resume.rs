#![allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn active_index_leaf(index: &[store::EventIndex]) -> Option<String> {
    index.last().map(|event| event.id.clone())
}

fn active_index_path(index: &[store::EventIndex]) -> Vec<usize> {
    use std::collections::HashMap;
    let by_id: HashMap<&str, usize> = index
        .iter()
        .enumerate()
        .map(|(i, e)| (e.id.as_str(), i))
        .collect();
    let mut out = Vec::new();
    let mut cur = index.len().checked_sub(1);
    while let Some(i) = cur {
        out.push(i);
        cur = index[i]
            .parent_id
            .as_deref()
            .and_then(|id| by_id.get(id).copied());
        if out.len() > index.len() {
            return Vec::new();
        }
    }
    out.reverse();
    out
}

pub(super) fn visible_index_path(path: &Path, index: &[store::EventIndex]) -> Result<Vec<usize>> {
    use std::collections::HashSet;
    let active = active_index_path(index);
    let mut hidden = HashSet::new();
    for (pos, &i) in active.iter().enumerate() {
        if index[i].kind != store::IndexKind::Compaction {
            continue;
        }
        let ev = store::load_event_at(path, index[i].offset)?;
        if let SessionEventKind::Compaction {
            first_kept_entry_id,
            checkpointed_tail: true,
            ..
        } = ev.kind
        {
            if let Some(start) = active[..pos]
                .iter()
                .position(|&j| index[j].id == first_kept_entry_id)
            {
                hidden.extend(active[start..pos].iter().copied());
            }
        }
    }
    Ok(active.into_iter().filter(|i| !hidden.contains(i)).collect())
}

pub(super) fn history_from_index(
    path: &Path,
    index: &[store::EventIndex],
    edit: &lofi_types::EditConfig,
) -> Result<Vec<Message>> {
    let active = active_index_path(index);
    let mut start = 0;
    for (pos, &i) in active.iter().enumerate().rev() {
        if index[i].kind != store::IndexKind::Compaction {
            continue;
        }
        let ev = store::load_event_at(path, index[i].offset)?;
        if let SessionEventKind::Compaction {
            first_kept_entry_id,
            ..
        } = &ev.kind
        {
            start = if first_kept_entry_id.is_empty() {
                pos
            } else {
                active[..pos]
                    .iter()
                    .position(|&j| index[j].id == *first_kept_entry_id)
                    .unwrap_or(pos)
            };
            break;
        }
    }
    let offsets: Vec<u64> = active[start..].iter().map(|&i| index[i].offset).collect();
    let events = store::load_events_at(path, &offsets)?;
    Ok(messages_from_events(&events, edit))
}

pub(super) fn replay_indexed_session(
    app: &mut App,
    path: &Path,
    index: &[store::EventIndex],
    file_size: u64,
) -> Result<()> {
    let visible = visible_index_path(path, index)?;
    let starts: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter_map(|(p, &i)| (index[i].kind == store::IndexKind::UserPrompt).then_some(p))
        .collect();
    app.turn_byte_ranges.clear();
    for (turn, &start_pos) in starts.iter().enumerate() {
        let end_pos = starts.get(turn + 1).copied().unwrap_or(visible.len());
        let offsets: Vec<u64> = visible[start_pos..end_pos]
            .iter()
            .map(|&i| index[i].offset)
            .collect();
        let events = store::load_events_at(path, &offsets)?;
        replay_session_events(&events, |ev| app.apply_file_backed_replay_event(ev));
        let start = index[visible[start_pos]].offset;
        let end = starts
            .get(turn + 1)
            .map_or(file_size, |&p| index[visible[p]].offset);
        if let Some(range) = app.turn_byte_ranges.last_mut() {
            *range = Some((start, end));
        }
    }
    Ok(())
}

pub(super) fn last_run_model_from_index(
    path: &Path,
    index: &[store::EventIndex],
) -> Option<RunModel> {
    for &i in active_index_path(index).iter().rev() {
        if !matches!(
            index[i].kind,
            store::IndexKind::TurnEnd | store::IndexKind::TurnFailed
        ) {
            continue;
        }
        match store::load_event_at(path, index[i].offset).ok()?.kind {
            SessionEventKind::TurnEnd { model, .. }
            | SessionEventKind::TurnFailed { model, .. } => return Some(model),
            _ => {}
        }
    }
    None
}

pub(super) fn restore_compaction_from_index(
    app: &mut App,
    path: &Path,
    index: &[store::EventIndex],
) {
    let active = active_index_path(index);
    let mut last_compaction_pos = None;
    let mut last_usage = None;
    for (pos, &i) in active.iter().enumerate() {
        match index[i].kind {
            store::IndexKind::Compaction => last_compaction_pos = Some(pos),
            store::IndexKind::TurnEnd | store::IndexKind::TurnFailed => {
                if let Ok(ev) = store::load_event_at(path, index[i].offset) {
                    match ev.kind {
                        SessionEventKind::TurnEnd { usage, .. }
                        | SessionEventKind::TurnFailed { usage, .. } => {
                            last_usage = Some((pos, usage));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // A compaction invalidates every older provider-usage measurement. This
    // remains true when partial continuation messages follow the marker but
    // no new terminal usage event was committed before shutdown.
    let usage_after_compaction = last_usage
        .filter(|(pos, _)| last_compaction_pos.is_none_or(|compact_pos| *pos > compact_pos))
        .map(|(_, usage)| usage);
    app.compacted = last_compaction_pos.is_some() && usage_after_compaction.is_none();
    // Resume itself never compacts. Leave hysteresis unarmed so the first
    // newly completed model round is evaluated against the soft cap instead
    // of inheriting a missed pre-shutdown crossing forever.
    app.prev_ctx_tokens = None;
    app.status_usage = usage_after_compaction;
}
