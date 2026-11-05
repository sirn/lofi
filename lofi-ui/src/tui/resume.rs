#![allow(clippy::wildcard_imports)]
use std::collections::HashSet;

use super::*;

pub(super) use lofi_core::session::replay::last_run_model_from_index;

/// Records the indexed replay applies to the gauges: the per-turn
/// accounting, plus the compaction that resets them. One definition, because
/// the sweep that reads them and the loop that applies them must agree on the
/// set or every later turn reads the wrong record.
fn is_accounting(kind: store::IndexKind) -> bool {
    matches!(
        kind,
        store::IndexKind::TurnEnd
            | store::IndexKind::TurnFailed
            | store::IndexKind::TurnCancelled
            | store::IndexKind::TurnUsage
            | store::IndexKind::SubagentUsage
            | store::IndexKind::ContextReported
            | store::IndexKind::Compaction
    )
}

/// The offsets the gauges apply, in file order: every accounting record,
/// except that a turn reports its spend once.
///
/// A terminal marker carries the whole turn and supersedes the snapshots its
/// own checkpoints wrote, so it replaces them. A turn no marker settled — a
/// process killed mid-turn wrote none — reports its last snapshot, which is
/// all a resume has of what that turn cost.
fn accounting_offsets(
    index: &[store::SessionIndexEntry],
    visible: &[usize],
    notices: &HashSet<usize>,
) -> Vec<u64> {
    let mut kept: Vec<u64> = Vec::new();
    let mut snapshot: Option<u64> = None;
    for (pos, &i) in visible.iter().enumerate() {
        let entry = &index[i];
        match entry.kind {
            store::IndexKind::TurnUsage => snapshot = Some(entry.offset),
            store::IndexKind::TurnEnd
            | store::IndexKind::TurnFailed
            | store::IndexKind::TurnCancelled => {
                snapshot = None;
                kept.push(entry.offset);
            }
            // A prompt that is not a notice opens the next turn, which settles
            // the snapshot of a turn that never got its marker.
            store::IndexKind::UserPrompt if !notices.contains(&pos) => kept.extend(snapshot.take()),
            kind if is_accounting(kind) => kept.push(entry.offset),
            _ => {}
        }
    }
    kept.extend(snapshot);
    kept
}

/// Restore the transcript as file-backed turn shells. Turns need only their
/// prompt, offsets, and terminal accounting at startup; their full blocks are
/// materialized from disk only when the viewport reaches them. This includes
/// the final historical turn: eagerly hydrating it makes resume memory depend
/// on the size of the last response.
pub(super) fn replay_indexed_session(
    app: &mut App,
    cursor: &store::SessionCursor,
    index: &[store::SessionIndexEntry],
    file_size: u64,
    contiguous: bool,
) -> Result<()> {
    let visible = lofi_core::session::replay::visible_index_path(index);
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
    let prompt_texts = cursor.prompt_texts(&prompt_offsets)?;
    let mut prompts = vec![(String::new(), lofi_types::PromptKind::User); starts.len()];
    for ((turn, _), prompt) in prompt_entries.into_iter().zip(prompt_texts) {
        prompts[turn] = prompt;
    }
    app.transcript.turn_byte_ranges.clear();
    app.transcript.turn_event_offsets.clear();
    // A notice opens a display row, not a turn: its spend belongs to the turn
    // that auto-continued through it.
    let notices: HashSet<usize> = starts
        .iter()
        .enumerate()
        .filter(|(turn, _)| {
            prompts
                .get(*turn)
                .is_some_and(|(_, kind)| *kind == lofi_types::PromptKind::Notice)
        })
        .map(|(_, &pos)| pos)
        .collect();
    // Read every turn-outcome event in one sweep of the file: per-turn
    // `event_at` would open and seek the transcript once per turn — over a
    // thousand opens on a long resume.
    let applied_offsets = accounting_offsets(index, &visible, &notices);
    let mut outcomes: Vec<SessionEvent> = Vec::with_capacity(applied_offsets.len());
    cursor.visit_events(&applied_offsets, |ev| {
        outcomes.push(ev);
        Ok(())
    })?;
    let mut next_outcome = 0usize;
    for (turn, &start_pos) in starts.iter().enumerate() {
        let end_pos = starts.get(turn + 1).copied().unwrap_or(visible.len());
        let selected = &visible[start_pos..end_pos];
        let offsets: Vec<u64> = selected.iter().map(|&i| index[i].offset).collect();
        if index[visible[start_pos]].kind == store::IndexKind::UserShell {
            let event = cursor.display_event_at(index[visible[start_pos]].offset)?;
            replay_selected_session_events(&[event], |ev| {
                app.apply_file_backed_replay_event(ev);
            });
        } else {
            // Notices keep their persisted kind: a hollow bullet marks them
            // as app-injected, not typed input.
            let (prompt, kind) = prompts.get(turn).cloned().unwrap_or_default();
            app.apply_file_backed_replay_event(AgentEvent::Prompt { prompt, kind });
        }
        let start = index[visible[start_pos]].offset;
        let lineage_end = visible.last().map_or(file_size, |&i| index[i].end_offset);
        let end = starts
            .get(turn + 1)
            .map_or(lineage_end, |&p| index[visible[p]].offset);
        // A shell turn has no accounting of its own, but a compaction or a
        // subagent's spend that followed it belongs to this range: apply what
        // the sweep read, or the gauges drift. A killed turn's spend lands in
        // the row it opened, before the next turn reports its own.
        while next_outcome < applied_offsets.len() && applied_offsets[next_outcome] < end {
            let event = &outcomes[next_outcome];
            next_outcome += 1;
            replay_selected_session_events(std::slice::from_ref(event), |ev| {
                app.apply_file_backed_replay_event(ev);
            });
        }
        app.transcript
            .restore_last_turn(start, end, offsets, contiguous);
    }
    debug_assert_eq!(
        app.transcript.turns.len(),
        app.transcript.turn_byte_ranges.len()
    );
    debug_assert_eq!(
        app.transcript.turns.len(),
        app.transcript.turn_event_offsets.len()
    );
    Ok(())
}

pub(super) fn restore_compaction_from_index(
    app: &mut App,
    cursor: &store::SessionCursor,
    index: &[store::SessionIndexEntry],
) {
    let (compacted, status_usage) =
        lofi_core::session::replay::compaction_status_from_index(cursor, index);
    app.compacted = compacted;
    app.status_usage = status_usage;
}
