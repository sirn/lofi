//! Turn/Block shaping over the core session replay stream. The
//! `SessionEvent` → `AgentEvent` mapping lives in
//! `lofi_core::session::replay`; this module only applies the resulting
//! `AgentEvent`s to the TUI's `Turn`/`Block` model.

#![allow(clippy::wildcard_imports)]

use super::*;

#[cfg(test)]
pub(super) use lofi_core::session::replay::{messages_from_events, turn_byte_ranges_from_events};
pub(super) use lofi_core::session::replay::{
    replay_selected_session_events, replay_session_events,
};

/// Byte budget for live user-shell accumulation in [`apply_event_to_turns`];
/// mirrors core's 64 KiB capture tail.
const LIVE_TAIL_BYTES: usize = 64 * 1024;

/// The last turn ends in a still-running user-shell block (live streaming
/// only — replay and finalize-in-place never leave one behind).
pub(super) fn user_shell_running(turns: &[Turn]) -> bool {
    matches!(
        turns.last().and_then(|turn| turn.blocks.last()),
        Some(Block::UserShell { running: true, .. })
    )
}

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

fn settle_open_tools(turn: &mut Turn, result: &str, elapsed_ms: u64) {
    for block in &mut turn.blocks {
        if let Block::Tool(tool) = block {
            if !tool.done {
                for native in &mut tool.native {
                    if !native.done {
                        native.result = Some(result.to_string());
                        native.is_error = true;
                        native.done = true;
                    }
                }
                tool.result = Some(result.to_string());
                tool.is_error = true;
                tool.done = true;
                tool.elapsed = Some(Duration::from_millis(elapsed_ms));
            }
        }
    }
}

/// The turn-building core of [`App::apply_event`], free of `App`-owned state
/// (cost/usage totals, retry indicator, byte ranges). Shared by the live
/// `apply_event` and by [`turns_from_session_events`] (used to materialize a
/// frozen turn from its byte range on demand) so there is exactly one place
/// that maps an `AgentEvent` to `Block`s.
#[allow(clippy::too_many_lines)]
pub(super) fn apply_event_to_turns(turns: &mut Vec<Turn>, ev: AgentEvent) {
    if let AgentEvent::TurnStart { prompt, kind } = ev {
        turns.push(Turn {
            prompt,
            kind,
            blocks: Vec::new(),
        });
        return;
    }
    if let AgentEvent::UserShellStart {
        command,
        exclude_from_context,
    } = ev
    {
        turns.push(Turn {
            prompt: String::new(),
            kind: lofi_types::PromptKind::User,
            blocks: vec![Block::UserShell {
                id: detail_block_id(turns.len(), 0),
                command,
                output: String::new(),
                exit_code: None,
                signal: None,
                duration: Duration::ZERO,
                truncated: false,
                cancelled: false,
                running: true,
                exclude_from_context,
            }],
        });
        return;
    }
    if let AgentEvent::UserShellDelta(delta) = ev {
        if let Some(Block::UserShell {
            output,
            running: true,
            ..
        }) = turns.last_mut().and_then(|turn| turn.blocks.last_mut())
        {
            output.push_str(&delta);
            // Bound live accumulation the way the final event's captured
            // tail does: past this size only the newest bytes stay visible.
            if output.len() > LIVE_TAIL_BYTES {
                let mut cut = output.len() - LIVE_TAIL_BYTES;
                while !output.is_char_boundary(cut) {
                    cut += 1;
                }
                output.drain(..cut);
            }
        }
        return;
    }
    if let AgentEvent::UserShell {
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
        if user_shell_running(turns) {
            // Live stream finalize: same turn, same detail id. The captured
            // tail replaces the accumulated chunks so the rendered block
            // matches what the session file records.
            if let Some(Block::UserShell {
                output: block_output,
                exit_code: block_exit_code,
                signal: block_signal,
                duration: block_duration,
                truncated: block_truncated,
                cancelled: block_cancelled,
                running,
                ..
            }) = turns.last_mut().and_then(|turn| turn.blocks.last_mut())
            {
                *block_output = output;
                *block_exit_code = exit_code;
                *block_signal = signal;
                *block_duration = Duration::from_millis(duration_ms);
                *block_truncated = truncated;
                *block_cancelled = cancelled;
                *running = false;
            }
        } else {
            turns.push(Turn {
                prompt: String::new(),
                kind: lofi_types::PromptKind::User,
                blocks: vec![Block::UserShell {
                    id: detail_block_id(turns.len(), 0),
                    command,
                    output,
                    exit_code,
                    signal,
                    duration: Duration::from_millis(duration_ms),
                    truncated,
                    cancelled,
                    running: false,
                    exclude_from_context,
                }],
            });
        }
        return;
    }
    let turn_index = turns.len().saturating_sub(1);
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
                detail_id: detail_block_id(turn_index, turn.blocks.len()),
                id,
                name,
                input: String::new(),
                label: None,
                native: Vec::new(),
                result: None,
                result_availability: ResultAvailability::None,
                result_committed: false,
                is_error: false,
                done: false,
                elapsed: None,
            }));
        }
        AgentEvent::ToolInput { id, code, label } => {
            if let Some(t) = tool_mut(&mut turn.blocks, &id) {
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
        AgentEvent::ToolEnd {
            id,
            result,
            is_error,
            elapsed_ms,
        } => {
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
                if !result.is_empty() {
                    t.result_availability = ResultAvailability::Available;
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
                    preview: None,
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
        AgentEvent::TurnEnd {
            model, elapsed_ms, ..
        } => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::TurnEnd {
                label: model.label(),
                elapsed: Duration::from_millis(elapsed_ms),
            });
        }
        AgentEvent::TurnFailed {
            model,
            elapsed_ms,
            error,
            ..
        } => {
            finalize_open_thinking(turn);
            settle_open_tools(turn, &error, elapsed_ms);
            turn.blocks.push(Block::TurnFailed {
                label: model.label(),
                elapsed: Duration::from_millis(elapsed_ms),
                error,
            });
        }
        AgentEvent::TurnCancelled {
            model, elapsed_ms, ..
        } => {
            finalize_open_thinking(turn);
            settle_open_tools(turn, "Operation aborted", elapsed_ms);
            turn.blocks.push(Block::TurnCancelled {
                label: model.label(),
                elapsed: Duration::from_millis(elapsed_ms),
            });
        }
        AgentEvent::Error(msg) => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::Error(msg));
        }
        AgentEvent::Compaction {
            summarized,
            kept,
            summary,
        } => {
            turn.blocks.push(Block::Compaction {
                summarized,
                kept,
                summary,
            });
        }
        AgentEvent::RetryStart { .. }
        | AgentEvent::RetryEnd { .. }
        | AgentEvent::RoundCommitted { .. }
        | AgentEvent::TurnCommitted { .. }
        | AgentEvent::RoundUsage { .. }
        | AgentEvent::TurnStart { .. }
        | AgentEvent::UserShellStart { .. }
        | AgentEvent::UserShellDelta(_)
        | AgentEvent::UserShell { .. }
        | AgentEvent::TurnContinue
        | AgentEvent::Notice(_)
        | AgentEvent::ContextPressure { .. } => {}
    }
}

pub(super) fn turns_from_session_events(events: &[SessionEvent]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    replay_session_events(events, |ev| apply_event_to_turns(&mut turns, ev));
    turns
}

pub(super) fn turns_from_selected_session_events(events: &[SessionEvent]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    replay_selected_session_events(events, |ev| apply_event_to_turns(&mut turns, ev));
    turns
}
