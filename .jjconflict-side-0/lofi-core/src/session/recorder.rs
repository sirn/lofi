//! Durable session writer.
//!
//! This module shapes durable `SessionEvent`s. [`SessionCursor::record`]
//! handles ordinary session writes. The job helpers bind acquisition and
//! release markers to one lineage owner. [`SessionRecorder`] tracks turn
//! deltas and records a [`SessionRecord::Turn`] batch after each settled round.

use std::{collections::HashSet, path::Path};

use lofi_types::{
    Message, NativeToolRecord, Role, RunModel, SessionEvent, SessionEventKind, Usage,
};

use crate::session::store::{self, CompactionCounts};
use crate::shell::UserShellResult;
use lofi_error::Result;

#[derive(Debug, Clone)]
pub enum TurnOutcome {
    Finished,
    Failed(String),
    /// The run was force-stopped at the hard context cap mid-turn. The
    /// partial turn's messages and timings are written (so a following
    /// compaction can keep the latest turn verbatim and `messages_from_events`
    /// includes them), but NO terminal marker is written — the turn did not
    /// finish or fail, and the UI is signaled via a live `ContextPressure`
    /// event instead. Behaves like [`Detached`] for the empty-input
    /// short-circuit (no terminal marker).
    ContextPressure,
    /// Explicit user interruption. Unlike a failure, its messages remain in
    /// model context; the terminal marker records the aborted status.
    Cancelled,
    /// The event consumer disappeared (for example, a broken stdout pipe).
    /// Preserve any already-built data but do not stamp a user cancellation.
    Detached,
}

#[derive(Debug, Clone)]
pub struct TurnSummary {
    pub elapsed_ms: u64,
    pub cost: f64,
    pub usage: Usage,
    pub stop_reason: Option<lofi_types::StopReason>,
    pub tool_elapsed: Vec<(String, u64)>,
    pub thinking_elapsed: Vec<u64>,
    pub native_tools: Vec<NativeToolRecord>,
}

/// One durable session write. Callers never build a [`SessionEvent`].
pub enum SessionRecord<'a> {
    System {
        prompt: &'a str,
    },
    UserShell {
        result: &'a UserShellResult,
        exclude_from_context: bool,
    },
    Compaction {
        kept_messages: &'a [Message],
        summary: &'a str,
        summarized_range: &'a [String; 2],
        counts: CompactionCounts,
        system_prompt: &'a str,
    },
    /// Durable mid-turn boundary: the round it ends was dropped from the
    /// model history but stays visible on the lineage.
    RoundDiscarded {
        detail: &'a str,
    },
    /// Incremental turn suffix. Built only by [`SessionRecorder`].
    Turn(TurnBatch<'a>),
}

/// New messages and timings for one checkpoint.
pub struct TurnBatch<'a> {
    messages: &'a [Message],
    native_tools: &'a [NativeToolRecord],
    tool_elapsed: Vec<(&'a String, &'a u64)>,
    thinking_elapsed: &'a [u64],
    terminal: Option<SessionEventKind>,
}

