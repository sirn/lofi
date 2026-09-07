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
    ContentBlock, Message, NativeToolRecord, PromptKind, Role, SessionEvent, SessionEventKind,
    Usage,
};

use super::store::{self, IndexId, IndexKind, SessionCursor, SessionIndexEntry};
use crate::agent::AgentEvent;
use crate::exec_input_code_and_label;
use crate::shell::UserShellResult;

/// Marks pre-compaction tail events hidden by checkpointed compaction markers
/// along `path`. `compaction_at` returns the marker's
/// `(checkpointed_tail, first_kept_entry_id)` for the event at the given path
/// position, or `None` when it is not a compaction; `id_matches` reports
/// whether the event at a path position has the given id. Parameterising these
/// two lookups lets one hiding rule serve both the in-memory event slice (kind
/// and id read inline) and the resume index (kind via `event_at`, id via the
/// index's normalized `IndexId`).
///
/// The target id is normalized into an `IndexId` once per marker: a compacted
/// transcript otherwise re-parses the 32-char hex string on every one of the
/// O(path) comparisons per marker, which dominates session-resume time on
/// long, heavily compacted sessions. The first position of each id comes from
/// a lazily built map — only materialized when a checkpointed marker exists —
/// so locating the hide start is O(1) per marker instead of another scan.
/// First-occurrence mapping reproduces the previous `(0..marker_pos).find`
/// walk exactly: both resolve to the smallest position holding the id, and
/// only positions before the marker count.
fn hidden_compaction_range(
    path: &[usize],
    mut compaction_at: impl FnMut(usize) -> Option<(bool, String)>,
    mut id_position: impl FnMut(&IndexId) -> Option<usize>,
) -> Vec<bool> {
    let mut hidden = vec![false; path.len()];
    for marker_pos in 0..path.len() {
        let Some((true, first_kept_entry_id)) = compaction_at(marker_pos) else {
            continue;
        };
        if first_kept_entry_id.is_empty() {
            continue;
        }
        let want = IndexId::borrow(&first_kept_entry_id);
        if let Some(start_pos) = id_position(&want).filter(|&p| p < marker_pos) {
            hidden[start_pos..marker_pos].fill(true);
        }
    }
    hidden
}

