#![allow(clippy::wildcard_imports)]

use super::*;

/// Borrow the last Tool block matching id (newest-first).
/// Stamp the elapsed duration on the trailing thinking block, if any is
/// still open. Called whenever the stream moves on to a different block
/// kind or the run ends.
pub(super) fn finalize_open_thinking(turn: &mut Turn) {
    if let Some(Block::Thinking(t)) = turn.blocks.last_mut() {
        if t.elapsed.is_none() {
            t.elapsed = Some(t.start.elapsed());
        }
    }
}

pub(super) fn tool_mut<'a>(blocks: &'a mut [Block], id: &str) -> Option<&'a mut ToolCall> {
    blocks.iter_mut().rev().find_map(|b| match b {
        Block::Tool(t) if t.id == id => Some(t),
        _ => None,
    })
}

/// Active-path event indices used for visible transcript replay. Context-edited
/// kept-tail copies are model-history checkpoints, not additional UI turns.
fn visible_event_indices(events: &[SessionEvent]) -> Vec<usize> {
    use std::collections::HashSet;
    let path = store::active_path_from_leaf(events);
    let mut hidden: HashSet<usize> = HashSet::new();
    for (marker_pos, &event_idx) in path.iter().enumerate() {
        if let SessionEventKind::Compaction {
            first_kept_entry_id,
            checkpointed_tail: true,
            ..
        } = &events[event_idx].kind
        {
            if let Some(start_pos) = path[..marker_pos]
                .iter()
                .position(|&i| events[i].id == *first_kept_entry_id)
            {
                hidden.extend(path[start_pos..marker_pos].iter().copied());
            }
        }
    }
    path.into_iter().filter(|i| !hidden.contains(i)).collect()
}

/// Build per-turn byte ranges from a transcript event log and the byte
/// offset of each event's line. A turn starts at a `User` message that isn't
/// a tool-result (mirroring [`turns_from_events`]); its byte range runs from
/// that line's offset to the next turn's start, or to `file_size` for the
/// last turn. Parallel to the `Vec<Turn>` returned by [`turns_from_events`].
#[cfg(test)]
pub(super) fn turn_byte_ranges_from_events(
    events: &[SessionEvent],
    offsets: &[u64],
    file_size: u64,
) -> Vec<Option<(u64, u64)>> {
    let mut ranges: Vec<Option<(u64, u64)>> = Vec::new();
    let mut cur_start: Option<u64> = None;
    for i in visible_event_indices(events) {
        let ev = &events[i];
        let is_turn_start = matches!(
            &ev.kind,
            SessionEventKind::Message(m)
                if m.role == Role::User
                    && !m.blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        );
        if is_turn_start {
            if let Some(start) = cur_start {
                if let Some(last) = ranges.last_mut() {
                    *last = Some((start, offsets[i]));
                }
            }
            cur_start = Some(offsets[i]);
            ranges.push(None);
        }
    }
    if let Some(start) = cur_start {
        if let Some(last) = ranges.last_mut() {
            *last = Some((start, file_size));
        }
    }
    ranges
}

