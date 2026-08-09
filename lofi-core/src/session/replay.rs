//! `SessionEvent` → `AgentEvent` replay mapping. Owns the durable-log-to-live-
//! stream translation so the TUI only applies the resulting `AgentEvent`s to
//! its widgets. Tool results in the durable log travel as a separate `Message`
//! with `Role::Tool` (or, for legacy entries, `Role::User` carrying
//! `ToolResult` blocks) *after* the assistant's `ToolUse`. The live stream
//! delivers the result inline via `AgentEvent::ToolEnd`, so the replay adapter
//! defers each `ToolEnd` until the matching tool-result message arrives.

use std::collections::HashMap as Map;
use std::collections::HashSet;

use lofi_types::{
    ContentBlock, Message, NativeToolRecord, Role, SessionEvent, SessionEventKind, Usage,
};

use super::store::{self, EventIndex, IndexKind, SessionCursor};
use crate::agent::AgentEvent;
use crate::exec_input_code_and_label;
use crate::user_bash::UserBashResult;

/// Positions of pre-compaction tail events hidden by checkpointed compaction
/// markers along `path`. `compaction_at` returns the marker's
/// `(checkpointed_tail, first_kept_entry_id)` for the event at the given path
/// position, or `None` when it is not a compaction; `id_matches` reports
/// whether the event at a path position has the given id. Parameterising these
/// two lookups lets one hiding rule serve both the in-memory event slice (kind
/// and id read inline) and the resume index (kind via `event_at`, id via the
/// index's normalized `IndexId`).
fn hidden_compaction_range(
    path: &[usize],
    mut compaction_at: impl FnMut(usize) -> Option<(bool, String)>,
    mut id_matches: impl FnMut(usize, &str) -> bool,
) -> HashSet<usize> {
    let mut hidden = HashSet::new();
    for marker_pos in 0..path.len() {
        let Some((true, first_kept_entry_id)) = compaction_at(marker_pos) else {
            continue;
        };
        if first_kept_entry_id.is_empty() {
            continue;
        }
        if let Some(start_pos) = (0..marker_pos).find(|&p| id_matches(p, &first_kept_entry_id)) {
            hidden.extend(start_pos..marker_pos);
        }
    }
    hidden
}

#[must_use]
pub fn visible_event_indices(events: &[SessionEvent]) -> Vec<usize> {
    let path = store::active_path_from_leaf(events);
    let hidden = hidden_compaction_range(
        &path,
        |p| match &events[path[p]].kind {
            SessionEventKind::Compaction {
                checkpointed_tail,
                first_kept_entry_id,
                ..
            } => Some((*checkpointed_tail, first_kept_entry_id.clone())),
            _ => None,
        },
        |p, id| events[path[p]].id == id,
    );
    path.into_iter().filter(|i| !hidden.contains(i)).collect()
}

pub fn replay_session_events(events: &[SessionEvent], emit: impl FnMut(AgentEvent)) {
    let visible: Vec<&SessionEvent> = visible_event_indices(events)
        .into_iter()
        .map(|i| &events[i])
        .collect();
    replay_visible_events(&visible, emit);
}

pub fn replay_selected_session_events(events: &[SessionEvent], emit: impl FnMut(AgentEvent)) {
    let visible: Vec<&SessionEvent> = events.iter().collect();
    replay_visible_events(&visible, emit);
}

#[allow(clippy::too_many_lines)]
fn replay_visible_events(visible: &[&SessionEvent], mut emit: impl FnMut(AgentEvent)) {
    let mut tool_elapsed: Map<&str, u64> = Map::new();
    let mut native_by_parent: Map<&str, Vec<&NativeToolRecord>> = Map::new();
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
    // Pending kind for the next user-message turn. Populated by a
    // SessionEventKind::TurnPrompt marker (recorder emits one only for
    // non-User kinds, so typed input has no marker and the default applies).
    let mut next_turn_kind = lofi_types::PromptKind::User;
    for &ev in visible {
        match &ev.kind {
            SessionEventKind::TurnPrompt { kind } => {
                next_turn_kind = *kind;
            }
            SessionEventKind::Message(msg) => match msg.role {
                Role::User => {
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
                                ..
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
                    emit(AgentEvent::TurnStart {
                        prompt,
                        kind: next_turn_kind,
                    });
                    // Reset for the next turn; User remains the default when
                    // no marker precedes the next user message.
                    next_turn_kind = lofi_types::PromptKind::User;
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
                                let (code, label) = if name == "exec" {
                                    exec_input_code_and_label(input)
                                } else {
                                    (input.to_string(), None)
                                };
                                emit(AgentEvent::ToolInput {
                                    id: id.clone(),
                                    code,
                                    label,
                                });
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
                            ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
                        }
                    }
                }
                Role::Tool => {
                    for b in &msg.blocks {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
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
            | SessionEventKind::ThinkingTiming { .. }
            | SessionEventKind::Cursor { .. } => {}
            SessionEventKind::Compaction {
                summarized,
                kept,
                summary,
                ..
            } => {
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
            SessionEventKind::TurnCancelled {
                model,
                elapsed_ms,
                cost,
                usage,
            } => {
                emit(AgentEvent::TurnCancelled {
                    model: model.clone(),
                    elapsed_ms: *elapsed_ms,
                    cost: *cost,
                    usage: *usage,
                });
            }
        }
    }
}

/// The agent-visible message a session event contributes, if any: a chat
/// `Message` passes through unchanged; a `UserBash` becomes its context-text
/// user message; excluded bashes and everything else contribute none. Shared
/// by message reconstruction (`messages_from_events`, `compact`) so the
/// bash-to-context transform and exclusion rule live in one place.
#[must_use]
pub fn agent_message_for_event(kind: &SessionEventKind) -> Option<Message> {
    match kind {
        SessionEventKind::Message(m) => Some(m.clone()),
        SessionEventKind::UserBash {
            command,
            output,
            exit_code,
            signal,
            duration_ms,
            truncated,
            cancelled,
            exclude_from_context: false,
        } => {
            let result = UserBashResult::from_session(
                command.clone(),
                output.clone(),
                *exit_code,
                *signal,
                *duration_ms,
                *truncated,
                *cancelled,
            );
            Some(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: result.context_text(),
                }],
            })
        }
        _ => None,
    }
}

