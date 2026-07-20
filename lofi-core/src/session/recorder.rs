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

use lofi_types::{Message, NativeToolRecord, SessionEvent, SessionEventKind,
    Usage};

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
    label: String,
    /// The entry id to branch this turn from. `None` appends to the file's
    /// current active leaf (linear continuation); `Some(id)` starts a new
    /// branch as a sibling of `id`'s existing children.
    parent_hint: Option<String>,
    flushed: bool,
}

impl SessionRecorder {
    /// Wrap a transcript path + the run label (`provider/model · level`) used
    /// for the `SessionEvent::TurnEnd` marker. The turn appends to the file's
    /// active leaf (no branching).
    #[must_use]
    pub fn new(path: std::path::PathBuf, label: String) -> Self {
        Self {
            path,
            label,
            parent_hint: None,
            flushed: false,
        }
    }

    /// Like [`new`](Self::new) but branches the turn off `parent_hint` instead
    /// of appending to the active leaf. Used by the agent when the user
    /// resumes from a selected entry in the tree picker.
    #[must_use]
    pub fn with_parent(path: std::path::PathBuf, label: String, parent_hint: String) -> Self {
        Self {
            path,
            label,
            parent_hint: Some(parent_hint),
            flushed: false,
        }
    }

    /// Flush the durable subset for this turn to the transcript and return the
    /// byte range of the appended lines (`None` if nothing was written).
    ///
    /// `messages` is the slice of conversation messages produced this turn
    /// (user prompt, assistant turns, tool-result turns), in order. `outcome`
    /// selects the terminal marker: [`TurnOutcome::Finished`] writes a
    /// `TurnEnd`, [`TurnOutcome::Failed`] writes a `TurnFailed` (chained
    /// linearly off the turn's last message, just like `TurnEnd`, so the
    /// failed turn's content stays on the active path and remains visible on
    /// resume; `messages_from_events` then skips that content when building
    /// the agent's history), and [`TurnOutcome::Cancelled`] writes no marker.
    /// `summary` carries the engine's timing/cost/native-tool accumulators
    /// that become `ToolTiming`/`ThinkingTiming`/`NativeTool`/terminal events.
    ///
    /// # Errors
    /// Propagates [`lofi_error::Error`] from disk write/serialization.
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
        let has_terminal = !matches!(outcome, TurnOutcome::Cancelled);
        if messages.is_empty()
            && summary.tool_elapsed.is_empty()
            && summary.thinking_elapsed.is_empty()
            && summary.native_tools.is_empty()
            && !has_terminal
        {
            return Ok(None);
        }
        let mut events: Vec<SessionEvent> = messages
            .iter()
            .cloned()
            .map(|m| SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(m),
            })
            .collect();
        for rec in &summary.native_tools {
            events.push(SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::NativeTool(rec.clone()),
            });
        }
        for (id, ms) in &summary.tool_elapsed {
            events.push(SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::ToolTiming {
                    tool_call_id: id.clone(),
                    elapsed_ms: *ms,
                },
            });
        }
        for ms in &summary.thinking_elapsed {
            events.push(SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::ThinkingTiming { elapsed_ms: *ms },
            });
        }
        match outcome {
            TurnOutcome::Finished => {
                events.push(SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: SessionEventKind::TurnEnd {
                        label: self.label.clone(),
                        elapsed_ms: summary.elapsed_ms,
                        cost: summary.cost,
                        usage: summary.usage,
                    },
                });
            }
            TurnOutcome::Failed(error) => {
                // Chain linearly off the turn's last message (same as
                // `TurnEnd`) so the failed turn's content stays on the active
                // path and remains visible on resume. The agent-history walk
                // in `messages_from_events` skips the failed turn's messages
                // via the `TurnFailed` boundary, so the model is not fed
                // partial/errored content.
                events.push(SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: SessionEventKind::TurnFailed {
                        label: self.label.clone(),
                        elapsed_ms: summary.elapsed_ms,
                        error: error.clone(),
                        cost: summary.cost,
                        usage: summary.usage,
                    },
                });
            }
            TurnOutcome::Cancelled => {}
        }
        let (start, end) = store::append_events(&self.path, &mut events, self.parent_hint.as_deref())?;
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
        assert!(rec.flush(&messages, &TurnOutcome::Finished, &summary(100)).unwrap().is_none());
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
            SessionEventKind::ToolTiming { tool_call_id, elapsed_ms } => {
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
            SessionEventKind::TurnEnd { label, elapsed_ms, cost, .. } => {
                assert_eq!(label, "m");
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
    fn flush_without_turn_end_when_unfinished() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, header()).unwrap();
        let mut rec = SessionRecorder::new(path.clone(), "m".into());
        let messages = vec![user_msg("go"), assistant_text("hi")];
        rec.flush(&messages, &TurnOutcome::Cancelled, &summary(50)).unwrap();
        let (_meta, events, _, _) = store::load(&path).unwrap();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, SessionEventKind::TurnEnd { .. }))
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
        rec1
            .flush(
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
        rec2
            .flush(
                &[user_msg("second"), assistant_text("partial")],
                &TurnOutcome::Failed("boom".into()),
                &TurnSummary {
                    elapsed_ms: 5,
                    cost: 0.02,
                    usage: Usage { input_tokens: 1, ..Usage::default() },
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