/// The turn-building core of [`App::apply_event`], free of `App`-owned state
/// (cost/usage totals, retry indicator, byte ranges). Shared by the live
/// `apply_event` and by [`turns_from_session_events`] (used to materialize a
/// frozen turn from its byte range on demand) so there is exactly one place
/// that maps an `AgentEvent` to `Block`s.
///
/// `TurnStart` pushes a new turn; every other event mutates the last turn.
/// Status-only events (`RetryStart`/`RetryEnd`/`TurnCommitted`) are no-ops
/// here — the caller (`App::apply_event`) handles them before calling this.
// One match over AgentEvent shaping the turn list; per-variant helpers would
// scatter the shared turn/byte-range state.
#[allow(clippy::too_many_lines)]
pub(super) fn apply_event_to_turns(turns: &mut Vec<Turn>, ev: AgentEvent) {
    if let AgentEvent::TurnStart { prompt } = ev {
        turns.push(Turn {
            prompt,
            blocks: Vec::new(),
        });
        return;
    }
    if let AgentEvent::UserBash {
        command,
        output,
        exit_code,
        signal,
        duration_ms,
        truncated,
        cancelled,
        exclude_from_context,
    } = ev
    {
        turns.push(Turn {
            prompt: String::new(),
            blocks: vec![Block::UserBash {
                command,
                output,
                exit_code,
                signal,
                duration: Duration::from_millis(duration_ms),
                truncated,
                cancelled,
                exclude_from_context,
            }],
        });
        return;
    }
    let Some(turn) = turns.last_mut() else {
        return;
    };
    match ev {
        AgentEvent::Text(delta) => {
            if let Some(Block::Text(t)) = turn.blocks.last_mut() {
                t.push_str(&delta);
            } else {
                finalize_open_thinking(turn);
                turn.blocks.push(Block::Text(delta));
            }
        }
        AgentEvent::Thinking(delta) => {
            if let Some(Block::Thinking(t)) = turn.blocks.last_mut() {
                if t.elapsed.is_none() {
                    t.text.push_str(&delta);
                    return;
                }
            }
            turn.blocks.push(Block::Thinking(ThinkingBlock {
                text: delta,
                start: Instant::now(),
                elapsed: None,
            }));
        }
        AgentEvent::ThinkingEnd { elapsed_ms } => {
            // The engine owns the thinking-block timer; stamp it here so the
            // "Thought for Ns" marker matches the persisted `ThinkingTiming`
            // on resume (the UI's own `start` Instant is only a fallback while
            // the block is still open).
            if let Some(Block::Thinking(t)) = turn.blocks.last_mut() {
                if t.elapsed.is_none() {
                    t.elapsed = Some(Duration::from_millis(elapsed_ms));
                }
            }
        }
        AgentEvent::ToolStart { id, name } => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::Tool(ToolCall {
                id,
                name,
                input: String::new(),
                label: None,
                native: Vec::new(),
                result: None,
                is_error: false,
                done: false,
                elapsed: None,
            }));
        }
        AgentEvent::ToolInput { id, code, label } => {
            if let Some(t) = tool_mut(&mut turn.blocks, &id) {
                // Streamed deltas already grew `input`; the finalized event
                // replaces it with the authoritative full code and stamps the
                // label. Falls back to `code` when nothing streamed.
                t.input = code;
                if t.label.is_none() {
                    t.label = label;
                }
            }
        }
        AgentEvent::ToolInputDelta { id, delta } => {
            if let Some(t) = tool_mut(&mut turn.blocks, &id) {
                t.input.push_str(&delta);
            }
        }
        AgentEvent::ToolEnd { id, result, is_error, elapsed_ms } => {
            if let Some(t) = tool_mut(&mut turn.blocks, &id) {
                if is_error {
                    // A rejected exec promise cancels sibling native-tool
                    // futures. Their End events may never be produced (or may
                    // still be queued behind this parent event), so settle any
                    // open rows now rather than leaving stale spinners behind.
                    for nt in &mut t.native {
                        if !nt.done {
                            nt.result = Some("cancelled because parent exec failed".to_string());
                            nt.is_error = true;
                            nt.done = true;
                        }
                    }
                }
                t.result = Some(result);
                t.is_error = is_error;
                t.done = true;
                t.elapsed = Some(Duration::from_millis(elapsed_ms));
            }
        }
        AgentEvent::NativeToolStart {
            parent,
            id,
            name,
            args,
        } => {
            if let Some(t) = tool_mut(&mut turn.blocks, &parent) {
                t.native.push(NativeTool {
                    id,
                    name,
                    args,
                    result: None,
                    is_error: false,
                    done: false,
                });
            }
        }
        AgentEvent::NativeToolEnd {
            parent,
            id,
            result,
            is_error,
        } => {
            if let Some(t) = tool_mut(&mut turn.blocks, &parent) {
                if let Some(nt) = t.native.iter_mut().find(|n| n.id == id) {
                    nt.result = Some(result);
                    nt.is_error = is_error;
                    nt.done = true;
                }
            }
        }
        AgentEvent::TurnEnd { model, elapsed_ms, .. } => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::TurnEnd {
                label: model.label(),
                elapsed: Duration::from_millis(elapsed_ms),
            });
        }
        AgentEvent::TurnFailed { model, elapsed_ms, error, .. } => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::TurnFailed {
                label: model.label(),
                elapsed: Duration::from_millis(elapsed_ms),
                error,
            });
        }
        AgentEvent::Error(msg) => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::Error(msg));
        }
        AgentEvent::Compaction { summarized, kept, summary } => {
            turn.blocks.push(Block::Compaction { summarized, kept, summary });
        }
        // Status-only events are handled by `App::apply_event` before
        // reaching this builder; they are no-ops here.
        AgentEvent::RetryStart { .. }
        | AgentEvent::RetryEnd { .. }
        | AgentEvent::TurnCommitted { .. }
        | AgentEvent::RoundUsage { .. }
        | AgentEvent::TurnStart { .. }
        | AgentEvent::UserBash { .. }
        // Live-only signals handled by `App::apply_event`; no block here.
        | AgentEvent::TurnContinue
        | AgentEvent::ContextPressure { .. } => {}
    }
}