/// Rebuild the agent-visible message history from the durable log along the
/// selected leaf. Mirrors `compacted_history`: compaction summary first, then
/// the kept tail, stopping at a `TurnFailed` boundary.
#[must_use]
pub fn messages_from_events(events: &[SessionEvent]) -> Vec<Message> {
    let path = store::active_path_from_leaf(events);
    let mut out: Vec<(String, Message)> = Vec::new();
    let mut skipping = false;
    // The compaction summary is captured when the Compaction marker is seen
    // and prepended to the result so it leads the history. Held aside because
    // the walk is leaf-first; injecting it inline would place the summary
    // after the kept tail once reversed, and mid-stream when a force-continued
    // turn follows the marker.
    let mut summary_msg: Option<Message> = None;
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
            SessionEventKind::Message(_) | SessionEventKind::UserBash { .. } if !skipping => {
                let Some(message) = agent_message_for_event(&events[i].kind) else {
                    continue;
                };
                let boundary_hit = boundary.as_ref().is_some_and(|b| b == &events[i].id);
                out.push((events[i].id.clone(), message));
                if boundary_hit {
                    break;
                }
            }
            _ => {}
        }
    }
    out.reverse();
    let mut messages: Vec<Message> = out.into_iter().map(|(_, m)| m).collect();
    if let Some(s) = summary_msg {
        messages.insert(0, s);
    }
    messages
}

/// Index-driven projections for resume and the status line. All operate on the
/// lightweight index plus targeted `event_at` reads, so a resumed session's
/// memory stays bounded by projection size, not transcript size.
#[must_use]
pub fn visible_index_path(cursor: &SessionCursor, index: &[EventIndex]) -> Vec<usize> {
    // The resume projection spans the whole index, so the path is 0..len and
    // hidden positions are index positions directly.
    let all: Vec<usize> = (0..index.len()).collect();
    let hidden = hidden_compaction_range(
        &all,
        |p| {
            (index[p].kind == IndexKind::Compaction).then(|| {
                let (_, _, checkpointed_tail, first_kept_entry_id) =
                    cursor.compaction_details_at(index[p].offset);
                (checkpointed_tail, first_kept_entry_id)
            })
        },
        |p, id| index[p].id.matches(id),
    );
    (0..index.len()).filter(|i| !hidden.contains(i)).collect()
}

#[must_use]
pub fn last_run_model_from_index(
    cursor: &SessionCursor,
    index: &[EventIndex],
) -> Option<lofi_types::RunModel> {
    for i in (0..index.len()).rev() {
        if !matches!(
            index[i].kind,
            IndexKind::TurnEnd | IndexKind::TurnFailed | IndexKind::TurnCancelled
        ) {
            continue;
        }
        let ev = cursor.event_at(index[i].offset).ok()?;
        if let Some(model) = store::turn_outcome_model(&ev.kind) {
            return Some(model.clone());
        }
    }
    None
}

/// Whether the session was compacted with no usage recorded after the marker
/// (so the status line shows the compacted banner) plus the latest usage.
#[must_use]
pub fn compaction_status_from_index(
    cursor: &SessionCursor,
    index: &[EventIndex],
) -> (bool, Option<Usage>) {
    let mut last_compaction_pos = None;
    let mut last_usage = None;
    for (pos, event) in index.iter().enumerate() {
        match event.kind {
            IndexKind::Compaction => last_compaction_pos = Some(pos),
            IndexKind::TurnEnd | IndexKind::TurnFailed | IndexKind::TurnCancelled => {
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
    (
        last_compaction_pos.is_some() && usage_after_compaction.is_none(),
        usage_after_compaction,
    )
}

/// Byte ranges per turn, derived from the visible event path. Test helper
/// shared with the TUI's resume projection.
#[must_use]
pub fn turn_byte_ranges_from_events(
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
