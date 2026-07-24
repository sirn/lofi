//! Durable-event recorder: translates a finished turn into the
//! [`SessionEvent`] subset that gets appended to the transcript.
//!
//! This is the single place that shapes `SessionEvent`s, so the agent loop
//! (`agent::run_continuation`) is free of on-disk-format concerns — it emits
//! `AgentEvent`s to the live channel and, at turn end, hands the recorder the
//! growing message slice plus a [`TurnSummary`] of the engine's accumulators.
//! The recorder appends the newly completed suffix after each round, then
//! writes the terminal marker when the turn settles.
//!
//! The recorder is checkpoint-based rather than a streaming subscriber:
//! native-tool events originate in a synchronous sandbox callback and are
//! accumulated in `TurnStats`. Snapshotting at clean provider-round boundaries
//! keeps complete assistant/tool-result cycles durable without duplicating the
//! live event fan-out machinery.

use std::{collections::HashSet, path::Path};

use lofi_types::{Message, NativeToolRecord, RunModel, SessionEvent, SessionEventKind, Usage};

use crate::session::store;
use lofi_error::Result;

/// How a turn ended, for [`SessionRecorder::flush`]. Determines whether a
/// `TurnEnd`, `TurnFailed`, or no terminal marker is written.
#[derive(Debug, Clone)]
pub enum TurnOutcome {
    /// The turn completed normally — write a `TurnEnd` marker.
    Finished,
    /// The turn ended in a non-retryable error or was cancelled — write a
    /// `TurnFailed` marker carrying the error message. The turn's messages
    /// and timings are still written so the failed attempt is visible in the
    /// tree and its consumed tokens are honestly accounted for;
    /// `messages_from_events` then skips the failed turn's messages when
    /// building the agent's history on resume (so the model is not fed
    /// partial/errored content) while the UI still renders them.
    Failed(String),
    /// The run was force-stopped at the hard context cap mid-turn. The
    /// partial turn's messages and timings are written (so a following
    /// compaction can keep the latest turn verbatim and `messages_from_events`
    /// includes them), but NO terminal marker is written — the turn did not
    /// finish or fail, and the UI is signaled via a live `ContextPressure`
    /// event instead. Behaves like [`Cancelled`] for the empty-input
    /// short-circuit (no terminal marker).
    ContextPressure,
    /// The turn was abandoned before it produced anything worth recording —
    /// write no terminal marker (and the flush's empty-input short-circuit
    /// applies).
    Cancelled,
}

/// A finished turn's engine-side accumulators, snapshotted at commit time and
/// handed to [`SessionRecorder::flush`]. Built from the agent's private
/// `TurnStats` so the recorder only depends on public data.
#[derive(Debug, Clone)]
pub struct TurnSummary {
    /// Wall-clock duration of the whole turn in milliseconds.
    pub elapsed_ms: u64,
    /// Accumulated USD cost across the turn's rounds.
    pub cost: f64,
    /// Final round's token usage (drives the context gauge).
    pub usage: Usage,
    /// `(tool_call_id, elapsed_ms)` for each tool call this turn, so the
    /// exec block's `took Ns` marker survives resume.
    pub tool_elapsed: Vec<(String, u64)>,
    /// Wall-clock duration of each assistant thinking block this turn, in
    /// emission order, so the "Thought for Ns" marker survives resume.
    pub thinking_elapsed: Vec<u64>,
    /// Native tool calls (`lofi.<tool>`) that ran inside `exec` blocks this
    /// turn, in the order they were captured.
    pub native_tools: Vec<NativeToolRecord>,
}

/// Writes one turn's durable [`SessionEvent`]s to the transcript.
///
/// Construct one per persisted turn (when a `SessionCommit` is available)
/// and call [`flush`](Self::flush) once when the turn is done. `flush` is
/// idempotent — a second call writes nothing — so it is safe to call after
/// both a normal `TurnEnd` and an error path.
#[derive(Debug)]
pub struct SessionRecorder {
    path: std::path::PathBuf,
    model: RunModel,
    /// The entry id to branch this turn from. `None` appends to the file's
    /// current active leaf (linear continuation); `Some(id)` starts a new
    /// branch as a sibling of `id`'s existing children.
    parent_hint: Option<String>,
    flushed: bool,
    message_count: usize,
    native_tool_count: usize,
    thinking_timing_count: usize,
    tool_timing_ids: HashSet<String>,
    byte_start: Option<u64>,
    byte_end: Option<u64>,
}

