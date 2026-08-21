#![allow(clippy::wildcard_imports)]
use super::*;

pub(super) use lofi_core::session::replay::last_run_model_from_index;

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
    let t0 = std::time::Instant::now();
    let visible = lofi_core::session::replay::visible_index_path(cursor, index);
    eprintln!("[phase] visible_index_path: {:?}", t0.elapsed());
    let starts: Vec<usize> = visible
        .iter()
        .enumerate()
        .filter_map(|(p, &i)| {
            matches!(
                index[i].kind,
                store::IndexKind::UserPrompt | store::IndexKind::UserShell
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
    let t1 = std::time::Instant::now();
    let prompt_texts = cursor.prompt_texts(&prompt_offsets)?;
    eprintln!("[phase] prompt_texts: {:?}", t1.elapsed());
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
        if index[visible[start_pos]].kind == store::IndexKind::UserShell {
            let event = cursor.event_at(index[visible[start_pos]].offset)?;
            replay_selected_session_events(&[event], |ev| {
                app.apply_file_backed_replay_event(ev);
            });
        } else {
            app.apply_file_backed_replay_event(AgentEvent::TurnStart {
                prompt: prompts.get(turn).cloned().unwrap_or_default(),
                // The kind is not persisted; replays render every prompt as a
                // user turn.
                kind: lofi_types::PromptKind::User,
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
    eprintln!("[phase] replay loop: {:?}", t0.elapsed());
    debug_assert_eq!(app.turns.len(), app.turn_byte_ranges.len());
    debug_assert_eq!(app.turns.len(), app.turn_event_offsets.len());
    Ok(())
}

pub(super) fn restore_compaction_from_index(
    app: &mut App,
    cursor: &store::SessionCursor,
    index: &[store::EventIndex],
) {
    let (compacted, status_usage) =
        lofi_core::session::replay::compaction_status_from_index(cursor, index);
    app.compacted = compacted;
    app.status_usage = status_usage;
}