#[must_use]
pub fn visible_event_indices(events: &[SessionEvent]) -> Vec<usize> {
    let path = store::active_path_from_leaf(events);
    let mut positions: Option<Map<IndexId, usize>> = None;
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
        |id| {
            positions
                .get_or_insert_with(|| {
                    let mut map = Map::new();
                    for (pos, &i) in path.iter().enumerate() {
                        // First occurrence holds: `insert` overwrites and a
                        // later duplicate must not win the hide start.
                        map.entry(IndexId::borrow(&events[i].id)).or_insert(pos);
                    }
                    map
                })
                .get(id)
                .copied()
        },
    );
    path.into_iter()
        .zip(hidden)
        .filter_map(|(i, hide)| (!hide).then_some(i))
        .collect()
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
    for &ev in visible {
        match &ev.kind {
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
                    if let Some(event) = AgentEvent::from_prompt(msg) {
                        emit(event);
                    }
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
                            ContentBlock::ToolResult { .. }
                            | ContentBlock::PartSignature { .. }
                            | ContentBlock::Image { .. } => {}
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
            SessionEventKind::UserShell {
                command,
                output,
                exit_code,
                signal,
                duration_ms,
                truncated,
                cancelled,
                exclude_from_context,
            } => {
                emit(AgentEvent::UserShell {
                    command: command.clone(),
                    output: output.clone(),
                    exit_code: *exit_code,
                    signal: *signal,
                    duration_ms: *duration_ms,
                    truncated: *truncated,
                    cancelled: *cancelled,
                    exclude_from_context: *exclude_from_context,
                });
            }
            SessionEventKind::RoundDiscarded { .. }
            | SessionEventKind::NativeTool(_)
            | SessionEventKind::ToolTiming { .. }
            | SessionEventKind::ThinkingTiming { .. }
            | SessionEventKind::JobStarted { .. }
            | SessionEventKind::JobFinished { .. }
            | SessionEventKind::Cursor { .. }
            | SessionEventKind::Unknown => {}
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
                stop_reason,
                ..
            } => {
                emit(AgentEvent::TurnEnd {
                    model: model.clone(),
                    elapsed_ms: *elapsed_ms,
                    cost: *cost,
                    usage: *usage,
                    stop_reason: *stop_reason,
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
/// `Message` passes through unchanged; a `UserShell` becomes its context-text
/// user message; excluded bashes and everything else contribute none. Shared
/// by message reconstruction (`messages_from_events`, `compact`) so the
/// bash-to-context transform and exclusion rule live in one place.
#[must_use]
#[allow(clippy::match_same_arms)]
pub fn agent_message_for_event(kind: &SessionEventKind) -> Option<Message> {
    match kind {
        // Lineage bookkeeping only; the model learns about background work
        // from tool results and notices. Kept explicit against the wildcard
        // so the lifecycle rule is visible without scanning the `_` arm.
        SessionEventKind::JobStarted { .. } | SessionEventKind::JobFinished { .. } => None,
        SessionEventKind::Message(m) => Some(m.clone()),
        SessionEventKind::UserShell {
            command,
            output,
            exit_code,
            signal,
            duration_ms,
            truncated,
            cancelled,
            exclude_from_context: false,
        } => {
            let result = UserShellResult::from_session(
                command.clone(),
                output.clone(),
                *exit_code,
                *signal,
                *duration_ms,
                *truncated,
                *cancelled,
            );
            Some(Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: result.context_text(),
                }],
                kind: PromptKind::default(),
            })
        }
        _ => None,
    }
}

#[derive(Default)]
pub(crate) struct LeafFirstContextFilter {
    skipping_failed_turn: bool,
    discarding_assistant: bool,
}

impl LeafFirstContextFilter {
    pub(crate) fn push(&mut self, kind: &SessionEventKind) -> Option<Message> {
        match kind {
            SessionEventKind::Compaction { .. }
            | SessionEventKind::TurnEnd { .. }
            | SessionEventKind::TurnCancelled { .. } => {
                self.skipping_failed_turn = false;
                self.discarding_assistant = false;
                None
            }
            SessionEventKind::TurnFailed { .. } => {
                self.skipping_failed_turn = true;
                self.discarding_assistant = false;
                None
            }
            SessionEventKind::RoundDiscarded { .. } if !self.skipping_failed_turn => {
                self.discarding_assistant = true;
                None
            }
            SessionEventKind::Message(message) if message.role == Role::System => {
                Some(message.clone())
            }
            SessionEventKind::Message(_) | SessionEventKind::UserShell { .. }
                if !self.skipping_failed_turn =>
            {
                let message = agent_message_for_event(kind)?;
                if self.discarding_assistant {
                    if message.role == Role::Assistant {
                        return None;
                    }
                    self.discarding_assistant = false;
                }
                Some(message)
            }
            _ => None,
        }
    }
}

/// Leaf-first durable-event projection into model context. Both complete
/// in-memory replay and the bounded file-backed restore path use this state
/// machine so compaction and system-message boundaries cannot diverge.
#[derive(Default)]
pub(crate) struct AgentHistoryProjection {
    messages: Vec<Message>,
    latest_system: Option<Message>,
    summary: Option<Message>,
    boundary: Option<String>,
    filter: LeafFirstContextFilter,
    done: bool,
}

impl AgentHistoryProjection {
    pub(crate) fn push(&mut self, event: &SessionEvent) {
        if self.done {
            return;
        }
        if let SessionEventKind::Compaction {
            summary,
            first_kept_entry_id,
            ..
        } = &event.kind
        {
            // Checkpoint copies omit TurnEnd, so compaction must end a failed-turn skip.
            self.filter.push(&event.kind);
            if !summary.is_empty() && self.summary.is_none() {
                self.summary = Some(Message {
                    origin: None,
                    role: Role::User,
                    blocks: vec![ContentBlock::Text {
                        text: summary.clone(),
                    }],
                    kind: PromptKind::default(),
                });
            }
            if first_kept_entry_id.is_empty() {
                self.done = true;
            } else {
                self.boundary = Some(first_kept_entry_id.clone());
            }
            return;
        }

        let Some(message) = self.filter.push(&event.kind) else {
            return;
        };
        if message.role == Role::System {
            if self.latest_system.is_none() {
                self.latest_system = Some(message);
            }
            return;
        }
        let boundary_hit = self
            .boundary
            .as_ref()
            .is_some_and(|boundary| boundary == &event.id);
        self.messages.push(message);
        if boundary_hit {
            self.done = true;
        }
    }

    #[must_use]
    pub(crate) fn finish(mut self) -> Vec<Message> {
        self.messages.reverse();
        let mut messages = Vec::with_capacity(
            self.messages.len()
                + usize::from(self.latest_system.is_some())
                + usize::from(self.summary.is_some()),
        );
        messages.extend(self.latest_system);
        messages.extend(self.summary);
        messages.extend(self.messages);
        messages
    }
}

/// Rebuild the agent-visible message history from the durable log along the
/// selected leaf. Mirrors `compacted_history`: current system prompt first,
/// then the compaction summary and retained tail. Failed and discarded rounds
/// remain visible in the transcript but are excluded from this model context.
#[must_use]
pub fn messages_from_events(events: &[SessionEvent]) -> Vec<Message> {
    let path = store::active_path_from_leaf(events);
    let mut projection = AgentHistoryProjection::default();
    for &index in path.iter().rev() {
        projection.push(&events[index]);
    }
    projection.finish()
}

#[must_use]
pub fn visible_index_path(index: &[SessionIndexEntry]) -> Vec<usize> {
    index
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| entry.visible.then_some(position))
        .collect()
}