/// Build a `Vec<Turn>` from a transcript event log by replaying each converted
/// event directly through [`apply_event_to_turns`]. Used to materialize a frozen turn from its byte
/// range on demand (`materialize_turn`); the live path and full-session
/// resume go through `App::apply_event` instead, which also updates totals.
pub(super) fn turns_from_session_events(events: &[SessionEvent]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    replay_session_events(events, |ev| apply_event_to_turns(&mut turns, ev));
    turns
}

/// Build turns from events already selected by the cursor index. Their order
/// is authoritative even when checkpoint-copy nodes were removed and the
/// remaining partial slice is therefore not a self-contained parent chain.
pub(super) fn turns_from_selected_session_events(events: &[SessionEvent]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    replay_selected_session_events(events, |ev| apply_event_to_turns(&mut turns, ev));
    turns
}

/// Reconstruct a faithful [`AgentEvent`] stream from a transcript event log,
/// emitting each event immediately so resume never retains a second full
/// transcript representation. The resume and live paths still share one
/// builder ([`App::apply_event`]).
///
/// Tool timings, thinking timings, and native-tool records are gathered first
/// (they are written after the messages) so each `ToolUse` block can be
/// stamped as it is replayed. A `SessionEvent::TurnEnd` becomes the matching
/// `AgentEvent::TurnEnd`, attaching the `◇ Done in Ns with <model>` block to the
/// turn it follows.
///
/// Tool results in the durable log travel as a separate `Message` with
/// `Role::Tool` (or, for legacy entries, `Role::User` carrying `ToolResult`
/// blocks) *after* the assistant's `ToolUse`. The live stream delivers the
/// result inline via `AgentEvent::ToolEnd`, so the replay adapter defers each
/// `ToolEnd` until the matching tool-result message arrives — keeping
/// `apply_event`'s assumption that `ToolEnd` carries the result.
pub(super) fn replay_session_events(events: &[SessionEvent], emit: impl FnMut(AgentEvent)) {
    let visible: Vec<&SessionEvent> = visible_event_indices(events)
        .into_iter()
        .map(|i| &events[i])
        .collect();
    replay_visible_events(&visible, emit);
}

/// Replay events whose lineage and checkpoint-copy filtering was already
/// established by a SessionCursor index. Re-running active-path discovery on
/// one partial turn can lose its prompt when a compaction marker points to a
/// deliberately omitted checkpoint-copy parent.
pub(super) fn replay_selected_session_events(
    events: &[SessionEvent],
    emit: impl FnMut(AgentEvent),
) {
    let visible: Vec<&SessionEvent> = events.iter().collect();
    replay_visible_events(&visible, emit);
}