impl SessionRecorder {
    /// Wrap a transcript path + the raw model identity used for the
    /// `SessionEvent::TurnEnd` marker. The turn appends to the file's active
    /// leaf (no branching).
    #[must_use]
    pub fn new(path: std::path::PathBuf, model: RunModel) -> Self {
        Self {
            path,
            model,
            parent_hint: None,
            flushed: false,
            message_count: 0,
            native_tool_count: 0,
            thinking_timing_count: 0,
            tool_timing_ids: HashSet::new(),
            byte_start: None,
            byte_end: None,
        }
    }

    /// Like [`new`](Self::new) but branches the turn off `parent_hint` instead
    /// of appending to the active leaf. Used by the agent when the user
    /// resumes from a selected entry in the tree picker.
    #[must_use]
    pub fn with_parent(path: std::path::PathBuf, model: RunModel, parent_hint: String) -> Self {
        Self {
            path,
            model,
            parent_hint: Some(parent_hint),
            flushed: false,
            message_count: 0,
            native_tool_count: 0,
            thinking_timing_count: 0,
            tool_timing_ids: HashSet::new(),
            byte_start: None,
            byte_end: None,
        }
    }

    /// Append everything completed since the previous checkpoint, without a
    /// terminal marker. Called after each provider/tool round so a long turn
    /// is durable before the whole agent loop settles.
    ///
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
    ///
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
            }),
            TurnOutcome::Failed(error) => Some(SessionEventKind::TurnFailed {
                model: self.model.clone(),
                elapsed_ms: summary.elapsed_ms,
                error: error.clone(),
                cost: summary.cost,
                usage: summary.usage,
            }),
            TurnOutcome::ContextPressure | TurnOutcome::Cancelled => None,
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
        let new_messages = messages.get(self.message_count..).unwrap_or_default();
        let new_native = summary
            .native_tools
            .get(self.native_tool_count..)
            .unwrap_or_default();
        let new_thinking = summary
            .thinking_elapsed
            .get(self.thinking_timing_count..)
            .unwrap_or_default();
        let mut events = Vec::new();
        events.extend(new_messages.iter().cloned().map(|message| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(message),
        }));
        events.extend(new_native.iter().cloned().map(|record| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::NativeTool(record),
        }));
        let new_tool_timings: Vec<(&String, &u64)> = summary
            .tool_elapsed
            .iter()
            .filter(|(id, _)| !self.tool_timing_ids.contains(id))
            .map(|(id, elapsed_ms)| (id, elapsed_ms))
            .collect();
        for (id, elapsed_ms) in &new_tool_timings {
            events.push(SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::ToolTiming {
                    tool_call_id: (*id).clone(),
                    elapsed_ms: **elapsed_ms,
                },
            });
        }
        events.extend(new_thinking.iter().copied().map(|elapsed_ms| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::ThinkingTiming { elapsed_ms },
        }));
        if let Some(kind) = terminal {
            events.push(SessionEvent {
                id: String::new(),
                parent_id: None,
                kind,
            });
        }
        if events.is_empty() {
            return Ok(None);
        }
        let parent_hint = self.parent_hint.take();
        let (start, end) =
            match store::append_events(&self.path, &mut events, parent_hint.as_deref()) {
                Ok(range) => range,
                Err(error) => {
                    // Do not advance cursors or consume the explicit branch
                    // point: a later checkpoint/final flush retries the
                    // complete unwritten suffix from the same parent.
                    self.parent_hint = parent_hint;
                    return Err(error);
                }
            };
        self.message_count = messages.len();
        self.native_tool_count = summary.native_tools.len();
        self.thinking_timing_count = summary.thinking_elapsed.len();
        self.tool_timing_ids
            .extend(new_tool_timings.into_iter().map(|(id, _)| id.clone()));
        if end > start {
            self.byte_start.get_or_insert(start);
            self.byte_end = Some(end);
            Ok(Some((start, end)))
        } else {
            Ok(None)
        }
    }

    /// The transcript path this recorder writes to.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::*;
    use lofi_types::{ContentBlock, Role};
    use tempfile::tempdir;

    fn user_msg(t: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: t.into() }],
        }
    }

    fn assistant_text(t: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: t.into() }],
        }
    }

    fn header() -> &'static [u8] {
        b"{\"type\":\"meta\",\"version\":2,\"created\":0,\"cwd\":\"\",\"model\":\"m\"}\n"
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
        }
    }

    #[test]
    fn flush_writes_messages_timings_and_turn_end_in_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let mut rec = SessionRecorder::new(path.clone(), "m".into());
        let messages = vec![
            user_msg("go"),
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "exec".into(),
                    input: serde_json::Value::String("1".into()),
                }],
            },
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "2".into(),
                    is_error: false,
                }],
            },
        ];
        let range = rec
            .flush(&messages, &TurnOutcome::Finished, &summary(100))
            .unwrap()
            .expect("wrote something");
        // Second flush is a no-op.
        assert!(rec
            .flush(&messages, &TurnOutcome::Finished, &summary(100))
            .unwrap()
            .is_none());
        let (_meta, events, _offsets, _size) = store::load(&path).unwrap();
        // Expected order: 3 messages, native tool, tool timing, thinking
        // timing, turn end.
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
        // Every flushed event got an id and chains to the previous one
        // (first event's parent is None — root of the file).
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
        let mut rec = SessionRecorder::new(path.clone(), "m".into());
        let mut first_summary = summary(10);
        first_summary.native_tools.clear();
        first_summary.thinking_elapsed.clear();
        first_summary.tool_elapsed = vec![("t1".into(), 7)];
        let first = vec![user_msg("go"), assistant_text("round one")];

        let first_range = rec
            .checkpoint(&first, &first_summary)
            .unwrap()
            .expect("first round persisted");
        let (_, checkpoint_events, _, checkpoint_size) = store::load(&path).unwrap();
        assert_eq!(
            checkpoint_events
                .iter()
                .filter(|event| matches!(event.kind, SessionEventKind::Message(_)))
                .count(),
            2
        );
        assert!(!checkpoint_events.iter().any(|event| matches!(
            event.kind,
            SessionEventKind::TurnEnd { .. } | SessionEventKind::TurnFailed { .. }
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
        let (_, events, _, file_size) = store::load(&path).unwrap();
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
    fn flush_without_turn_end_when_unfinished() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let mut rec = SessionRecorder::new(path.clone(), "m".into());
        let messages = vec![user_msg("go"), assistant_text("hi")];
        rec.flush(&messages, &TurnOutcome::Cancelled, &summary(50))
            .unwrap();
        let (_meta, events, _, _) = store::load(&path).unwrap();
        assert!(!events
            .iter()
            .any(|e| matches!(e.kind, SessionEventKind::TurnEnd { .. })));
    }

    #[test]
    fn flush_empty_turn_writes_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let mut rec = SessionRecorder::new(path.clone(), "m".into());
        let empty = TurnSummary {
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
            tool_elapsed: vec![],
            thinking_elapsed: vec![],
            native_tools: vec![],
        };
        let range = rec.flush(&[], &TurnOutcome::Cancelled, &empty).unwrap();
        assert!(range.is_none());
        let (_meta, events, _, _) = store::load(&path).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn flush_with_turn_failed_chains_linearly_off_last_message() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        // First turn: a completed user -> assistant exchange.
        let mut rec1 = SessionRecorder::new(path.clone(), "m".into());
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
            },
        )
        .unwrap();

        // Second turn: failed. Its messages + TurnFailed marker chain
        // linearly off the first turn's TurnEnd (same shape as a successful
        // turn), so the failed turn's content stays on the active path and
        // remains visible on resume.
        let mut rec2 = SessionRecorder::new(path.clone(), "m".into());
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
            },
        )
        .unwrap();
        let (_meta, events, _, _) = store::load(&path).unwrap();
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
}