#[must_use]
pub fn last_run_model_from_index(
    cursor: &SessionCursor,
    index: &[SessionIndexEntry],
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
    index: &[SessionIndexEntry],
) -> (bool, Option<Usage>) {
    // Read only the latest turn outcome: the prior shape fetched every
    // TurnEnd event on the transcript, and each fetch opens and seeks the
    // session file — hundreds of opens on a long resume when only the final
    // usage matters for the status line.
    let last_compaction_pos = index
        .iter()
        .rposition(|event| event.kind == IndexKind::Compaction);
    let last_usage = index
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, event)| {
            matches!(
                event.kind,
                IndexKind::TurnEnd | IndexKind::TurnFailed | IndexKind::TurnCancelled
            )
        })
        .find_map(|(pos, event)| {
            let ev = cursor.event_at(event.offset).ok()?;
            match ev.kind {
                SessionEventKind::TurnEnd { usage, .. }
                | SessionEventKind::TurnFailed { usage, .. }
                | SessionEventKind::TurnCancelled { usage, .. } => Some((pos, usage)),
                _ => None,
            }
        });
    let usage_after_compaction = last_usage
        .filter(|(pos, _)| last_compaction_pos.is_none_or(|compact_pos| *pos > compact_pos))
        .map(|(_, usage)| usage);
    (
        last_compaction_pos.is_some() && usage_after_compaction.is_none(),
        usage_after_compaction,
    )
}

/// Job ids with a `JobStarted` marker but no matching `JobFinished` marker
/// on the visible lineage.
#[must_use]
pub fn outstanding_job_ids(events: &[SessionEvent]) -> Vec<u64> {
    let mut started: Vec<u64> = Vec::new();
    let mut finished: HashSet<u64> = HashSet::new();
    for i in visible_event_indices(events) {
        track_lifecycle(&events[i].kind, &mut started, &mut finished);
    }
    started.retain(|id| !finished.contains(id));
    started
}

/// Like [`outstanding_job_ids`] but reads lifecycle markers directly from a
/// cursor over an indexed lineage; `index` must be lineage-scoped (see
/// `SessionCursor::snapshot`). Only `IndexKind::JobLifecycle` entries are
/// touched.
/// # Errors
/// Propagates cursor I/O failures.
pub fn outstanding_job_ids_at(
    cursor: &SessionCursor,
    index: &[SessionIndexEntry],
) -> lofi_error::Result<Vec<u64>> {
    Ok(job_lifecycle_ids_at(cursor, index)?.outstanding)
}

pub(crate) struct JobLifecycleIds {
    pub started: Vec<u64>,
    pub outstanding: Vec<u64>,
}

pub(crate) fn job_lifecycle_ids_at(
    cursor: &SessionCursor,
    index: &[SessionIndexEntry],
) -> lofi_error::Result<JobLifecycleIds> {
    let mut started: Vec<u64> = Vec::new();
    let mut finished: HashSet<u64> = HashSet::new();
    for entry in index.iter().filter(|e| e.kind == IndexKind::JobLifecycle) {
        let ev = cursor.event_at(entry.offset)?;
        track_lifecycle(&ev.kind, &mut started, &mut finished);
    }
    let outstanding = started
        .iter()
        .copied()
        .filter(|id| !finished.contains(id))
        .collect();
    Ok(JobLifecycleIds {
        started,
        outstanding,
    })
}

