#![allow(clippy::wildcard_imports)]

use super::*;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Live-only lifecycle marker for a new agent run. This resets run-level
    /// state but does not create transcript content; every durable prompt uses
    /// [`Prompt`](Self::Prompt). Replay restores durable usage from terminal
    /// events instead of guessing run boundaries from adjacent prompts.
    RunStart,
    /// A durable prompt was added to the transcript. User input, job notices,
    /// and prompts injected by the run loop all use this event.
    Prompt {
        prompt: String,
        kind: lofi_types::PromptKind,
    },
    TurnContinue,
    UserShell {
        command: String,
        output: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        duration_ms: u64,
        truncated: bool,
        cancelled: bool,
        exclude_from_context: bool,
    },
    /// Live-only: a direct `!cmd` has started. Opens the standalone turn so
    /// its output can stream as it is produced; the finished command still
    /// records (and replays as) [`UserShell`](Self::UserShell), so this event
    /// is never persisted.
    UserShellStart {
        command: String,
        exclude_from_context: bool,
    },
    /// Live-only: an ANSI-stripped, UTF-8-safe output chunk of the running
    /// `!cmd`. The final [`UserShell`](Self::UserShell) event replaces the
    /// accumulated chunks with the bounded captured tail, so nothing depends
    /// on these besides the live view.
    UserShellDelta(String),
    Text(String),
    /// A chunk of the model's reasoning / chain-of-thought trace. Surfaced
    /// separately from [`Text`](Self::Text) so the UI can fold it while
    /// still streaming it live.
    Thinking(String),
    ThinkingEnd {
        elapsed_ms: u64,
    },
    ToolStart {
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
        /// Provider-reported stop reason of the turn's final round. Mirrors
        /// the value persisted in the transcript's `TurnEnd` marker.
        stop_reason: Option<lofi_types::StopReason>,
    },
    TurnFailed {
        model: RunModel,
        elapsed_ms: u64,
        error: String,
        cost: f64,
        usage: Usage,
    },
    /// The user interrupted the turn. Completed rounds and any partial
    /// assistant response have already been retained in history and in the
    /// durable transcript.
    TurnCancelled {
        model: RunModel,
        elapsed_ms: u64,
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
    /// A non-fatal notice the consumer may surface as a warning, such as an
    /// image omitted because the active model does not support images.
    /// Live-only: never persisted as a `SessionEvent`, matching
    /// `ContextPressure`.
    Notice(String),
    /// A provider error that ended the run after retry classification.
    /// Transient errors that will be retried use RetryStart/RetryEnd instead,
    /// so they never appear as fatal transcript rows.
    Error(String),
    RetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: String,
    },
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
    RoundCommitted {
        byte_start: u64,
        byte_end: u64,
    },
    TurnCommitted {
        byte_start: u64,
        byte_end: u64,
    },
    Compaction {
        summarized: usize,
        kept: usize,
        summary: String,
    },
}

impl AgentEvent {
    /// Convert a durable user-role prompt into its transcript boundary.
    /// Tool-result messages continue an existing turn and therefore have no
    /// boundary event. Live execution and replay must both use this mapping.
    pub(crate) fn from_prompt(message: &Message) -> Option<Self> {
        if message.role != Role::User
            || message
                .blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        {
            return None;
        }
        let text = message.blocks.iter().find_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        });
        let attachments = message
            .blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { media_type, .. } => Some(format!(
                    "[image: {}]",
                    media_type.strip_prefix("image/").unwrap_or(media_type)
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let prompt = match (text, attachments.is_empty()) {
            (Some(text), true) => text.to_string(),
            (Some("") | None, false) => attachments.join(" "),
            (Some(text), false) => format!("{text}\n{}", attachments.join(" ")),
            (None, true) => String::new(),
        };
        Some(Self::Prompt {
            prompt,
            kind: message.kind,
        })
    }
}
