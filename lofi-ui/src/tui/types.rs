#[allow(clippy::wildcard_imports)]
use super::*;

// Tree rows are produced by the core session projection; the TUI keeps the
// historical TreeEntry name for its picker state.
pub(crate) use lofi_core::session::tree::TreeRow as TreeEntry;

pub(crate) fn detail_block_id(turn: usize, block: usize) -> u64 {
    ((turn as u64) << 32) | block as u64
}

pub(crate) fn assign_detail_ids(turn: &mut Turn, turn_index: usize) {
    for (block_index, block) in turn.blocks.iter_mut().enumerate() {
        let id = detail_block_id(turn_index, block_index);
        match block {
            Block::Tool(tool) => tool.detail_id = id,
            Block::UserShell { id: block_id, .. } => *block_id = id,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(crate) enum DetailKey {
    Exec(u64),
    ExecResult(u64),
    NativeTool { parent: u64, id: u64 },
    UserShell(u64),
}

impl DetailKey {
    pub(crate) fn retains_expansion(&self) -> bool {
        matches!(self, Self::UserShell(_))
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DetailState {
    pub(crate) turn: usize,
    pub(crate) scroll: Option<usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct DetailFocus {
    pub(crate) key: DetailKey,
    pub(crate) cursor: usize,
    pub(crate) col: usize,
}

impl DetailFocus {
    pub(crate) fn on(target: &view::DetailTarget, col: usize) -> Self {
        Self {
            key: target.key.clone(),
            // A released result leaves the collapsed header's total stale, so
            // a tail detail cannot know its last row yet. usize::MAX defers
            // the clamp to the next render, which sees the expanded layout.
            cursor: target
                .row
                .unwrap_or(if target.tail { usize::MAX } else { 0 }),
            col,
        }
    }
}

/// A native tool call (`lofi.bash`/`lofi.read`/…) observed inside an `exec`
/// block, surfaced so the UI can render each one under its parent exec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NativePreview {
    pub(crate) header_suffix: Option<String>,
    pub(crate) lines: Vec<String>,
    pub(crate) total_lines: usize,
    pub(crate) preview_start: usize,
    pub(crate) numbered: bool,
    pub(crate) start_line: usize,
    pub(crate) is_diff: bool,
    pub(crate) notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NativeTool {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) args: String,
    pub(crate) result: Option<String>,
    pub(crate) preview: Option<Box<NativePreview>>,
    pub(crate) is_error: bool,
    pub(crate) done: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub(crate) enum ResultAvailability {
    #[default]
    None,
    Available,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ToolCall {
    #[serde(skip)]
    pub(crate) detail_id: u64,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) input: String,
    pub(crate) label: Option<String>,
    pub(crate) native: Vec<NativeTool>,
    pub(crate) result: Option<String>,
    #[serde(default)]
    pub(crate) result_availability: ResultAvailability,
    pub(crate) result_committed: bool,
    pub(crate) is_error: bool,
    pub(crate) done: bool,
    pub(crate) elapsed: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThinkingBlock {
    pub(crate) text: String,
    #[serde(skip, default = "Instant::now")]
    pub(crate) start: Instant,
    pub(crate) elapsed: Option<Duration>,
}

#[derive(Debug, Clone)]
pub(crate) struct RetryState {
    pub(crate) attempt: u32,
    pub(crate) max_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Block {
    Text(String),
    Thinking(ThinkingBlock),
    Tool(ToolCall),
    UserShell {
        #[serde(skip)]
        id: u64,
        command: String,
        output: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        duration: Duration,
        truncated: bool,
        cancelled: bool,
        /// Live-only: the command is still running and `output` is the
        /// stream accumulated so far. Default `false` (finished) matches any
        /// value serialized before the field existed.
        #[serde(default)]
        running: bool,
        exclude_from_context: bool,
    },
    Error(String),
    TurnEnd {
        label: String,
        elapsed: Duration,
    },
    TurnFailed {
        label: String,
        elapsed: Duration,
        error: String,
    },
    TurnCancelled {
        label: String,
        elapsed: Duration,
    },
    Compaction {
        summarized: usize,
        kept: usize,
        summary: String,
    },
}

/// A prompt waiting to be run: typed input queued while a run is live, or
/// an app-injected notice waking the agent. The kind rides along so the
/// resulting turn renders with the right marker.
#[derive(Debug, Clone)]
pub(crate) struct QueuedPrompt {
    pub text: String,
    pub kind: lofi_types::PromptKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Turn {
    pub(crate) prompt: String,
    /// Where the prompt came from. The kind is not persisted to the session
    /// log, so restored turns always report `User`.
    #[serde(default)]
    pub(crate) kind: lofi_types::PromptKind,
    pub(crate) blocks: Vec<Block>,
}

#[derive(Debug)]
pub(crate) struct SessionState {
    /// Core-owned session write endpoint. `None` in ephemeral mode
    /// (`--no-session`). The UI never assembles or appends session events;
    /// every mutation flows through this sink.
    pub(crate) sink: Option<lofi_core::session::sink::SessionSink>,
    pub(crate) cursor: Option<store::SessionCursor>,
    pub(crate) cwd: PathBuf,
}

impl SessionState {
    pub(crate) fn path(&self) -> Option<&Path> {
        self.cursor.as_ref().map(store::SessionCursor::path)
    }

    /// Attach a cursor to the session owner and refresh the read mirror.
    /// Keeping this as one operation prevents resume from rendering one lineage
    /// while subsequent writes and tree operations still use another.
    pub(crate) fn attach_cursor(&mut self, cursor: store::SessionCursor) {
        if let Some(sink) = self.sink.as_mut() {
            sink.set_cursor(cursor.clone());
        }
        self.cursor = Some(cursor);
    }

    /// Sync `cursor` from the sink after any sink-side mutation. The sink owns
    /// cursor creation and branch moves; this mirror exists only for reads.
    pub(crate) fn refresh_cursor(&mut self) {
        self.cursor = self.sink.as_ref().and_then(|s| s.cursor().cloned());
    }

    /// Borrow the sink for a core-side session mutation. Returns `None` in
    /// ephemeral (`--no-session`) mode, so callers skip session recording.
    pub(crate) fn sink_mut(&mut self) -> Option<&mut lofi_core::session::sink::SessionSink> {
        self.sink.as_mut()
    }
}

pub(crate) struct SessionConfig {
    /// Core-owned session write endpoint; `None` for ephemeral sessions.
    pub(crate) sink: Option<lofi_core::session::sink::SessionSink>,
    pub(crate) cursor: Option<store::SessionCursor>,
    pub(crate) index: Vec<store::SessionIndexEntry>,
    pub(crate) file_size: u64,
    pub(crate) history_start: usize,
    pub(crate) contiguous: bool,
    pub(crate) cwd: PathBuf,
}

impl SessionConfig {
    pub(crate) fn ephemeral(cwd: PathBuf) -> Self {
        Self {
            sink: None,
            cursor: None,
            index: Vec::new(),
            file_size: 0,
            history_start: 0,
            contiguous: true,
            cwd,
        }
    }

    /// Fresh session: no file on disk yet. The sink creates it lazily on the
    /// first run or user bash. `sink` is `None` only for `--no-session`.
    pub(crate) fn fresh(sink: lofi_core::session::sink::SessionSink, cwd: PathBuf) -> Self {
        Self {
            sink: Some(sink),
            cursor: None,
            index: Vec::new(),
            file_size: 0,
            history_start: 0,
            contiguous: true,
            cwd,
        }
    }

    /// Resume an existing session at its selected cursor.
    pub(crate) fn resumed(
        sink: lofi_core::session::sink::SessionSink,
        cursor: store::SessionCursor,
        index: Vec<store::SessionIndexEntry>,
        file_size: u64,
        history_start: usize,
        contiguous: bool,
        cwd: PathBuf,
    ) -> Self {
        Self {
            sink: Some(sink),
            cursor: Some(cursor),
            index,
            file_size,
            history_start,
            contiguous,
            cwd,
        }
    }

    #[must_use]
    pub(crate) fn last_run_model(&self) -> Option<RunModel> {
        self.cursor
            .as_ref()
            .and_then(|cursor| last_run_model_from_index(cursor, &self.index))
    }
}

pub(crate) struct Selection {
    pub(crate) start: (usize, usize),
    pub(crate) end: (usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Input,
    Navigate,
    Select,
}

pub(crate) enum LifecycleOperation {
    Compact,
    AutoCompact(Usage),
    HardCompact,
    Recall(String),
}

pub(crate) enum LifecycleResult {
    Compact(Result<Option<lofi_core::Compaction>>),
    AutoCompact(Result<Option<lofi_core::Compaction>>),
    HardCompact(Result<HardCompactOutcome>),
    Recall {
        line: String,
        result: Result<Option<lofi_core::recall::RecallOutcome>>,
    },
    Failed(String),
}

pub(crate) enum PickerLoad {
    ResumePreviews {
        generation: u64,
        rows: Vec<(usize, String)>,
    },
    ResumeRows {
        generation: u64,
        rows: Vec<(usize, SessionEntry)>,
    },
    TreeReady {
        generation: u64,
        entries: Vec<TreeEntry>,
        /// Handle for the IO-thread-owned snapshot. The full
        /// `Vec<EventIndex>` stays on the worker that built it so its free
        /// happens on the same thread that allocated — sending the Vec itself
        /// would push ~9 MB onto the main thread and leave the freed slack in
        /// the worker's arena on close.
        snapshot_id: u64,
    },
    TreeRows {
        generation: u64,
        rows: Vec<(usize, TreeEntry)>,
    },
    TreeFailed {
        generation: u64,
        error: String,
    },
}