fn track_lifecycle(kind: &SessionEventKind, started: &mut Vec<u64>, finished: &mut HashSet<u64>) {
    match kind {
        SessionEventKind::JobStarted { job_id } => started.push(*job_id),
        SessionEventKind::JobFinished { job_id } => {
            finished.insert(*job_id);
        }
        _ => {}
    }
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn user_msg(t: &str) -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: t.into() }],
                kind: PromptKind::default(),
            }),
        }
    }

    fn user_msg_with_kind(t: &str, kind: lofi_types::PromptKind) -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: t.into() }],
                kind,
            }),
        }
    }

    fn chain(events: &mut [SessionEvent]) {
        for (i, ev) in events.iter_mut().enumerate() {
            ev.id = format!("e{i}");
            ev.parent_id = (i > 0).then(|| format!("e{}", i - 1));
        }
    }

    fn job_started(id: u64) -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::JobStarted { job_id: id },
        }
    }

    fn job_finished(id: u64) -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::JobFinished { job_id: id },
        }
    }

    fn assistant_msg(t: &str) -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                origin: None,
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: t.into() }],
                kind: PromptKind::default(),
            }),
        }
    }

    fn round_discarded(detail: &str) -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::RoundDiscarded {
                detail: detail.to_string(),
            },
        }
    }

    fn turn_end() -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "m".into(),
                elapsed_ms: 0,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        }
    }

    fn turn_failed() -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnFailed {
                model: "m".into(),
                elapsed_ms: 0,
                error: "boom".into(),
                cost: 0.0,
                usage: Usage::default(),
            },
        }
    }

    fn turn_cancelled() -> SessionEvent {
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnCancelled {
                model: "m".into(),
                elapsed_ms: 0,
                cost: 0.0,
                usage: Usage::default(),
            },
        }
    }

    #[test]
    fn round_discarded_keeps_content_visible_but_excludes_it_from_context() {
        let mut events = vec![
            user_msg("go"),
            assistant_msg("repeating reasoning"),
            round_discarded("potential agent loop detected"),
            user_msg_with_kind("try again differently", lofi_types::PromptKind::Notice),
            assistant_msg("recovery answer"),
            turn_end(),
        ];
        chain(&mut events);
        let visible = visible_event_indices(&events);
        assert_eq!(
            visible.len(),
            events.len(),
            "the discarded round stays visible"
        );

        let messages = messages_from_events(&events);
        let texts: Vec<&str> = messages
            .iter()
            .map(|message| match &message.blocks[0] {
                ContentBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(texts, ["go", "try again differently", "recovery answer"]);

        let mut replayed = Vec::new();
        replay_session_events(&events, |event| replayed.push(event));
        assert!(replayed.iter().any(|event| matches!(
            event,
            AgentEvent::Prompt { prompt, kind }
                if *kind == PromptKind::Notice && prompt == "try again differently"
        )));
        assert!(
            replayed
                .iter()
                .all(|event| !matches!(event, AgentEvent::Notice(_))),
            "discard metadata must not become a transient notification"
        );
    }

    #[test]
    fn consecutive_round_discards_chain() {
        let mut events = vec![
            user_msg("go"),
            assistant_msg("first loop"),
            round_discarded("first discard"),
            user_msg_with_kind("notice", lofi_types::PromptKind::Notice),
            assistant_msg("second loop"),
            assistant_msg("still looping"),
            round_discarded("second discard"),
            user_msg_with_kind("notice2", lofi_types::PromptKind::Notice),
            assistant_msg("done answer"),
            turn_end(),
        ];
        chain(&mut events);
        let messages = messages_from_events(&events);
        let texts: Vec<&str> = messages
            .iter()
            .map(|message| match &message.blocks[0] {
                ContentBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(
            texts,
            ["go", "notice", "notice2", "done answer"],
            "each marker drops the assistant messages it ends with"
        );
    }

    #[test]
    fn discarded_round_in_failed_turn_does_not_hide_prior_reply() {
        let mut events = vec![
            user_msg("previous prompt"),
            assistant_msg("previous reply"),
            turn_end(),
            user_msg("failed prompt"),
            assistant_msg("discarded attempt"),
            round_discarded("loop"),
            user_msg_with_kind("retry notice", lofi_types::PromptKind::Notice),
            assistant_msg("failed partial"),
            turn_failed(),
        ];
        chain(&mut events);

        let messages = messages_from_events(&events);
        let texts: Vec<&str> = messages
            .iter()
            .map(|message| match &message.blocks[0] {
                ContentBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .collect();

        assert_eq!(texts, ["previous prompt", "previous reply"]);
    }

    #[test]
    fn failed_turn_does_not_hide_prior_cancelled_turn() {
        let mut events = vec![
            user_msg("cancelled prompt"),
            assistant_msg("cancelled partial"),
            turn_cancelled(),
            user_msg("failed prompt"),
            assistant_msg("failed partial"),
            turn_failed(),
        ];
        chain(&mut events);

        let messages = messages_from_events(&events);
        let texts: Vec<&str> = messages
            .iter()
            .map(|message| match &message.blocks[0] {
                ContentBlock::Text { text } => text.as_str(),
                _ => "",
            })
            .collect();

        assert_eq!(texts, ["cancelled prompt", "cancelled partial"]);
    }

    #[test]
    fn outstanding_job_ids_returns_empty_on_no_markers() {
        let mut events = vec![user_msg("hello")];
        chain(&mut events);
        assert!(outstanding_job_ids(&events).is_empty());
    }

    #[test]
    fn outstanding_job_ids_returns_unfinished_starts() {
        let mut events = vec![
            user_msg("a"),
            job_started(1),
            job_started(2),
            job_finished(1),
            user_msg("b"),
        ];
        chain(&mut events);
        assert_eq!(outstanding_job_ids(&events), vec![2]);
    }

    #[test]
    fn outstanding_job_ids_returns_all_when_none_finished() {
        let mut events = vec![job_started(7), user_msg("x"), job_started(8)];
        chain(&mut events);
        assert_eq!(outstanding_job_ids(&events), vec![7, 8]);
    }

    #[test]
    fn outstanding_job_ids_returns_empty_when_all_finished() {
        let mut events = vec![job_started(3), job_finished(3), user_msg("done")];
        chain(&mut events);
        assert!(outstanding_job_ids(&events).is_empty());
    }

    #[test]
    fn job_lifecycle_markers_do_not_map_to_ir() {
        assert!(agent_message_for_event(&SessionEventKind::JobStarted { job_id: 1 }).is_none());
        assert!(agent_message_for_event(&SessionEventKind::JobFinished { job_id: 1 }).is_none());
    }

    #[test]
    fn replay_suppresses_job_lifecycle_markers() {
        let events = vec![
            user_msg("a"),
            job_started(1),
            job_finished(1),
            user_msg("b"),
        ];
        let mut prompts: Vec<String> = Vec::new();
        replay_selected_session_events(&events, |ev| {
            if let AgentEvent::Prompt { prompt, .. } = ev {
                prompts.push(prompt);
            }
        });
        assert_eq!(prompts, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn replay_restores_turn_kind_from_message_kind_field() {
        let events = vec![
            user_msg("typed"),
            user_msg_with_kind("job 1 completed", lofi_types::PromptKind::Notice),
            user_msg("typed again"),
        ];
        let mut kinds: Vec<lofi_types::PromptKind> = Vec::new();
        replay_selected_session_events(&events, |ev| {
            if let AgentEvent::Prompt { kind, .. } = ev {
                kinds.push(kind);
            }
        });
        assert_eq!(
            kinds,
            vec![
                lofi_types::PromptKind::User,
                lofi_types::PromptKind::Notice,
                lofi_types::PromptKind::User,
            ],
            "kind travels with each user message and resets to User"
        );
    }

    #[test]
    fn replay_does_not_guess_live_run_boundaries() {
        let events = vec![
            user_msg("first"),
            assistant_msg("partial"),
            user_msg("second"),
        ];
        let mut prompts = Vec::new();
        replay_selected_session_events(&events, |event| match event {
            AgentEvent::RunStart => panic!("run starts are live-only"),
            AgentEvent::Prompt { prompt, .. } => prompts.push(prompt),
            _ => {}
        });
        assert_eq!(prompts, ["first", "second"]);
    }
}
