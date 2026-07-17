//! Durable-event recorder: translates a finished turn into the
//! [`SessionEvent`] subset that gets appended to the transcript.
//!
//! This is the single place that shapes `SessionEvent`s, so the agent loop
//! (`agent::run_continuation`) is free of on-disk-format concerns — it emits
//! `AgentEvent`s to the live channel and, at turn end, hands the recorder the
//! finalized message slice plus a [`TurnSummary`] of the engine's
//! accumulators. The recorder pairs them into `SessionEvent`s and writes one
//! append block per turn.
//!
//! The recorder is flush-time rather than a streaming subscriber because the
//! engine's native-tool events reach the live channel through an unbounded
//! relay (they originate in a sync sandbox callback), and re-teeing that relay
//! into the recorder would duplicate the fan-out machinery. The engine already
//! accumulates the durable-relevant state in `TurnStats`; the recorder borrows
//! a snapshot of it at flush, keeping the on-disk translation in one module.

use std::path::Path;

use lofi_types::{Message, NativeToolRecord, SessionEvent, Usage};

use crate::session::store;
use lofi_error::Result;

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
    label: String,
    flushed: bool,
}

impl SessionRecorder {
    /// Wrap a transcript path + the run label (`provider/model · level`) used
    /// for the `SessionEvent::TurnEnd` marker.
    #[must_use]
    pub fn new(path: std::path::PathBuf, label: String) -> Self {
        Self {
            path,
            label,
            flushed: false,
        }
    }

    /// Flush the durable subset for this turn to the transcript and return the
    /// byte range of the appended lines (`None` if nothing was written).
    ///
    /// `messages` is the slice of conversation messages produced this turn
    /// (user prompt, assistant turns, tool-result turns), in order. `finished`
    /// is whether the turn completed normally (an errored or abandoned turn
    /// writes its messages but no `TurnEnd` marker, matching the live view's
    /// missing marker). `summary` carries the engine's timing/cost/native-tool
    /// accumulators that become `ToolTiming`/`ThinkingTiming`/`NativeTool`/
    /// `TurnEnd` events.
    ///
    /// # Errors
    /// Propagates [`lofi_error::Error`] from disk write/serialization.
    pub fn flush(
        &mut self,
        messages: &[Message],
        finished: bool,
        summary: &TurnSummary,
    ) -> Result<Option<(u64, u64)>> {
        if self.flushed {
            return Ok(None);
        }
        self.flushed = true;
        if messages.is_empty()
            && summary.tool_elapsed.is_empty()
            && summary.thinking_elapsed.is_empty()
            && summary.native_tools.is_empty()
            && !finished
        {
            return Ok(None);
        }
        let mut events: Vec<SessionEvent> =
            messages.iter().cloned().map(SessionEvent::Message).collect();
        for rec in &summary.native_tools {
            events.push(SessionEvent::NativeTool(rec.clone()));
        }
        for (id, ms) in &summary.tool_elapsed {
            events.push(SessionEvent::ToolTiming {
                id: id.clone(),
                elapsed_ms: *ms,
            });
        }
        for ms in &summary.thinking_elapsed {
            events.push(SessionEvent::ThinkingTiming { elapsed_ms: *ms });
        }
        if finished {
            events.push(SessionEvent::TurnEnd {
                label: self.label.clone(),
                elapsed_ms: summary.elapsed_ms,
                cost: summary.cost,
                usage: summary.usage,
            });
        }
        let (start, end) = store::append_events(&self.path, &events)?;
        if end > start {
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
                id: 0,
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
            .flush(&messages, true, &summary(100))
            .unwrap()
            .expect("wrote something");
        // Second flush is a no-op.
        assert!(rec.flush(&messages, true, &summary(100)).unwrap().is_none());
        let (_meta, events, _offsets, _size) = store::load(&path).unwrap();
        // Expected order: 3 messages, native tool, tool timing, thinking
        // timing, turn end.
        let mut i = 0;
        assert!(matches!(events[i], SessionEvent::Message(_)));
        i += 1;
        assert!(matches!(events[i], SessionEvent::Message(_)));
        i += 1;
        assert!(matches!(events[i], SessionEvent::Message(_)));
        i += 1;
        match &events[i] {
            SessionEvent::NativeTool(rec) => {
                assert_eq!(rec.name, "bash");
                assert_eq!(rec.parent, "t1");
                assert_eq!(rec.result, "file");
            }
            other => panic!("expected native tool, got {other:?}"),
        }
        i += 1;
        match &events[i] {
            SessionEvent::ToolTiming { id, elapsed_ms } => {
                assert_eq!(id, "t1");
                assert_eq!(*elapsed_ms, 7);
            }
            other => panic!("expected tool timing, got {other:?}"),
        }
        i += 1;
        match &events[i] {
            SessionEvent::ThinkingTiming { elapsed_ms } => assert_eq!(*elapsed_ms, 12),
            other => panic!("expected thinking timing, got {other:?}"),
        }
        i += 1;
        match &events[i] {
            SessionEvent::TurnEnd { label, elapsed_ms, cost, .. } => {
                assert_eq!(label, "m");
                assert_eq!(*elapsed_ms, 100);
                assert!((cost - 0.01).abs() < 1e-9);
            }
            other => panic!("expected turn end, got {other:?}"),
        }
        assert_eq!(events.len(), i + 1);
        let _ = range;
    }

    #[test]
    fn flush_without_turn_end_when_unfinished() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let mut rec = SessionRecorder::new(path.clone(), "m".into());
        let messages = vec![user_msg("go"), assistant_text("hi")];
        rec.flush(&messages, false, &summary(50)).unwrap();
        let (_meta, events, _, _) = store::load(&path).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SessionEvent::TurnEnd { .. }))
        );
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
        let range = rec.flush(&[], false, &empty).unwrap();
        assert!(range.is_none());
        let (_meta, events, _, _) = store::load(&path).unwrap();
        assert!(events.is_empty());
    }
}