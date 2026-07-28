#![allow(clippy::wildcard_imports)]

use super::*;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A user turn has begun with this prompt. Always the first event of a
    /// turn; the matching [`TurnEnd`](Self::TurnEnd) (or
    /// [`Error`](Self::Error)) closes it. Making the turn boundary explicit
    /// lets a consumer build its view from the event stream alone — no
    /// external "push a new turn" call — so the live path and the
    /// from-disk replay path share one builder.
    TurnStart {
        prompt: String,
    },
    /// A silent continuation of the current turn has begun — the run was
    /// force-stopped at the hard cap, compacted, and is now resuming on the
    /// compacted history without a new user prompt. The UI must NOT push a
    /// new turn (no "You:" line); it appends blocks to the current turn.
    /// Mirrors [`TurnStart`](Self::TurnStart) for a continuation.
    TurnContinue,
    /// A completed direct user shell command. This never starts an agent
    /// turn; the TUI renders it as a standalone shell block and the session
    /// replayer reconstructs it from `SessionEventKind::UserBash`.
    UserBash {
        command: String,
        output: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        duration_ms: u64,
        truncated: bool,
        cancelled: bool,
        exclude_from_context: bool,
    },
    Text(String),
    Thinking(String),
    ThinkingEnd {
        elapsed_ms: u64,
    },
    ToolStart {
        /// The tool-call id assigned by the provider.
        id: String,
        name: String,
    },
    ToolInput {
        id: String,
        code: String,
        label: Option<String>,
    },
    ToolInputDelta {
        id: String,
        delta: String,
    },
    ToolEnd {
        id: String,
        result: String,
        is_error: bool,
        elapsed_ms: u64,
    },
    NativeToolStart {
        parent: String,
        id: u64,
        name: String,
        args: String,
    },
    NativeToolEnd {
        parent: String,
        id: u64,
        result: String,
        is_error: bool,
    },
    /// A turn completed: the raw model identity that ran it, wall-clock
    /// duration, the accumulated USD cost across the turn's rounds, and the
    /// final round's token usage. Emitted once per turn by
    /// [`Agent::run_continuation`] and also written to the transcript, so the
    /// live view and a resumed view render the same `◇ Done in Ns with
    /// <model>` block — the model travels with the event rather than being
    /// re-derived from the (possibly switched) active model on resume.
    TurnEnd {
        model: RunModel,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
    },
    /// A turn ended in failure (a non-retryable provider error or a user
    /// cancel): the rounds that did run still consumed tokens, so this
    /// carries the same cost/usage as [`TurnEnd`](Self::TurnEnd) plus the
    /// error message. The UI folds the turn's `turn_cost` into `cost` (so
    /// failed attempts are honestly accounted for) and renders a red
    /// `◇ Failed in Ns with <model>` marker. Emitted once per failed turn,
    /// after any [`RoundUsage`](Self::RoundUsage) events for the rounds that
    /// completed.
    TurnFailed {
        model: RunModel,
        elapsed_ms: u64,
        /// The error that ended the turn (provider error or "cancelled").
        error: String,
        cost: f64,
        usage: Usage,
    },
    /// The run hit the hard context cap mid-turn: the latest round's
    /// input tokens exceeded `context_window - reserved_context_tokens`.
    /// The engine stops before the next (overflowing) round and commits the
    /// partial turn (which ends in a completed tool cycle, so its messages
    /// are kept verbatim). The UI force-compacts and silently continues via
    /// [`TurnContinue`](Self::TurnContinue). Carries the same accumulators as
    /// [`TurnEnd`](Self::TurnEnd) so the context gauge and cost stay honest.
    /// Never persisted as a `SessionEvent` — the partial turn is committed
    /// without a terminal marker, so this is a live-only signal.
    ContextPressure {
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
    },
    /// A provider error that ended the run after retry classification.
    /// Transient errors that will be retried use RetryStart/RetryEnd instead,
    /// so they never appear as fatal transcript rows.
    Error(String),
    /// The provider returned a transient error and the agent is waiting out
    /// the backoff before retrying the last assistant round. Emitted before
    /// the sleep so the UI can show a retry indicator.
    RetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
    /// A retry completed (either the retry succeeded or the budget was
    /// exhausted). Emitted after the retry's outcome is known.
    RetryEnd {
        success: bool,
        attempt: u32,
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
        cost: f64,
        usage: Usage,
    },
    /// One complete provider/tool round was durably appended to the transcript.
    /// This is a storage/watermark signal only: it must never create, replace,
    /// or finalize a visible turn. The TUI may release already-hidden payloads
    /// from completed tool rows, while preserving the prompt and every block.
    RoundCommitted { byte_start: u64, byte_end: u64 },
    TurnCommitted { byte_start: u64, byte_end: u64 },
    /// An offline compaction ran: `summarized` live messages were folded into
    /// a structured summary and `kept` remain in the tail. Never produced by
    /// the agent loop — synthesized by the replay path from a
    /// `SessionEventKind::Compaction` marker and by the `/compact` command,
    /// so the live view and a resumed view render the same marker. `summary`
    /// carries the folded text so `/verbose` can expand it inline.
    Compaction {
        summarized: usize,
        kept: usize,
        summary: String,
    },
}