impl store::SessionCursor {
    /// Persist a queued prompt the app dropped without running, as a turn
    /// the process died before serving: the prompt message with no
    /// terminal marker.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn record_unrun_prompt(&self, prompt: &str, kind: lofi_types::PromptKind) -> Result<()> {
        let message = Message {
            role: Role::User,
            blocks: vec![lofi_types::ContentBlock::Text {
                text: prompt.to_string(),
            }],
            kind,
        };
        let batch = TurnBatch {
            messages: std::slice::from_ref(&message),
            native_tools: &[],
            tool_elapsed: Vec::new(),
            thinking_elapsed: &[],
            terminal: None,
        };
        self.record(SessionRecord::Turn(batch)).map(|_| ())
    }

    /// Record a job acquisition and return its durable event id. The id owns
    /// the corresponding release marker and remains stable across branch
    /// switches.
    pub(crate) fn record_job_started(&self, job_id: u64) -> Result<String> {
        let mut events = [SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::JobStarted { job_id },
        }];
        self.append_events(&mut events)?;
        Ok(events[0].id.clone())
    }

    /// Record a job release only if its acquisition is still on the selected
    /// lineage. A job that finishes after a branch switch cannot attach its
    /// release marker to the replacement branch.
    pub(crate) fn record_job_finished(&self, owner_event_id: &str, job_id: u64) -> Result<()> {
        let mut events = [SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::JobFinished { job_id },
        }];
        self.append_events_if_ancestor(owner_event_id, &mut events)?;
        Ok(())
    }

    /// Persist one ordinary session record.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn record(&self, record: SessionRecord<'_>) -> Result<(u64, u64)> {
        match record {
            SessionRecord::RoundDiscarded { detail } => {
                let mut events = [SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: SessionEventKind::RoundDiscarded {
                        detail: detail.to_string(),
                    },
                }];
                self.append_events(&mut events)
            }
            SessionRecord::System { prompt } => self.append_system(prompt),
            SessionRecord::UserShell {
                result,
                exclude_from_context,
            } => {
                let mut events = [SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: SessionEventKind::UserShell {
                        command: result.command.clone(),
                        output: result.output.clone(),
                        exit_code: result.exit_code,
                        signal: result.signal,
                        duration_ms: result.duration_ms,
                        truncated: result.truncated,
                        cancelled: result.cancelled,
                        exclude_from_context,
                    },
                }];
                self.append_events(&mut events)
            }
            SessionRecord::Compaction {
                kept_messages,
                summary,
                summarized_range,
                counts,
                system_prompt,
            } => {
                let (start, end) =
                    self.append_compaction(kept_messages, summary, summarized_range, counts)?;
                if system_prompt.is_empty() {
                    return Ok((start, end));
                }
                let (_, sys_end) = self.append_system(system_prompt)?;
                Ok((start, sys_end))
            }
            SessionRecord::Turn(batch) => record_turn(self, batch),
        }
    }
}

fn record_turn(cursor: &store::SessionCursor, batch: TurnBatch<'_>) -> Result<(u64, u64)> {
    let mut events = Vec::new();
    events.extend(batch.messages.iter().cloned().map(|message| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::Message(message),
    }));
    events.extend(
        batch
            .native_tools
            .iter()
            .cloned()
            .map(|record| SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::NativeTool(record),
            }),
    );
    for (id, elapsed_ms) in &batch.tool_elapsed {
        events.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::ToolTiming {
                tool_call_id: (*id).clone(),
                elapsed_ms: **elapsed_ms,
            },
        });
    }
    events.extend(
        batch
            .thinking_elapsed
            .iter()
            .copied()
            .map(|elapsed_ms| SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::ThinkingTiming { elapsed_ms },
            }),
    );
    if let Some(kind) = batch.terminal {
        events.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind,
        });
    }
    if events.is_empty() {
        return Ok((0, 0));
    }
    cursor.append_events(&mut events)
}

#[derive(Debug)]
pub struct SessionRecorder {
    cursor: store::SessionCursor,
    model: RunModel,
    flushed: bool,
    message_count: usize,
    /// Counted messages the engine later removed from the in-memory
    /// history: the suffix slice start is reduced by this many so a
    /// durable removal cannot shift what later checkpoints record.
    displaced: usize,
    native_tool_count: usize,
    thinking_timing_count: usize,
    tool_timing_ids: HashSet<String>,
    byte_start: Option<u64>,
    byte_end: Option<u64>,
}

impl SessionRecorder {
    #[must_use]
    pub fn new(cursor: store::SessionCursor, model: RunModel) -> Self {
        Self {
            cursor,
            model,
            flushed: false,
            message_count: 0,
            displaced: 0,
            native_tool_count: 0,
            thinking_timing_count: 0,
            tool_timing_ids: HashSet::new(),
            byte_start: None,
            byte_end: None,
        }
    }

