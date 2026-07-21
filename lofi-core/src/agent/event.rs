#![allow(clippy::wildcard_imports)]

use super::*;

/// Events emitted by the agent loop to the TUI / print driver.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A user turn has begun with this prompt. Always the first event of a
    /// turn; the matching [`TurnEnd`](Self::TurnEnd) (or
    /// [`Error`](Self::Error)) closes it. Making the turn boundary explicit
    /// lets a consumer build its view from the event stream alone — no
    /// external "push a new turn" call — so the live path and the
    /// from-disk replay path share one builder.
    TurnStart {
        /// The user's prompt text.
        prompt: String,
    },
    /// A chunk of assistant text.
    Text(String),
    /// A chunk of the model's reasoning / chain-of-thought trace. Surfaced
    /// separately from [`Text`](Self::Text) so the UI can fold it while
    /// still streaming it live.
    Thinking(String),
    /// A thinking block just ended; `elapsed_ms` is its wall-clock duration,
    /// stamped by the engine so the "Thought for Ns" marker survives resume
    /// without a UI-owned timer (mirroring [`ToolEnd`](Self::ToolEnd) owning
    /// the tool timer). Always emitted after the [`Thinking`](Self::Thinking)
    /// deltas of one block and before the next block or a non-thinking event.
    ThinkingEnd {
        /// Wall-clock duration of the thinking block in milliseconds.
        elapsed_ms: u64,
    },
    /// A tool call has begun.
    ToolStart {
        /// The tool-call id assigned by the provider.
        id: String,
        /// The tool name (currently always `exec`).
        name: String,
    },
    /// The assembled input (the TypeScript `code`) for a tool call, emitted
    /// just before execution so the UI can show what ran — not just the id.
    ToolInput {
        /// The tool-call id.
        id: String,
        /// The TypeScript source passed to the sandbox.
        code: String,
        /// A short human label for the exec (from the `display` field), shown
        /// as `Exec <label>` by the UI. `None` when the model omitted it.
        label: Option<String>,
    },
    /// A streaming fragment of a tool call's `code` input, emitted as the
    /// model writes it so the UI can show the exec source growing live. The
    /// fragments concatenate into the same `code` that [`ToolInput`]
    /// finalizes (with the label and authoritative full text).
    ToolInputDelta {
        /// The tool-call id.
        id: String,
        /// A decoded fragment of the TypeScript `code` field.
        delta: String,
    },
    /// A tool call has completed with `result` (a JSON string for success, an
    /// error message for failure). `elapsed_ms` is the wall-clock duration from
    /// the matching [`AgentEvent::ToolStart`], stamped by the engine so it
    /// survives resume without a UI-owned timer.
    ToolEnd {
        /// The tool-call id.
        id: String,
        /// The tool result payload.
        result: String,
        /// Whether the call failed — drives the exec block's red background.
        is_error: bool,
        /// Wall-clock duration of the call in milliseconds.
        elapsed_ms: u64,
    },
    /// A native tool call inside an `exec` block has started. `parent` is the
    /// enclosing exec tool-call id; `id` is a per-exec counter.
    NativeToolStart {
        parent: String,
        id: u64,
        name: String,
        args: String,
    },
    /// A native tool call inside an `exec` block has finished.
    NativeToolEnd {
        parent: String,
        id: u64,
        result: String,
        is_error: bool,
    },
    /// A turn completed: its run label (`provider/model · level`), wall-clock
    /// duration, the accumulated USD cost across the turn's rounds, and the
    /// final round's token usage. Emitted once per turn by
    /// [`Agent::run_continuation`] and also written to the transcript, so the
    /// live view and a resumed view render the same `◇ label done in Ns`
    /// block — the label travels with the event rather than being re-derived
    /// from the (possibly switched) active model on resume.
    TurnEnd {
        /// `provider/model · level` label for the turn-end marker.
        label: String,
        /// Wall-clock duration of the turn in milliseconds.
        elapsed_ms: u64,
        /// Accumulated USD cost across the turn's rounds.
        cost: f64,
        /// Final round's token usage (drives the context gauge).
        usage: Usage,
    },
    /// A turn ended in failure (a non-retryable provider error or a user
    /// cancel): the rounds that did run still consumed tokens, so this
    /// carries the same cost/usage as [`TurnEnd`](Self::TurnEnd) plus the
    /// error message. The UI folds the turn's `turn_cost` into `cost` (so
    /// failed attempts are honestly accounted for) and renders a red
    /// `◇ label failed in Ns · <error>` marker. Emitted once per failed
    /// turn, after any [`RoundUsage`](Self::RoundUsage) events for the
    /// rounds that completed.
    TurnFailed {
        /// `provider/model · level` label for the failure marker.
        label: String,
        /// Wall-clock duration of the turn in milliseconds.
        elapsed_ms: u64,
        /// The error that ended the turn (provider error or "cancelled").
        error: String,
        /// Accumulated USD cost across the turn's rounds.
        cost: f64,
        /// Final round's token usage (drives the context gauge).
        usage: Usage,
    },
    /// A provider error was encountered mid-stream.
    Error(String),
    /// The provider returned a transient error and the agent is waiting out
    /// the backoff before retrying the last assistant round. Emitted before
    /// the sleep so the UI can show a retry indicator.
    RetryStart {
        /// 1-indexed retry attempt.
        attempt: u32,
        /// Maximum retry attempts.
        max_attempts: u32,
        /// Backoff duration in milliseconds.
        delay_ms: u64,
        /// The error message that triggered the retry.
        error: String,
    },
    /// A retry completed (either the retry succeeded or the budget was
    /// exhausted). Emitted after the retry's outcome is known.
    RetryEnd {
        /// Whether the retry ultimately succeeded (a non-error assistant
        /// message landed).
        success: bool,
        /// How many retries were attempted.
        attempt: u32,
        /// When `success` is false, the final error message that could not be
        /// recovered.
        final_error: Option<String>,
    },
    /// A provider round completed within the current turn, carrying the
    /// turn's cumulative USD cost so far and this round's token usage.
    /// Emitted after each `StreamingEvent::Done` so the UI can refresh the
    /// context gauge and cost counter per round instead of waiting for the
    /// turn's final [`TurnEnd`](Self::TurnEnd). Multi-round turns (tool-use
    /// loops) emit one per round; `TurnEnd` still fires once at the end with
    /// the same totals (so the resume path, which has no `RoundUsage`
    /// events, reconstructs identical state from `TurnEnd` alone).
    RoundUsage {
        /// Cumulative USD cost across the turn's rounds so far.
        cost: f64,
        /// This round's token usage (drives the context gauge).
        usage: Usage,
    },
    /// A turn's events were durably appended to the transcript file, covering
    /// the byte range `[byte_start, byte_end)`. The UI uses this to make the
    /// now-frozen turn file-backed (drop its in-memory blocks and re-materialize
    /// from this range on demand). Emitted only for persisted sessions, after
    /// the file is flushed.
    TurnCommitted {
        byte_start: u64,
        byte_end: u64,
    },
    /// An offline compaction ran: `summarized` live messages were folded into
    /// a structured summary and `kept` remain in the tail. Never produced by
    /// the agent loop — synthesized by the replay path from a
    /// `SessionEventKind::Compaction` marker and by the `/compact` command,
    /// so the live view and a resumed view render the same marker.
    Compaction {
        summarized: usize,
        kept: usize,
    },
}