#[allow(clippy::too_many_lines)]
fn replay_visible_events(visible: &[&SessionEvent], mut emit: impl FnMut(AgentEvent)) {
    use std::collections::HashMap as Map;

    // These indexes borrow the durable events. Cloning native records here
    // used to duplicate every captured tool result during resume (several MiB
    // in large sessions) before replay had even started.
    let mut tool_elapsed: Map<&str, u64> = Map::new();
    let mut native_by_parent: Map<&str, Vec<&NativeToolRecord>> = Map::new();
    // Thinking-block durations in emission order, matched positionally to
    // assistant `Thinking` blocks as they are replayed.
    let mut thinking_timing: Vec<u64> = Vec::new();
    for &ev in visible {
        match &ev.kind {
            SessionEventKind::ToolTiming {
                tool_call_id,
                elapsed_ms,
            } => {
                tool_elapsed.insert(tool_call_id.as_str(), *elapsed_ms);
            }
            SessionEventKind::ThinkingTiming { elapsed_ms } => {
                thinking_timing.push(*elapsed_ms);
            }
            SessionEventKind::Cursor { .. } => {}
            SessionEventKind::NativeTool(rec) => {
                native_by_parent
                    .entry(rec.parent.as_str())
                    .or_default()
                    .push(rec);
            }
            _ => {}
        }
    }
    let mut thinking_idx = 0usize;
    for &ev in visible {
        match &ev.kind {
            SessionEventKind::Message(msg) => match msg.role {
                Role::User => {
                    // ToolResult blocks attach to the current turn's pending
                    // tool calls as deferred `ToolEnd` events.
                    if msg
                        .blocks
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    {
                        for b in &msg.blocks {
                            if let ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } = b
                            {
                                emit(AgentEvent::ToolEnd {
                                    id: tool_use_id.clone(),
                                    result: if *is_error {
                                        format!("error: {content}")
                                    } else {
                                        content.clone()
                                    },
                                    is_error: *is_error,
                                    elapsed_ms: tool_elapsed
                                        .get(tool_use_id.as_str())
                                        .copied()
                                        .unwrap_or(0),
                                });
                            }
                        }
                        continue;
                    }
                    // Otherwise a prompt: start a new turn.
                    let prompt = msg
                        .blocks
                        .iter()
                        .find_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    emit(AgentEvent::TurnStart { prompt });
                }
                Role::Assistant => {
                    for b in &msg.blocks {
                        match b {
                            ContentBlock::Text { text } => {
                                emit(AgentEvent::Text(text.clone()));
                            }
                            ContentBlock::Thinking { text, .. } => {
                                emit(AgentEvent::Thinking(text.clone()));
                                let elapsed =
                                    thinking_timing.get(thinking_idx).copied().unwrap_or(0);
                                thinking_idx += 1;
                                emit(AgentEvent::ThinkingEnd {
                                    elapsed_ms: elapsed,
                                });
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                emit(AgentEvent::ToolStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                });
                                // For a restored `exec`, split the stored
                                // input JSON back into the code (shown with
                                // line numbers) and the `display` label.
                                let (code, label) = if name == "exec" {
                                    lofi_core::exec_input_code_and_label(input)
                                } else {
                                    (input.to_string(), None)
                                };
                                emit(AgentEvent::ToolInput {
                                    id: id.clone(),
                                    code,
                                    label,
                                });
                                // Replay the native tool calls that ran inside
                                // this exec, in order, before the `ToolEnd`
                                // (which is deferred to the tool-result
                                // message below).
                                if let Some(natives) = native_by_parent.get(id.as_str()) {
                                    for rec in natives {
                                        emit(AgentEvent::NativeToolStart {
                                            parent: id.clone(),
                                            id: rec.call_id,
                                            name: rec.name.clone(),
                                            args: rec.args.clone(),
                                        });
                                        emit(AgentEvent::NativeToolEnd {
                                            parent: id.clone(),
                                            id: rec.call_id,
                                            result: rec.result.clone(),
                                            is_error: rec.is_error,
                                        });
                                    }
                                }
                            }
                            ContentBlock::ToolResult { .. } => {}
                        }
                    }
                }
                // The engine writes tool results as `Role::Tool`; replay
                // them as deferred `ToolEnd` events, matching the live
                // stream's inline-result semantics.
                Role::Tool => {
                    for b in &msg.blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = b
                        {
                            emit(AgentEvent::ToolEnd {
                                id: tool_use_id.clone(),
                                result: if *is_error {
                                    format!("error: {content}")
                                } else {
                                    content.clone()
                                },
                                is_error: *is_error,
                                elapsed_ms: tool_elapsed
                                    .get(tool_use_id.as_str())
                                    .copied()
                                    .unwrap_or(0),
                            });
                        }
                    }
                }
                Role::System => {}
            },
            SessionEventKind::UserBash {
                command,
                output,
                exit_code,
                signal,
                duration_ms,
                truncated,
                cancelled,
                exclude_from_context,
            } => emit(AgentEvent::UserBash {
                command: command.clone(),
                output: output.clone(),
                exit_code: *exit_code,
                signal: *signal,
                duration_ms: *duration_ms,
                truncated: *truncated,
                cancelled: *cancelled,
                exclude_from_context: *exclude_from_context,
            }),
            SessionEventKind::NativeTool(_)
            | SessionEventKind::ToolTiming { .. }
            | SessionEventKind::ThinkingTiming { .. } => {}
            SessionEventKind::Compaction {
                summarized,
                kept,
                summary,
                ..
            } => {
                // The summary is injected into the agent history by
                // `messages_from_events`; carry it on the marker block too
                // so `/verbose` can expand it inline.
                emit(AgentEvent::Compaction {
                    summarized: *summarized,
                    kept: *kept,
                    summary: summary.clone(),
                });
            }
            SessionEventKind::TurnEnd {
                model,
                elapsed_ms,
                cost,
                usage,
                ..
            } => {
                emit(AgentEvent::TurnEnd {
                    model: model.clone(),
                    elapsed_ms: *elapsed_ms,
                    cost: *cost,
                    usage: *usage,
                });
            }
            SessionEventKind::TurnFailed {
                model,
                elapsed_ms,
                error,
                cost,
                usage,
                ..
            } => {
                emit(AgentEvent::TurnFailed {
                    model: model.clone(),
                    elapsed_ms: *elapsed_ms,
                    error: error.clone(),
                    cost: *cost,
                    usage: *usage,
                });
            }
            SessionEventKind::Cursor { .. } => {}
        }
    }
}