    /// Stamp the durable boundary for a checkpointed round being dropped
    /// from the model history.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn discard_round(&mut self, detail: &str) -> Result<()> {
        self.cursor
            .record(SessionRecord::RoundDiscarded { detail })?;
        Ok(())
    }

    /// Account for a counted message the engine removed from memory.
    pub fn note_displaced(&mut self, removed: usize) {
        self.displaced += removed;
    }

    /// Append everything completed since the previous checkpoint, without a
    /// terminal marker. Called after each provider/tool round so a long turn
    /// is durable before the whole agent loop settles.
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn checkpoint(
        &mut self,
        messages: &[Message],
        summary: &TurnSummary,
    ) -> Result<Option<(u64, u64)>> {
        if self.flushed {
            return Ok(None);
        }
        self.append_pending(messages, summary, None)
    }

    /// Flush the remaining durable data and terminal marker for this turn.
    /// Messages/timings already written by [`checkpoint`](Self::checkpoint)
    /// are not duplicated.
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn flush(
        &mut self,
        messages: &[Message],
        outcome: &TurnOutcome,
        summary: &TurnSummary,
    ) -> Result<Option<(u64, u64)>> {
        if self.flushed {
            return Ok(None);
        }
        self.flushed = true;
        let terminal = match outcome {
            TurnOutcome::Finished => Some(SessionEventKind::TurnEnd {
                model: self.model.clone(),
                elapsed_ms: summary.elapsed_ms,
                cost: summary.cost,
                usage: summary.usage,
                stop_reason: summary.stop_reason,
            }),
            TurnOutcome::Failed(error) => Some(SessionEventKind::TurnFailed {
                model: self.model.clone(),
                elapsed_ms: summary.elapsed_ms,
                error: error.clone(),
                cost: summary.cost,
                usage: summary.usage,
            }),
            TurnOutcome::Cancelled => Some(SessionEventKind::TurnCancelled {
                model: self.model.clone(),
                elapsed_ms: summary.elapsed_ms,
                cost: summary.cost,
                usage: summary.usage,
            }),
            TurnOutcome::ContextPressure | TurnOutcome::Detached => None,
        };
        self.append_pending(messages, summary, terminal)?;
        Ok(self.byte_start.zip(self.byte_end))
    }

    fn append_pending(
        &mut self,
        messages: &[Message],
        summary: &TurnSummary,
        terminal: Option<SessionEventKind>,
    ) -> Result<Option<(u64, u64)>> {
        let slice_from = self.message_count.saturating_sub(self.displaced);
        let new_messages = messages.get(slice_from..).unwrap_or_default();
        let new_native = summary
            .native_tools
            .get(self.native_tool_count..)
            .unwrap_or_default();
        let new_thinking = summary
            .thinking_elapsed
            .get(self.thinking_timing_count..)
            .unwrap_or_default();
        let new_tool_timings: Vec<(&String, &u64)> = summary
            .tool_elapsed
            .iter()
            .filter(|(id, _)| !self.tool_timing_ids.contains(id))
            .map(|(id, elapsed_ms)| (id, elapsed_ms))
            .collect();
        let new_tool_ids: Vec<String> = new_tool_timings
            .iter()
            .map(|(id, _)| (*id).clone())
            .collect();
        let (start, end) = self.cursor.record(SessionRecord::Turn(TurnBatch {
            messages: new_messages,
            native_tools: new_native,
            tool_elapsed: new_tool_timings,
            thinking_elapsed: new_thinking,
            terminal,
        }))?;
        if end <= start {
            return Ok(None);
        }
        self.message_count = messages.len();
        self.displaced = 0;
        self.native_tool_count = summary.native_tools.len();
        self.thinking_timing_count = summary.thinking_elapsed.len();
        self.tool_timing_ids.extend(new_tool_ids);
        self.byte_start.get_or_insert(start);
        self.byte_end = Some(end);
        Ok(Some((start, end)))
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        self.cursor.path()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use lofi_types::{ContentBlock, PromptKind, Role};
    use tempfile::tempdir;

    fn user_msg(t: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: t.into() }],
            kind: PromptKind::default(),
        }
    }

    fn assistant_text(t: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: t.into() }],
            kind: PromptKind::default(),
        }
    }

    fn header() -> &'static [u8] {
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n"
    }

    fn summary(elapsed_ms: u64) -> TurnSummary {
        TurnSummary {
            elapsed_ms,
            cost: 0.01,
            usage: Usage::default(),
            tool_elapsed: vec![("t1".into(), 7)],
            thinking_elapsed: vec![12],
            native_tools: vec![NativeToolRecord {
                parent: "t1".into(),
                call_id: 0,
                name: "bash".into(),
                args: "ls".into(),
                result: "file".into(),
                is_error: false,
            }],
            stop_reason: None,
        }
    }

    #[test]
    fn flush_writes_messages_timings_and_turn_end_in_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut rec = SessionRecorder::new(cursor.clone(), "m".into());
        let messages = vec![
            user_msg("go"),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "exec".into(),
                    input: serde_json::Value::String("1".into()),
                }],
                kind: PromptKind::default(),
            },
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "2".into(),
                    is_error: false,
                    images: Vec::new(),
                }],
                kind: PromptKind::default(),
            },
        ];
        let range = rec
            .flush(&messages, &TurnOutcome::Finished, &summary(100))
            .unwrap()
            .expect("wrote something");
        assert!(rec
            .flush(&messages, &TurnOutcome::Finished, &summary(100))
            .unwrap()
            .is_none());
        let events = cursor.load_tree_events().unwrap();
        let mut i = 0;
        assert!(matches!(events[i].kind, SessionEventKind::Message(_)));
        i += 1;
        assert!(matches!(events[i].kind, SessionEventKind::Message(_)));
        i += 1;
        assert!(matches!(events[i].kind, SessionEventKind::Message(_)));
        i += 1;
        match &events[i].kind {
            SessionEventKind::NativeTool(rec) => {
                assert_eq!(rec.name, "bash");
                assert_eq!(rec.parent, "t1");
                assert_eq!(rec.result, "file");
            }
            other => panic!("expected native tool, got {other:?}"),
        }
        i += 1;
        match &events[i].kind {
            SessionEventKind::ToolTiming {
                tool_call_id,
                elapsed_ms,
            } => {
                assert_eq!(tool_call_id, "t1");
                assert_eq!(*elapsed_ms, 7);
            }
            other => panic!("expected tool timing, got {other:?}"),
        }
        i += 1;
        match &events[i].kind {
            SessionEventKind::ThinkingTiming { elapsed_ms } => assert_eq!(*elapsed_ms, 12),
            other => panic!("expected thinking timing, got {other:?}"),
        }
        i += 1;
        match &events[i].kind {
            SessionEventKind::TurnEnd {
                model,
                elapsed_ms,
                cost,
                ..
            } => {
                assert_eq!(model, &RunModel::from("m"));
                assert_eq!(*elapsed_ms, 100);
                assert!((cost - 0.01).abs() < 1e-9);
            }
            other => panic!("expected turn end, got {other:?}"),
        }
        assert_eq!(events.len(), i + 1);
        assert!(!events[0].id.is_empty());
        assert!(events[0].parent_id.is_none());
        for w in events.windows(2) {
            assert_eq!(w[1].parent_id.as_deref(), Some(w[0].id.as_str()));
        }
        let _ = range;
    }

    #[test]
    fn checkpoints_each_round_without_duplicate_final_flush() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut rec = SessionRecorder::new(cursor.clone(), "m".into());
        let mut first_summary = summary(10);
        first_summary.native_tools.clear();
        first_summary.thinking_elapsed.clear();
        first_summary.tool_elapsed = vec![("t1".into(), 7)];
        let first = vec![user_msg("go"), assistant_text("round one")];

        let first_range = rec
            .checkpoint(&first, &first_summary)
            .unwrap()
            .expect("first round persisted");
        let checkpoint_events = cursor.load_tree_events().unwrap();
        let checkpoint_size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            checkpoint_events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::Message(_)))
                .count(),
            2
        );
        assert!(!checkpoint_events.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::TurnEnd { .. }
                | SessionEventKind::TurnFailed { .. }
                | SessionEventKind::TurnCancelled { .. }
        )));
        assert_eq!(first_range.1, checkpoint_size);

        let mut all = first;
        all.push(assistant_text("round two"));
        let mut final_summary = first_summary;
        final_summary.elapsed_ms = 20;
        final_summary.thinking_elapsed.push(3);
        let full_range = rec
            .flush(&all, &TurnOutcome::Finished, &final_summary)
            .unwrap()
            .expect("final suffix persisted");
        let events = cursor.load_tree_events().unwrap();
        let file_size = std::fs::metadata(&path).unwrap().len();
        assert_eq!(full_range, (first_range.0, file_size));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::Message(_)))
                .count(),
            3,
            "checkpointed messages must not be duplicated"
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::ToolTiming { .. }))
                .count(),
            1,
            "checkpointed timings must not be duplicated"
        );
        assert!(matches!(
            events.last().map(|event| &event.kind),
            Some(SessionEventKind::TurnEnd { .. })
        ));
    }

    #[test]
    fn unrun_prompt_takes_the_shape_of_a_turn_died_before_serving() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);

        cursor
            .record_unrun_prompt("typed but never run", lofi_types::PromptKind::User)
            .unwrap();

        let events = cursor.load_events().unwrap();
        assert_eq!(events.len(), 1);
        match &events[0].kind {
            SessionEventKind::Message(message) => {
                assert_eq!(message.role, Role::User);
                match &message.blocks[0] {
                    lofi_types::ContentBlock::Text { text } => {
                        assert_eq!(text, "typed but never run");
                    }
                    other => panic!("expected text block, got {other:?}"),
                }
            }
            other => panic!("expected message, got {other:?}"),
        }
        let rebuilt = crate::session::replay::messages_from_events(&events);
        assert_eq!(rebuilt.len(), 1, "the unrun prompt rejoins the context");
    }

    #[test]
    fn displaced_removal_never_loses_or_duplicates_messages() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut rec = SessionRecorder::new(cursor.clone(), "m".into());
        let mut clean = summary(10);
        clean.native_tools.clear();
        clean.thinking_elapsed.clear();
        clean.tool_elapsed.clear();

        let mut messages = vec![user_msg("go"), assistant_text("loop round")];
        rec.checkpoint(&messages, &clean).unwrap();
        rec.discard_round("potential agent loop detected").unwrap();
        messages.pop();
        rec.note_displaced(1);
        messages.push(assistant_text("recovery"));
        rec.flush(&messages, &TurnOutcome::Finished, &clean)
            .unwrap()
            .expect("final suffix persisted");

        let events = cursor.load_tree_events().unwrap();
        let mut texts = Vec::new();
        let mut saw_discard_marker = false;
        for event in &events {
            match &event.kind {
                SessionEventKind::Message(message) => {
                    if let lofi_types::ContentBlock::Text { text } = &message.blocks[0] {
                        texts.push(text.clone());
                    }
                }
                SessionEventKind::RoundDiscarded { detail } => {
                    saw_discard_marker = true;
                    assert!(detail.contains("loop"));
                }
                SessionEventKind::TurnEnd { .. } => {}
                other => panic!("unexpected event in test: {other:?}"),
            }
        }
        assert!(saw_discard_marker);
        let expected = vec!["go", "loop round", "recovery"];
        assert_eq!(
            texts, expected,
            "discarded round stays durable and recovery answer is not lost"
        );
    }

    #[test]
    fn interleaved_writer_cannot_steal_later_checkpoint() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();

        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut first = SessionRecorder::new(cursor.clone(), "m".into());
        let mut clean = summary(10);
        clean.native_tools.clear();
        clean.thinking_elapsed.clear();
        clean.tool_elapsed.clear();
        first
            .checkpoint(&[user_msg("first"), assistant_text("round one")], &clean)
            .unwrap();
        let first_leaf = cursor.leaf_id().unwrap();

        let mut sibling = [SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(user_msg("sibling")),
        }];
        store::append_events(&path, &mut sibling, None).unwrap();

        first
            .flush(
                &[
                    user_msg("first"),
                    assistant_text("round one"),
                    assistant_text("round two"),
                ],
                &TurnOutcome::Finished,
                &clean,
            )
            .unwrap();
        let final_leaf = cursor.leaf_id().unwrap();
        let events = cursor.load_tree_events().unwrap();
        let final_path = store::active_path(&events, &final_leaf);

        assert!(final_path.iter().any(|&i| events[i].id == first_leaf));
        assert!(
            !final_path.iter().any(|&i| events[i].id == sibling[0].id),
            "later checkpoint must follow the recorder leaf, not physical EOF"
        );
    }

    #[test]
    fn flush_without_turn_end_when_unfinished() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut rec = SessionRecorder::new(cursor.clone(), "m".into());
        let messages = vec![user_msg("go"), assistant_text("hi")];
        rec.flush(&messages, &TurnOutcome::Detached, &summary(50))
            .unwrap();
        let events = cursor.load_tree_events().unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::TurnEnd { .. })));
    }

    #[test]
    fn flush_empty_turn_writes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut rec = SessionRecorder::new(cursor.clone(), "m".into());
        let empty = TurnSummary {
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
            tool_elapsed: vec![],
            thinking_elapsed: vec![],
            native_tools: vec![],
            stop_reason: None,
        };
        let range = rec.flush(&[], &TurnOutcome::Detached, &empty).unwrap();
        assert!(range.is_none());
        let events = cursor.load_tree_events().unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn flush_cancelled_writes_terminal_marker_after_partial_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path, None);
        let mut rec = SessionRecorder::new(cursor.clone(), "m".into());

        rec.flush(
            &[user_msg("go"), assistant_text("partial")],
            &TurnOutcome::Cancelled,
            &summary(50),
        )
        .unwrap();

        let events = cursor.load_events().unwrap();
        let cancelled = events.last().expect("terminal marker");
        assert!(matches!(
            cancelled.kind,
            SessionEventKind::TurnCancelled { elapsed_ms: 50, .. }
        ));
        assert_eq!(
            cancelled.parent_id.as_deref(),
            events.get(events.len() - 2).map(|event| event.id.as_str())
        );
    }

    #[test]
    fn flush_with_turn_failed_chains_linearly_off_last_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path.clone(), None);
        let mut rec1 = SessionRecorder::new(cursor.clone(), "m".into());
        rec1.flush(
            &[user_msg("first"), assistant_text("reply")],
            &TurnOutcome::Finished,
            &TurnSummary {
                elapsed_ms: 10,
                cost: 0.0,
                usage: Usage::default(),
                tool_elapsed: vec![],
                thinking_elapsed: vec![],
                native_tools: vec![],
                stop_reason: None,
            },
        )
        .unwrap();

        let mut rec2 = SessionRecorder::new(cursor.clone(), "m".into());
        rec2.flush(
            &[user_msg("second"), assistant_text("partial")],
            &TurnOutcome::Failed("boom".into()),
            &TurnSummary {
                elapsed_ms: 5,
                cost: 0.02,
                usage: Usage {
                    input_tokens: 1,
                    ..Usage::default()
                },
                tool_elapsed: vec![],
                thinking_elapsed: vec![],
                native_tools: vec![],
                stop_reason: None,
            },
        )
        .unwrap();
        let events = cursor.load_tree_events().unwrap();
        // The TurnFailed marker's parent is the failed turn's last message,
        // NOT the checkpoint — so the failed turn's content is on the active
        // path (visible) and messages_from_events skips it via the boundary.
        let failed = events
            .iter()
            .find(|e| matches!(e.kind, SessionEventKind::TurnFailed { .. }))
            .expect("TurnFailed marker written");
        let last_msg = events
            .iter()
            .find(|e| matches!(&e.kind, SessionEventKind::Message(m) if m.role == Role::Assistant
                && m.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "partial")))
            )
            .expect("failed turn assistant message present");
        assert_eq!(failed.parent_id.as_deref(), Some(last_msg.id.as_str()));
        match &failed.kind {
            SessionEventKind::TurnFailed { error, cost, .. } => {
                assert_eq!(error, "boom");
                assert!((*cost - 0.02).abs() < 1e-9);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn record_writes_system_bash_job_and_compaction() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let cursor = store::SessionCursor::new(path, None);

        cursor
            .record(SessionRecord::System { prompt: "sys" })
            .unwrap();
        let bash = UserShellResult {
            command: "pwd".into(),
            output: "/".into(),
            exit_code: Some(0),
            signal: None,
            duration_ms: 1,
            truncated: false,
            cancelled: false,
        };
        cursor
            .record(SessionRecord::UserShell {
                result: &bash,
                exclude_from_context: false,
            })
            .unwrap();
        let owner = cursor.record_job_started(7).unwrap();
        cursor.record_job_finished(&owner, 7).unwrap();
        cursor
            .record(SessionRecord::Compaction {
                kept_messages: &[user_msg("kept")],
                summary: "sum",
                summarized_range: &["a".into(), "b".into()],
                counts: CompactionCounts {
                    summarized: 1,
                    represented: 1,
                    kept: 1,
                },
                system_prompt: "sys2",
            })
            .unwrap();

        let kinds: Vec<&'static str> = cursor
            .load_tree_events()
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.kind {
                SessionEventKind::Message(message) if message.role == Role::System => {
                    Some("system")
                }
                SessionEventKind::UserShell { .. } => Some("bash"),
                SessionEventKind::JobStarted { .. } => Some("job-started"),
                SessionEventKind::JobFinished { .. } => Some("job-finished"),
                SessionEventKind::Message(message) if message.role == Role::User => Some("kept"),
                SessionEventKind::Compaction { .. } => Some("compaction"),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "system",
                "bash",
                "job-started",
                "job-finished",
                "kept",
                "compaction",
                "system"
            ]
        );
    }
}
