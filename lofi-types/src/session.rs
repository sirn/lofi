use serde::{Deserialize, Serialize};

use crate::{ContextUsage, Message, NativeToolRecord, RunModel, StopReason, Usage};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    Message(Message),
    /// The context state the engine reported after a step. Spend is on the
    /// turn markers. The old tag stays readable so existing sessions replay.
    #[serde(alias = "backend_usage")]
    ContextReported {
        context: Option<ContextUsage>,
    },
    UserShell {
        command: String,
        output: String,
        exit_code: Option<i32>,
        #[serde(default)]
        signal: Option<i32>,
        duration_ms: u64,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        exclude_from_context: bool,
    },
    /// Wall-clock duration of a completed tool call within a turn, so the
    /// exec block's `took Ns` marker survives resume. `tool_call_id` is the
    /// provider tool-call id; named `tool_call_id` (not `id`) so it does not
    /// collide with the tree-level `id` on [`SessionEvent`] when flattened.
    ToolTiming {
        tool_call_id: String,
        elapsed_ms: u64,
    },
    /// Wall-clock duration of a completed thinking block within a turn, so the
    /// "Thought for Ns" marker survives freeze/resume. Emitted in order, one
    /// per assistant thinking block.
    ThinkingTiming {
        elapsed_ms: u64,
    },
    TurnEnd {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
        /// Stop reason reported for the final round of the turn — the round
        /// that actually ended it. Persisted so finished transcripts
        /// accumulate stop-reason evidence (e.g. how often turns end on a
        /// clean stop versus a token cap).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },
    /// A mid-turn round the engine dropped from the model history (loop
    /// detection, retry re-roll). Its messages were already streamed, so
    /// they stay visible while context rebuild skips the assistant
    /// message(s) this marker ends with.
    RoundDiscarded {
        detail: String,
    },
    /// A turn that ended in a non-retryable provider/runtime failure.
    /// Its partial messages stay on the visible lineage, but context rebuilds
    /// skip them so retrying begins from the preceding successful checkpoint.
    TurnFailed {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        error: String,
        cost: f64,
        usage: Usage,
        /// The turn's messages stay in the model context on replay, bounded by
        /// its discard boundary. Logs written before this field carry false,
        /// which keeps their whole failed turn out of the context.
        #[serde(default)]
        keep_context: bool,
    },
    /// A turn explicitly interrupted by the user.
    /// message, completed rounds and the partial current response remain both
    /// visible and available to subsequent model turns.
    TurnCancelled {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
    },
    /// A native tool call that ran inside an `exec` block, so the nested
    /// `lofi.<tool>` call list survives resume.
    NativeTool(NativeToolRecord),
    Cursor {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leaf_id: Option<String>,
    },
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        summarized_range: [String; 2],
        /// True when the kept tail was copied immediately before this marker
        /// as a durable, context-edited checkpoint. UI replay suppresses those
        /// copies (the original turns remain visible); model-history replay
        /// reads them as the authoritative post-compaction tail. Older marker
        /// layouts default to false.
        #[serde(default)]
        checkpointed_tail: bool,
        summarized: usize,
        #[serde(default)]
        represented: usize,
        kept: usize,
    },
    /// Lineage marker for `jobSpawn`. Invisible to the model and
    /// transcript renderer.
    JobStarted {
        job_id: u64,
    },
    /// Paired with [`Self::JobStarted`] by id; a start with no matching
    /// finish on the visible lineage was still running when the session
    /// last ended.
    JobFinished {
        job_id: u64,
    },
    /// Spend of subagent runs that settled since the last record, as deltas.
    /// Accounting only: it never enters the model context.
    SubagentUsage {
        usage: Usage,
        cost: f64,
    },
    /// Terminal record of a subagent run, appended once at settle (after any
    /// timeout rollback) so it is the leaf of the active branch. A restart
    /// rehydrates the registry entry from it; a transcript whose leaf is not
    /// `RunSettled` belongs to a run the process never settled. Invisible to
    /// the model and the transcript renderer.
    RunSettled {
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
    },
    /// An event variant this build does not know about — typically a marker
    /// written by a newer (or older, pre-release) binary. The transcript
    /// stays loadable; consumers ignore these.
    #[serde(other)]
    Unknown,
}

/// One append-only line in a session transcript log.
/// The first line of a session file is the session header (written by the
/// store); every subsequent line is a `SessionEvent`. Events form a tree via
/// `id`/`parent_id`: each entry points at its parent, the root entry's
/// `parent_id` is `None`, and the "active leaf" is the current position in
/// the tree. Branching appends a new child to an earlier entry instead of to
/// the previous line, so alternatives coexist in one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(flatten)]
    pub kind: SessionEventKind,
}