#[cfg(test)]
pub(super) fn messages_from_events(
    events: &[SessionEvent],
    _edit: &lofi_types::EditConfig,
) -> Vec<Message> {
    let path = store::active_path_from_leaf(events);
    // Kept tail as (event_id, message) pairs so `edit_tail` can embed the
    // event id in its recall-recoverable stubs.
    let mut out: Vec<(String, Message)> = Vec::new();
    let mut skipping = false;
    // The compaction summary, captured when the Compaction marker is seen
    // and prepended to the result so it leads the history (matching
    // `compacted_history`: summary first, then the kept tail). Held aside
    // because the walk is leaf-first; injecting it inline would place the
    // summary after the kept tail once reversed, and mid-stream when a
    // force-continued turn follows the marker.
    let mut summary_msg: Option<Message> = None;
    // The compaction boundary: once a Compaction marker is seen leaf-first,
    // the walk stops at this event id, folding everything older into the
    // summary. `None` while no compaction is in effect on the active path.
    let mut boundary: Option<String> = None;
    // Iterate leaf-first so the `TurnFailed` boundary is seen before its
    // ancestors; `path` is root-first, so reverse.
    for &i in path.iter().rev() {
        match &events[i].kind {
            SessionEventKind::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } => {
                if !summary.is_empty() {
                    summary_msg = Some(Message {
                        role: Role::User,
                        blocks: vec![ContentBlock::Text {
                            text: summary.clone(),
                        }],
                    });
                }
                if first_kept_entry_id.is_empty() {
                    // Compact-all has no kept-tail boundary. Everything older
                    // than this marker is represented by the summary; retain
                    // only messages already collected after the marker.
                    break;
                }
                boundary = Some(first_kept_entry_id.clone());
            }
            SessionEventKind::TurnFailed { .. } => {
                skipping = true;
            }
            SessionEventKind::TurnEnd { .. } => {
                skipping = false;
            }
            SessionEventKind::Message(m) if !skipping => {
                out.push((events[i].id.clone(), m.clone()));
                if let Some(b) = &boundary {
                    if b == &events[i].id {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    // `out` is leaf-first; reverse to root-first for the model.
    // Read verbatim — the transcript is the checkpoint. At compact time the
    // edited kept-tail messages are written to the transcript, so no
    // in-memory edit_tail is needed here.
    out.reverse();
    let mut messages: Vec<Message> = out.into_iter().map(|(_, m)| m).collect();
    // The summary leads: it is the oldest context (the folded prefix), so it
    // must come before the kept tail and any post-compaction continuation.
    if let Some(s) = summary_msg {
        messages.insert(0, s);
    }
    messages
}
