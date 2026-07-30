#![allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn visible_index_path(
    cursor: &store::SessionCursor,
    index: &[store::EventIndex],
) -> Result<Vec<usize>> {
    use std::collections::HashSet;
    let mut hidden = HashSet::new();
    for (pos, event) in index.iter().enumerate() {
        if event.kind != store::IndexKind::Compaction {
            continue;
        }
        let ev = cursor.event_at(event.offset)?;
        if let SessionEventKind::Compaction {
            first_kept_entry_id,
            checkpointed_tail: true,
            detached: false,
            ..
        } = ev.kind
        {
            if let Some(start) = index[..pos]
                .iter()
                .position(|event| event.id.matches(&first_kept_entry_id))
            {
                hidden.extend(start..pos);
            }
        }
    }
    Ok((0..index.len()).filter(|i| !hidden.contains(i)).collect())
}

/// Restore the transcript as file-backed turn shells. Turns need only their
/// prompt, offsets, and terminal accounting at startup; their full blocks are
/// materialized from disk only when the viewport reaches them. This includes
/// the final historical turn: eagerly hydrating it makes resume memory depend
/// on the size of the last response.
pub(super) fn replay_indexed_session(
    app: &mut App,
    cursor: &store::SessionCursor,
    index: &[store::EventIndex],
    file_size: u64,
) -> Result<()> {
    let visible = visible_index_path(cursor, index)?;
    let starts: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter_map(|(p, &i)| {
            matches!(
                index[i].kind,
                store::IndexKind::UserPrompt | store::IndexKind::UserBash
            )
            .then_some(p)
        })
        .collect();
    let prompt_entries: Vec<(usize, u64)> = starts
        .iter()
        .enumerate()
        .filter_map(|(turn, &start_pos)| {
            (index[visible[start_pos]].kind == store::IndexKind::UserPrompt)
                .then_some((turn, index[visible[start_pos]].offset))
        })
        .collect();
    let prompt_offsets: Vec<u64> = prompt_entries.iter().map(|(_, offset)| *offset).collect();
    let prompt_texts = cursor.prompt_texts(&prompt_offsets)?;
    let mut prompts = vec![String::new(); starts.len()];
    for ((turn, _), prompt) in prompt_entries.into_iter().zip(prompt_texts) {
        prompts[turn] = prompt;
    }
    app.turn_byte_ranges.clear();
    app.turn_event_offsets.clear();
    for (turn, &start_pos) in starts.iter().enumerate() {
        let end_pos = starts.get(turn + 1).copied().unwrap_or(visible.len());
        let selected = &visible[start_pos..end_pos];
        let offsets: Vec<u64> = selected.iter().map(|&i| index[i].offset).collect();
        if index[visible[start_pos]].kind == store::IndexKind::UserBash {
            let event = cursor.event_at(index[visible[start_pos]].offset)?;
            replay_selected_session_events(&[event], |ev| {
                app.apply_file_backed_replay_event(ev);
            });
        } else {
            app.apply_file_backed_replay_event(AgentEvent::TurnStart {
                prompt: prompts.get(turn).cloned().unwrap_or_default(),
            });
            for &i in selected {
                if !matches!(
                    index[i].kind,
                    store::IndexKind::TurnEnd
                        | store::IndexKind::TurnFailed
                        | store::IndexKind::TurnCancelled
                ) {
                    continue;
                }
                let event = cursor.event_at(index[i].offset)?;
                replay_selected_session_events(&[event], |ev| {
                    app.apply_file_backed_replay_event(ev);
                });
            }
        }
        let start = index[visible[start_pos]].offset;
        let lineage_end = visible.last().map_or(file_size, |&i| index[i].end_offset);
        let end = starts
            .get(turn + 1)
            .map_or(lineage_end, |&p| index[visible[p]].offset);
        if let Some(range) = app.turn_byte_ranges.last_mut() {
            *range = Some((start, end));
        }
        if let Some(event_offsets) = app.turn_event_offsets.last_mut() {
            *event_offsets = Some(offsets);
        }
        if let Some(shell_turn) = app.turns.last_mut() {
            shell_turn.blocks.clear();
        }
    }
    debug_assert_eq!(app.turns.len(), app.turn_byte_ranges.len());
    debug_assert_eq!(app.turns.len(), app.turn_event_offsets.len());
    Ok(())
}

pub(super) fn last_run_model_from_index(
    cursor: &store::SessionCursor,
    index: &[store::EventIndex],
) -> Option<RunModel> {
    for i in (0..index.len()).rev() {
        if !matches!(
            index[i].kind,
            store::IndexKind::TurnEnd
                | store::IndexKind::TurnFailed
                | store::IndexKind::TurnCancelled
        ) {
            continue;
        }
        match cursor.event_at(index[i].offset).ok()?.kind {
            SessionEventKind::TurnEnd { model, .. }
            | SessionEventKind::TurnFailed { model, .. }
            | SessionEventKind::TurnCancelled { model, .. } => return Some(model),
            _ => {}
        }
    }
    None
}

pub(super) fn restore_compaction_from_index(
    app: &mut App,
    cursor: &store::SessionCursor,
    index: &[store::EventIndex],
) {
    let mut last_compaction_pos = None;
    let mut last_usage = None;
    for (pos, event) in index.iter().enumerate() {
        match event.kind {
            store::IndexKind::Compaction => last_compaction_pos = Some(pos),
            store::IndexKind::TurnEnd
            | store::IndexKind::TurnFailed
            | store::IndexKind::TurnCancelled => {
                if let Ok(ev) = cursor.event_at(event.offset) {
                    match ev.kind {
                        SessionEventKind::TurnEnd { usage, .. }
                        | SessionEventKind::TurnFailed { usage, .. }
                        | SessionEventKind::TurnCancelled { usage, .. } => {
                            last_usage = Some((pos, usage));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let usage_after_compaction = last_usage
        .filter(|(pos, _)| last_compaction_pos.is_none_or(|compact_pos| *pos > compact_pos))
        .map(|(_, usage)| usage);
    app.compacted = last_compaction_pos.is_some() && usage_after_compaction.is_none();
    app.status_usage = usage_after_compaction;
}
