//! Sessions: the append-only JSONL transcript and its shared logical cursor
//! are durable state; an `Arc<Mutex<Vec<Message>>` holds the active agent
//! context. The engine checkpoints each round through the cursor, while
//! `--continue`/`--resume` rebuild only the indexed active lineage and the
//! `/resume` picker can switch files mid-run.
//! Agent runs own a separate Tokio runtime thread. Provider work and durable
//! transcript syncs therefore cannot block terminal input or drawing.

mod app_commands;
mod app_input;
mod app_lifecycle;
mod app_nav;
mod app_render;
mod debug_stats;
mod input;
mod replay;
mod resume;
mod text;
mod tree;
mod tty_events;
pub mod view;

#[cfg(test)]
mod tests;

#[allow(clippy::wildcard_imports)]
use {input::*, replay::*, resume::*, text::*, tree::*};

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use lofi_core::session::store::{self, SessionEntry};
use lofi_types::{
    ContentBlock, Message, PromptKind, Role, RunModel, ServiceTier, SessionEvent, SessionEventKind,
    ThinkingLevel, Usage,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};

use base64::Engine;
use futures::FutureExt as _;

pub(crate) mod terminal_bg;
pub(crate) mod theme;
use std::time::Instant;
pub(crate) use theme::Theme;
use tokio::sync::mpsc::Receiver;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::MissedTickBehavior;

use crate::tui::view::HStack;
use lofi_core::{Agent, AgentEvent, AgentLifecycle, HardCompactOutcome};
use lofi_error::{Error, Result};

/// Retained model registry + config so `/model` can rebuild the agent
/// mid-session without re-running remote discovery. The registry is taken
/// from the startup `build_agent` call; `rebuild` is sync and side-effect-free
/// beyond provider construction, so a switch never blocks the UI on the
/// network. `choices` backs the `/model` picker.
pub(crate) struct ModelSwitcher {
    registry: lofi_core::ModelRegistry,
    config: lofi_types::Config,
    root: PathBuf,
    env: Vec<(String, String)>,
    choices: Vec<lofi_types::ModelChoice>,
}

impl ModelSwitcher {
    pub(crate) fn new(
        registry: lofi_core::ModelRegistry,
        config: lofi_types::Config,
        root: PathBuf,
        env: Vec<(String, String)>,
    ) -> Self {
        let choices = registry.choices();
        Self {
            registry,
            config,
            root,
            env,
            choices,
        }
    }

    pub(crate) fn choices(&self) -> &[lofi_types::ModelChoice] {
        &self.choices
    }

    pub(crate) fn rebuild(
        &self,
        existing: Option<&Agent>,
        query: &str,
    ) -> Result<(Agent, lofi_types::Model, ThinkingLevel)> {
        lofi_core::rebuild_agent(
            existing,
            &self.registry,
            &self.config,
            Some(query),
            &self.root,
            &self.env,
        )
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK_MS: u64 = 60;
const RESIZE_DEBOUNCE_MS: u64 = 50;
const AUTO_MODE_UI_GRACE: Duration = Duration::from_secs(3);
const YANK_NOTIFY: Duration = Duration::from_secs(2);
const NOTIFY_TTL: Duration = Duration::from_secs(5);
/// Cap on the notification area height: a long transient message wraps
/// across up to this many rows instead of truncating to one.
const NOTIFY_MAX_LINES: usize = 3;
/// Byte budget for the plain-job output viewer. PTY jobs use their bounded
/// terminal screen instead.
const JOB_LOG_WINDOW_BYTES: usize = 128 * 1024;
const MAX_INPUT_LINES: usize = 8;
const QUIT_DOUBLE_PRESS: Duration = Duration::from_secs(2);
/// How long quit waits for the agent to flush the current turn. Far beyond
/// any healthy settle: provider rounds are bounded by the cancel flag and
/// the stream idle timeout, and the final flush is local disk IO. Quitting
/// by abort is the one sanctioned transcript loss (the process giving up),
/// so the budget errs toward waiting it out.
const QUIT_FLUSH_TIMEOUT: Duration = Duration::from_mins(1);
const DEFAULT_CTX_LIMIT: u64 = 200_000;
const DETAIL_VIEW_ROWS: usize = 10;

fn detail_block_id(turn: usize, block: usize) -> u64 {
    ((turn as u64) << 32) | block as u64
}

fn assign_detail_ids(turn: &mut Turn, turn_index: usize) {
    for (block_index, block) in turn.blocks.iter_mut().enumerate() {
        let id = detail_block_id(turn_index, block_index);
        match block {
            Block::Tool(tool) => tool.detail_id = id,
            Block::UserShell { id: block_id, .. } => *block_id = id,
            _ => {}
        }
    }
}

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "clear the transcript log"),
    ("/compact", "fold older history into a summary"),
    ("/debug", "toggle resource diagnostics"),
    ("/exit", "exit lofi"),
    ("/help", "show keybindings and commands"),
    ("/job", "list background jobs, view output, stop a job"),
    ("/new", "start a fresh session"),
    ("/quit", "exit lofi"),
    ("/resume", "pick a past session to resume"),
    ("/session", "show session info"),
    ("/tree", "roll back to a past turn"),
    ("/model", "switch the active model"),
    ("/policy", "change bash approval mode for this session"),
    ("/service", "switch the service tier"),
    ("/theme", "switch color scheme for this session"),
    ("/thinking", "switch the thinking level"),
];

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub(crate) enum DetailKey {
    Exec(u64),
    ExecResult(u64),
    NativeTool { parent: u64, id: u64 },
    UserShell(u64),
}

impl DetailKey {
    fn retains_expansion(&self) -> bool {
        matches!(self, Self::UserShell(_))
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct DetailState {
    turn: usize,
    scroll: Option<usize>,
}

#[derive(Debug, Clone)]
struct DetailFocus {
    key: DetailKey,
    cursor: usize,
    col: usize,
}

impl DetailFocus {
    fn on(target: &view::DetailTarget, col: usize) -> Self {
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
struct NativePreview {
    header_suffix: Option<String>,
    lines: Vec<String>,
    total_lines: usize,
    preview_start: usize,
    numbered: bool,
    start_line: usize,
    is_diff: bool,
    notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NativeTool {
    id: u64,
    name: String,
    args: String,
    result: Option<String>,
    preview: Option<Box<NativePreview>>,
    is_error: bool,
    done: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
enum ResultAvailability {
    #[default]
    None,
    Available,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCall {
    #[serde(skip)]
    detail_id: u64,
    id: String,
    name: String,
    input: String,
    label: Option<String>,
    native: Vec<NativeTool>,
    result: Option<String>,
    #[serde(default)]
    result_availability: ResultAvailability,
    result_committed: bool,
    is_error: bool,
    done: bool,
    elapsed: Option<Duration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThinkingBlock {
    text: String,
    #[serde(skip, default = "Instant::now")]
    start: Instant,
    elapsed: Option<Duration>,
}

#[derive(Debug, Clone)]
struct RetryState {
    attempt: u32,
    max_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Block {
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
    prompt: String,
    /// Where the prompt came from. The kind is not persisted to the session
    /// log, so restored turns always report `User`.
    #[serde(default)]
    kind: lofi_types::PromptKind,
    blocks: Vec<Block>,
}

#[derive(Debug)]
struct SessionState {
    /// Core-owned session write endpoint. `None` in ephemeral mode
    /// (`--no-session`). The UI never assembles or appends session events;
    /// every mutation flows through this sink.
    sink: Option<lofi_core::session::sink::SessionSink>,
    cursor: Option<store::SessionCursor>,
    cwd: PathBuf,
}

impl SessionState {
    fn path(&self) -> Option<&Path> {
        self.cursor.as_ref().map(store::SessionCursor::path)
    }

    /// Attach a cursor to the session owner and refresh the read mirror.
    /// Keeping this as one operation prevents resume from rendering one lineage
    /// while subsequent writes and tree operations still use another.
    fn attach_cursor(&mut self, cursor: store::SessionCursor) {
        if let Some(sink) = self.sink.as_mut() {
            sink.set_cursor(cursor.clone());
        }
        self.cursor = Some(cursor);
    }

    /// Sync `cursor` from the sink after any sink-side mutation. The sink owns
    /// cursor creation and branch moves; this mirror exists only for reads.
    fn refresh_cursor(&mut self) {
        self.cursor = self.sink.as_ref().and_then(|s| s.cursor().cloned());
    }

    /// Borrow the sink for a core-side session mutation. Returns `None` in
    /// ephemeral (`--no-session`) mode, so callers skip session recording.
    fn sink_mut(&mut self) -> Option<&mut lofi_core::session::sink::SessionSink> {
        self.sink.as_mut()
    }
}

pub(crate) struct SessionConfig {
    /// Core-owned session write endpoint; `None` for ephemeral sessions.
    sink: Option<lofi_core::session::sink::SessionSink>,
    cursor: Option<store::SessionCursor>,
    index: Vec<store::SessionIndexEntry>,
    file_size: u64,
    history_start: usize,
    contiguous: bool,
    cwd: PathBuf,
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

#[derive(Debug, Clone)]
struct PickerEntry {
    file: store::SessionFile,
    preview: Option<String>,
    details: Option<SessionEntry>,
}

#[derive(Debug, Clone)]
struct PickerState {
    entries: Vec<PickerEntry>,
    selected: usize,
    generation: u64,
}

/// `scroll` is the top visible wrapped-line index; `total` and `view_h`
/// are filled by the renderer each frame so the key handler can clamp and
/// page without knowing the terminal size itself.
#[derive(Debug, Clone)]
struct InfoModal {
    title: String,
    lines: Vec<InfoLine>,
    scroll: usize,
    total: usize,
    view_h: usize,
}

#[derive(Debug, Clone)]
enum InfoLine {
    Section(String),
    Text(Line<'static>),
}

impl InfoModal {
    fn max_scroll(&self) -> usize {
        self.total.saturating_sub(self.view_h)
    }

    fn scroll_down(&mut self) {
        self.scroll = (self.scroll + 1).min(self.max_scroll());
    }

    fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    fn scroll_page_down(&mut self) {
        self.scroll = (self.scroll + self.view_h.max(1)).min(self.max_scroll());
    }

    fn scroll_page_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(self.view_h.max(1));
    }
}

fn info_section(label: &str) -> InfoLine {
    InfoLine::Section(label.to_string())
}

/// `key  value` row: key bold in `fg`, value muted, key padded to a fixed
/// column so the values line up. Longer keys just overflow the column.
fn info_kv(t: Theme, key: &str, value: &str) -> InfoLine {
    const COL: usize = 12;
    let pad = COL.saturating_sub(key.chars().count());
    InfoLine::Text(Line::from(vec![
        Span::styled(
            format!("{}{}", key, " ".repeat(pad)),
            Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_string(), Style::new().fg(t.muted)),
    ]))
}

fn info_note(t: Theme, text: &str) -> InfoLine {
    InfoLine::Text(Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(t.muted),
    )))
}

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", n, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyKind {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone)]
struct Notify {
    msg: String,
    kind: NotifyKind,
    at: Instant,
}

/// `j`/`k`/`q` remain printable because this popover overlays text input.
#[derive(Debug, Clone)]
struct SlashComplete {
    candidates: Vec<usize>,
    selected: usize,
}

#[derive(Debug, Clone)]
struct TreePickerState {
    entries: Vec<TreeEntry>,
    selected: usize,
    generation: u64,
    loading: bool,
}

/// Confirmation stays outside this trait because borrowing a modal and
/// `&mut App` together would conflict.
trait Modal {
    fn len(&self) -> usize;
    fn selected(&self) -> usize;
    fn set_selected(&mut self, n: usize);
}

trait Popover {
    fn len(&self) -> usize;
    fn selected(&self) -> usize;
    fn set_selected(&mut self, n: usize);
}

impl Modal for PickerState {
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

impl Modal for TreePickerState {
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

struct ModelPickerState {
    choices: Vec<lofi_types::ModelChoice>,
    selected: usize,
}

impl Modal for ModelPickerState {
    fn len(&self) -> usize {
        self.choices.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

struct ThinkingPickerState {
    levels: Vec<ThinkingLevel>,
    selected: usize,
}

struct ServicePickerState {
    tiers: Vec<ServiceTier>,
    selected: usize,
}

struct ThemePickerState {
    modes: [lofi_types::ThemeMode; 3],
    selected: usize,
}

/// State for the `/policy` modal: the session-scoped bash approval modes
/// the user can pick. `AskAuto` is offered only when auto mode is
/// configured.
struct PolicyPickerState {
    modes: Vec<lofi_types::BashApprovalMode>,
    selected: usize,
}

impl ThemePickerState {
    const MODES: [lofi_types::ThemeMode; 3] = [
        lofi_types::ThemeMode::Auto,
        lofi_types::ThemeMode::Light,
        lofi_types::ThemeMode::Dark,
    ];
}

/// State for the `/job` modal. The list is rebuilt from a fresh
/// [`lofi_core::JobRegistry::snapshot`] each render, so only `selected` and
/// the drill-in output view are kept here. Plain-job logs page bounded
/// windows from disk, so an unbounded log never inflates memory.
struct JobsModalState {
    selected: usize,
    /// Set while drilling into one job's output; `Esc` returns to the list.
    viewing: Option<JobOutputView>,
    /// Armed when the user picks kill: the next `y` confirms, anything else
    /// cancels. Holds the target job id so the confirm survives a re-render.
    confirm_kill: Option<u64>,
}

/// Drill-in output view for one job. PTY jobs show their parsed terminal
/// screen. Plain jobs retain a bounded, scrollable log tail.
struct JobOutputView {
    id: u64,
    content: JobViewContent,
}

enum JobViewContent {
    Terminal(lofi_core::JobScreen),
    Log {
        lines: std::collections::VecDeque<String>,
        cursor: u64,
        total: u64,
        scroll: usize,
    },
}

impl JobViewContent {
    fn empty_log() -> Self {
        Self::Log {
            lines: std::collections::VecDeque::new(),
            cursor: 0,
            total: 0,
            scroll: 0,
        }
    }
}

impl Modal for ThinkingPickerState {
    fn len(&self) -> usize {
        self.levels.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

impl Modal for ServicePickerState {
    fn len(&self) -> usize {
        self.tiers.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

impl Modal for PolicyPickerState {
    fn len(&self) -> usize {
        self.modes.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

fn policy_mode_label(mode: lofi_types::BashApprovalMode) -> &'static str {
    match mode {
        lofi_types::BashApprovalMode::AllowAll => "allow all",
        lofi_types::BashApprovalMode::AskManual => "ask (manual)",
        lofi_types::BashApprovalMode::AskAuto => "ask (auto)",
        lofi_types::BashApprovalMode::DenyAll => "deny all",
    }
}

impl Modal for ThemePickerState {
    fn len(&self) -> usize {
        self.modes.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

impl Popover for SlashComplete {
    fn len(&self) -> usize {
        self.candidates.len()
    }
    fn selected(&self) -> usize {
        self.selected
    }
    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

// Tree rows are produced by the core session projection; the TUI keeps the
// historical TreeEntry name for its picker state.
use lofi_core::session::tree::TreeRow as TreeEntry;

/// Retains compressed, width-independent display models so scrolling back
/// into a resumed transcript does not rescan its JSONL file. Successful
/// native results are reduced to their collapsed previews before insertion;
/// per-turn compression avoids retaining the full Rust object graph. Entries
/// are evicted least-recently-used past a hard budget, viewport-driven
/// materializations being the only inserts — background height re-measure
/// parses without caching, so the cache stays proportional to what the user
/// has actually looked at, not to the whole transcript.
struct CollapsedTurnCache {
    map: HashMap<usize, Box<[u8]>>,
    /// LRU order: front is the least recently used turn index.
    order: VecDeque<usize>,
    retained_bytes: usize,
    frame_hits: usize,
    frame_misses: usize,
    frame_materialize_us: u128,
}

impl CollapsedTurnCache {
    const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
    /// A single entry above this size is never retained; it reparses from the
    /// transcript on demand instead of flushing everyone else.
    const MAX_ENTRY_BYTES: usize = 1024 * 1024;

    fn new() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            retained_bytes: 0,
            frame_hits: 0,
            frame_misses: 0,
            frame_materialize_us: 0,
        }
    }

    fn get(&mut self, idx: usize) -> Option<Arc<Turn>> {
        let turn = self.map.get(&idx).and_then(|data| {
            lz4_flex::decompress_size_prepended(data)
                .ok()
                .and_then(|json| serde_json::from_slice(&json).ok())
                .map(|mut turn| {
                    assign_detail_ids(&mut turn, idx);
                    Arc::new(turn)
                })
        });
        if turn.is_some() {
            self.frame_hits += 1;
            if let Some(pos) = self.order.iter().position(|entry| *entry == idx) {
                self.order.remove(pos);
            }
            self.order.push_back(idx);
        } else {
            self.frame_misses += 1;
        }
        turn
    }

    fn finish_materialize(&mut self, elapsed_us: u128) {
        self.frame_materialize_us += elapsed_us;
    }

    fn reset_frame_profile(&mut self) {
        self.frame_hits = 0;
        self.frame_misses = 0;
        self.frame_materialize_us = 0;
    }

    fn insert(&mut self, idx: usize, turn: &Turn) {
        if self.map.contains_key(&idx) {
            return;
        }
        let Ok(json) = serde_json::to_vec(turn) else {
            return;
        };
        let data = lz4_flex::compress_prepend_size(&json).into_boxed_slice();
        let bytes = data.len();
        if bytes > Self::MAX_ENTRY_BYTES {
            return;
        }
        self.retained_bytes += bytes;
        self.order.push_back(idx);
        self.map.insert(idx, data);
        while self.retained_bytes > Self::MAX_RETAINED_BYTES {
            let Some(victim) = self.order.pop_front() else {
                break;
            };
            if let Some(data) = self.map.remove(&victim) {
                self.retained_bytes = self.retained_bytes.saturating_sub(data.len());
            }
        }
    }

    fn remove(&mut self, idx: usize) {
        if let Some(data) = self.map.remove(&idx) {
            self.retained_bytes = self.retained_bytes.saturating_sub(data.len());
            if let Some(pos) = self.order.iter().position(|entry| *entry == idx) {
                self.order.remove(pos);
            }
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
        self.retained_bytes = 0;
        self.reset_frame_profile();
    }
}

/// Retains only viewport-adjacent rendering to avoid holding the styled
/// representation of the full session.
struct FrozenCache {
    order: VecDeque<usize>,
    map: HashMap<usize, Vec<view::RenderLine>>,
}

impl FrozenCache {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    fn contains(&self, idx: usize) -> bool {
        self.map.contains_key(&idx)
    }

    fn get(&self, idx: usize) -> Option<&Vec<view::RenderLine>> {
        self.map.get(&idx)
    }

    fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }

    fn insert(&mut self, idx: usize, entry: Vec<view::RenderLine>) {
        if self.map.insert(idx, entry).is_none() {
            self.order.push_back(idx);
        }
    }

    fn retain_near(&mut self, visible: Option<(usize, usize)>, frozen_turns: usize) {
        let Some((first, last)) = visible else {
            self.clear();
            return;
        };
        let first = first.saturating_sub(1);
        let last = last.saturating_add(1).min(frozen_turns.saturating_sub(1));
        self.map.retain(|idx, _| (first..=last).contains(idx));
        self.order.retain(|idx| (first..=last).contains(idx));
    }
}

struct Selection {
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Input,
    Navigate,
    Select,
}

enum LifecycleOperation {
    Compact,
    AutoCompact(Usage),
    HardCompact,
    Recall(String),
}

enum LifecycleResult {
    Compact(Result<Option<lofi_core::Compaction>>),
    AutoCompact(Result<Option<lofi_core::Compaction>>),
    HardCompact(Result<HardCompactOutcome>),
    Recall {
        line: String,
        result: Result<Option<lofi_core::recall::RecallOutcome>>,
    },
    Failed(String),
}

enum PickerLoad {
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

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct App {
    turns: Vec<Turn>,
    /// Per-turn byte range in the transcript file (`None` for ephemeral or
    /// not-yet-committed turns). Parallel to `turns`. A frozen turn's blocks
    /// are cleared once committed; `materialize_turn` re-parses this range
    /// from the file on demand so the UI's memory stays bounded by the
    /// viewport, not the session length.
    turn_byte_ranges: Vec<Option<(u64, u64)>>,
    turn_event_offsets: Vec<Option<Vec<u64>>>,
    input: String,
    input_cursor: usize,
    lifecycle: AgentLifecycle,
    /// The system prompt the agent was built with; pinned onto the durable
    /// transcript at each context boundary so a resumed or compacted history
    /// replays the exact governing prompt rather than what today's config
    /// would produce.
    system_prompt: String,
    history_nav: Vec<String>,
    history_idx: Option<usize>,
    input_stash: String,
    model_label: String,
    thinking_label: Option<String>,
    thinking: ThinkingLevel,
    service_label: Option<String>,
    service_tier: ServiceTier,
    status_usage: Option<Usage>,
    total_in: u64,
    total_out: u64,
    prompt_queue: Vec<QueuedPrompt>,
    /// Stale-job notices computed at session startup, folded into the
    /// history of the first agent run after resume. Consumed on first use.
    startup_notices: Vec<String>,
    cost: f64,
    turn_cost: f64,
    turn_has_round_usage: bool,
    ctx_limit: u64,
    /// True when a settled round supplied usage that core policy has not
    /// evaluated yet.
    settled_usage_fresh: bool,
    compacted: bool,
    /// Set by `ContextPressure` when the engine force-stopped the run at the
    /// hard context cap. The run loop reads (and clears) it on channel close
    /// to drive the force-compact + silent continue, instead of the soft
    /// `agent_settled` path.
    context_pressure: bool,
    run: Option<usize>,
    run_start: Option<Instant>,
    /// Model label captured when the run started. A mid-run `/model` switch
    /// updates the footer (the newly set model); the "Working for ..." line
    /// must keep showing the model the in-flight run is actually using.
    run_model_label: Option<String>,
    retry: Option<RetryState>,
    /// True between the submit-time prompt pre-push and the engine's
    /// matching `Prompt` event. Gates the dedup in `apply_event` so replayed
    /// transcripts (where consecutive identical prompts are legal)
    /// never false-match the pre-pushed turn.
    pending_prompt_start: bool,
    lifecycle_tx: Option<tokio::sync::mpsc::UnboundedSender<LifecycleResult>>,
    lifecycle_busy: bool,
    continue_after_hard_compact: bool,
    pinned: bool,
    top_line: usize,
    last_base: usize,
    expanded_details: HashMap<DetailKey, DetailState>,
    detail_focus: Option<DetailFocus>,
    debug_after_draw: Option<&'static str>,
    debug: Option<debug_stats::DebugState>,
    should_quit: bool,
    session: SessionState,
    picker: Option<PickerState>,
    tree_picker: Option<TreePickerState>,
    tree_picker_snapshot: Option<u64>,
    tree_picker_pending: std::collections::HashSet<usize>,
    picker_load_tx: Option<tokio::sync::mpsc::UnboundedSender<PickerLoad>>,
    picker_generation: Arc<AtomicU64>,
    model_picker: Option<ModelPickerState>,
    thinking_picker: Option<ThinkingPickerState>,
    policy_picker: Option<PolicyPickerState>,
    /// Shared handle to the session's bash approval override; written on
    /// `/policy` confirm. None until the startup agent is wired in.
    policy_override: Option<lofi_core::PolicyOverride>,
    /// Whether auto-mode evaluation is configured; gates the `ask (auto)`
    /// choice in the `/policy` dialog.
    auto_mode_configured: bool,
    service_picker: Option<ServicePickerState>,
    theme_picker: Option<ThemePickerState>,
    /// The session's background-job registry, cloned from the agent at
    /// startup. `None` when no agent is configured. Backs the `/job` modal
    /// and the persistent running-jobs badge.
    jobs: Option<lofi_core::JobRegistry>,
    /// Set when a session switch resets the shared registry. The run loop
    /// replaces its receiver so notices already delivered for the old scope
    /// cannot enter the new session.
    jobs_receiver_stale: bool,
    jobs_modal: Option<JobsModalState>,
    model_choices: Vec<lofi_types::ModelChoice>,
    pending_model_switch: Option<String>,
    info: Option<InfoModal>,
    slash_complete: Option<SlashComplete>,
    no_models_hint: Option<String>,
    theme: Theme,
    theme_mode: lofi_types::ThemeMode,
    kill_ring: String,
    /// True when the previous command was `C-k` so a consecutive `C-k`
    /// appends to the kill ring instead of replacing it.
    last_kill_was_kill: bool,
    ctrl_c_at: Option<Instant>,
    log_rect: Rect,
    input_rect: Rect,
    log_vis: Vec<view::VisLine>,
    log_details: Vec<Option<view::DetailTarget>>,
    log_off: usize,
    input_scroll: usize,
    sel: Option<Selection>,
    mode: Mode,
    pending_confirms: Vec<lofi_core::ConfirmRequest>,
    deferred_confirms: Vec<lofi_core::ConfirmRequest>,
    confirm_selected: usize,
    confirm_scroll: usize,
    confirm_total: usize,
    confirm_view_h: usize,
    yank_notify: Option<Instant>,
    /// Cursor position saved at yank time so the next `enter_nav` can jump
    /// back to it instead of the bottom of the viewport. `None` when the
    /// cursor was on the last line (follow transcript) or no yank happened.
    yank_cursor: Option<(usize, usize)>,
    /// Transient status/error notification from a slash command (e.g.
    /// `/session` with no session, `/tree` with no session file, an unknown
    /// command). Surfaced on the rule line's left edge instead of as a chat
    /// turn so command feedback doesn't pollute the transcript.
    notify: Option<Notify>,
    nav_cursor: usize,
    nav_col: usize,
    select_anchor: (usize, usize),
    /// Total transcript line count (frozen + last turn + separators), stashed
    /// at render time so the Navigate cursor can be clamped between events.
    log_total: usize,
    last_turn_height: usize,
    log_view_h: usize,
    frozen_render: FrozenCache,
    collapsed_turns: Box<RefCell<CollapsedTurnCache>>,
    /// First durable-transcript read failure seen while rendering. Set from
    /// interior-mutability paths that only hold `&App`; surfaced as a
    /// notification after the frame so a turn silently rendering as an
    /// empty shell gets a visible explanation instead of looking like
    /// lost transcript.
    transcript_alert: RefCell<Option<String>>,
    /// Line count per frozen turn (all of them), so the viewport can be
    /// located and `total` computed without fetching rendered lines. Synced
    /// to the file-backed prefix (which may be all turns) for the active mode.
    frozen_heights: Vec<usize>,
    /// Parallel to `frozen_heights`: true while the entry is a placeholder
    /// estimate derived from the turn byte range, still waiting for an exact
    /// re-measure by the tick loop or the viewport pass. Resumed transcripts
    /// seed estimates for every historical turn so the first frame paints
    /// without re-reading the whole session file.
    frozen_heights_estimated: Vec<bool>,
    render_epoch: u64,
    frozen_epoch: u64,
    /// Viewport width the frozen cache was last built at. A resize changes
    /// the wrap width, so a mismatch discards the cache just like an epoch
    /// bump — otherwise background-padded lines keep the old (narrower)
    /// width after the terminal grows.
    frozen_width: usize,
    /// After a width-only resize, frozen turns below this exclusive upper
    /// bound still hold heights measured at the previous width. `ensure_frozen`
    /// keeps those stale heights as the total/base source (they only differ by
    /// re-wrap, so the thumb and bottom anchor stay stable); the tick loop
    /// re-measures them back-to-front — visible bottom turns first — so a
    /// resize never stalls a frame on a long transcript. `None` once every
    /// height matches `frozen_width`.
    height_remeasure_from: Option<usize>,
    render_profile: Box<RenderProfile>,
}

impl App {
    fn restore_indexed_session(
        &mut self,
        cursor: &store::SessionCursor,
        index: &[store::SessionIndexEntry],
        file_size: u64,
        history_start: usize,
        contiguous: bool,
    ) -> Result<()> {
        self.lifecycle
            .restore_history(cursor, index, history_start)?;
        self.turns.clear();
        self.expanded_details.clear();
        self.detail_focus = None;
        self.collapsed_turns.get_mut().clear();
        self.turn_byte_ranges.clear();
        self.turn_event_offsets.clear();
        self.cost = 0.0;
        self.total_in = 0;
        self.total_out = 0;
        self.reset_compaction_gauges();
        replay_indexed_session(self, cursor, index, file_size, contiguous)?;
        // Replay parses the whole transcript and drops most of it again; the
        // freed heap sits in the arena otherwise.
        lofi_core::release_freed_memory();

        restore_compaction_from_index(self, cursor, index);
        Ok(())
    }
}

/// Cancel an in-flight run and wait for its recorder to flush. Aborting the
/// task skips that flush, so the deadline sits far beyond any healthy
/// round; the engine's cancel flag and idle timeout bound the wait on
/// their own. Returns true when aborted by deadline — quitting must not
/// silently lose the un-flushed suffix.
async fn settle_run_for_quit(run: RunHandle, timeout: Duration) -> bool {
    run.cancel.store(true, Ordering::Relaxed);
    let RunHandle { handle, mut rx, .. } = run;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut aborted_by_deadline = false;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                if ev.is_none() {
                    break;
                }
            }
            () = tokio::time::sleep_until(deadline) => {
                aborted_by_deadline = true;
                break;
            },
        }
    }
    if aborted_by_deadline {
        handle.abort();
    } else {
        let _ = handle.await;
    }
    aborted_by_deadline
}

struct RunHandle {
    handle: JoinHandle<()>,
    rx: Receiver<AgentEvent>,
    cancel: Arc<AtomicBool>,
    /// When set, the agent exits its round loop gracefully after the current
    /// round finishes (tool results in hand) so the TUI can pop the next
    /// queued prompt at the earliest opportunity — between rounds, not after
    /// the entire multi-round turn.
    preempt: Arc<AtomicBool>,
    session_cursor: Arc<std::sync::Mutex<Option<store::SessionCursor>>>,
    user_shell: Option<(String, bool)>,
}

fn attach_run_cursor(app: &mut App, run: &RunHandle) {
    if app.session.cursor.is_some() {
        return;
    }
    let cursor = run
        .session_cursor
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let Some(cursor) = cursor {
        app.session.attach_cursor(cursor);
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct RenderProfile {
    resize_events: usize,
    resize_batch_us: u128,
    resize_quiet_us: u128,
    width: usize,
    height: usize,
    turns: usize,
    frozen_turns: usize,
    collapsed_cache_hits: usize,
    collapsed_cache_misses: usize,
    collapsed_cache_entries: usize,
    collapsed_cache_bytes: usize,
    materialize_us: u128,
    width_changed: bool,
    ensure_frozen_us: u128,
    live_height_us: u128,
    viewport_cache_us: u128,
    frozen_window_us: u128,
    live_window_us: u128,
    log_total_us: u128,
}

#[derive(Debug, Default)]
struct ResizeState {
    deadline: Option<tokio::time::Instant>,
    started: Option<Instant>,
    last: Option<Instant>,
    events: usize,
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn draw(
        &mut self,
        app: &mut App,
        resize_events: usize,
        resize_started: Option<Instant>,
        resize_last: Option<Instant>,
    ) -> Result<()> {
        app.collapsed_turns.get_mut().reset_frame_profile();
        *app.render_profile = RenderProfile {
            resize_events,
            resize_batch_us: resize_started.map_or(0, |at| at.elapsed().as_micros()),
            resize_quiet_us: resize_last.map_or(0, |at| at.elapsed().as_micros()),
            ..RenderProfile::default()
        };
        let draw_started = Instant::now();
        let mut render_us = 0;
        let frame = self
            .terminal
            .draw(|f| {
                let render_started = Instant::now();
                view::render(f, app);
                render_us = render_started.elapsed().as_micros();
            })
            .map_err(Error::Io)?;
        let draw_us = draw_started.elapsed().as_micros();
        {
            let cache = app.collapsed_turns.get_mut();
            app.render_profile.collapsed_cache_hits = cache.frame_hits;
            app.render_profile.collapsed_cache_misses = cache.frame_misses;
            app.render_profile.collapsed_cache_entries = cache.map.len();
            app.render_profile.collapsed_cache_bytes = cache.retained_bytes;
            app.render_profile.materialize_us = cache.frame_materialize_us;
        }
        app.debug_render_timing(frame.area.width, frame.area.height, draw_us, render_us);
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableBracketedPaste,
            DisableFocusChange,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        tty_events::set_reports_enabled(false);
        let _ = disable_raw_mode();
    }
}

/// `agent` is `None` when no model is configured: the UI still launches and
/// shows `no_models_hint` in the log; submitting a prompt re-surfaces the
/// hint instead of running.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run(
    agent: Option<Agent>,
    model_label: String,
    thinking: ThinkingLevel,
    service_tier: ServiceTier,
    ui_theme: lofi_types::ThemeMode,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    switcher: Option<ModelSwitcher>,
    system_prompt: String,
) -> Result<()> {
    tty_events::ensure_terminal_input().map_err(Error::Io)?;
    enable_raw_mode().map_err(Error::Io)?;
    // `Theme::resolve(Auto)` probes via OSC 11; that requires raw mode
    // (see terminal_bg module doc for why).
    let theme = Theme::resolve(ui_theme);
    let setup = (|| -> std::io::Result<_> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableFocusChange,
            EnableBracketedPaste
        )?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    let terminal = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = execute!(
                io::stdout(),
                DisableBracketedPaste,
                DisableFocusChange,
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            return Err(Error::Io(e));
        }
    };

    let mut guard = TerminalGuard { terminal };
    let local = LocalSet::new();
    let result = std::panic::AssertUnwindSafe(local.run_until(async move {
        // `run_loop` exceeds clippy's large_futures stack limit.
        Box::pin(run_loop(
            &mut guard,
            agent,
            theme,
            ui_theme,
            model_label,
            thinking,
            service_tier,
            session,
            no_models_hint,
            ctx_limit,
            compaction,
            switcher,
            system_prompt,
        ))
        .await
    }))
    .catch_unwind()
    .await;
    match result {
        Ok(result) => result,
        Err(payload) => Err(Error::State(format!(
            "TUI panicked: {}",
            panic_message(payload.as_ref())
        ))),
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_loop(
    guard: &mut TerminalGuard,
    mut agent: Option<Agent>,
    theme: Theme,
    theme_mode: lofi_types::ThemeMode,
    model_label: String,
    thinking: ThinkingLevel,
    service_tier: ServiceTier,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    switcher: Option<ModelSwitcher>,
    system_prompt: String,
) -> Result<()> {
    let SessionConfig {
        sink,
        cursor,
        index,
        file_size,
        history_start,
        contiguous,
        cwd,
    } = session;
    let model_choices = switcher
        .as_ref()
        .map_or(Vec::new(), |s| s.choices().to_vec());
    let mut app = App::new(
        model_label,
        thinking,
        service_tier,
        ctx_limit,
        compaction,
        system_prompt,
    );
    app.theme = theme;
    app.theme_mode = theme_mode;
    app.model_choices = model_choices;
    app.session = SessionState { sink, cursor, cwd };
    if let Some(cursor) = app.session.cursor.clone() {
        app.restore_indexed_session(&cursor, &index, file_size, history_start, contiguous)?;
        // Jobs whose started marker is on this lineage but whose terminal
        // marker is not (process died, or user /tree'd a fresh branch
        // elsewhere). Their ids are stale; surface that on the first agent
        // turn after resume so the model does not try to poll them. Reuses
        // the already-built lineage index — no second file scan.
        let outstanding =
            lofi_core::session::replay::outstanding_job_ids_at(&cursor, &index).unwrap_or_default();
        if !outstanding.is_empty() {
            let ids = outstanding
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            app.startup_notices.push(format!(
                "session resumed: jobs [{ids}] from the previous run are no longer running; their ids are stale. Use jobSpawn for new background work."
            ));
        }
    }
    // The resume index is startup scratch. All persistent UI backing uses the
    // compact turn ranges/offsets built above and opens a fresh cursor snapshot
    // only for branch operations. Explicitly drop it before the event loop so
    // an async state-machine frame cannot retain thousands of ID strings.
    drop(index);
    lofi_core::release_freed_memory();
    // Resume replay creates file-backed shells for every historical turn,
    // including the final one. Viewport materialization owns the bounded
    // display working set from this point onward.
    app.bump_render_epoch();
    app.no_models_hint = no_models_hint.clone();
    if let Some(hint) = no_models_hint {
        // Lead with the hint so the user sees why nothing will run.
        app.insert_turn(
            0,
            Turn {
                prompt: String::new(),
                kind: lofi_types::PromptKind::User,
                blocks: vec![Block::Error(hint)],
            },
        );
    }

    app.enable_debug_from_env();
    if app.debug.is_some() {
        app.debug_after_draw = Some("initial_draw");
    }

    let (confirm_tx, mut confirm_rx) =
        tokio::sync::mpsc::unbounded_channel::<lofi_core::ConfirmRequest>();
    if let Some(a) = agent.take() {
        let a = a.with_confirm_tx(confirm_tx);
        // Clone the session's job registry for the `/job` modal and the
        // running-jobs badge. Cheap: shares the agent's map.
        app.jobs = Some(a.jobs());
        // The `/policy` dialog writes the choice here; every exec in the
        // session reads it. Cloned (not moved) so model switches keep it.
        app.policy_override = Some(a.policy_override().clone());
        app.auto_mode_configured = a.has_auto_mode();
        agent = Some(a);
    }

    // Live notice feed. The job driver pushes onto this the moment a job
    // transitions; the select arm below reacts without waiting for a tick.
    // Buffered pre-UI notices flush into this receiver on subscribe.
    let mut job_notice_rx = app
        .jobs
        .as_ref()
        .map(lofi_core::JobRegistry::subscribe_notices);

    let (picker_load_tx, mut picker_load_rx) = tokio::sync::mpsc::unbounded_channel();
    app.picker_load_tx = Some(picker_load_tx);
    let (lifecycle_tx, mut lifecycle_rx) = tokio::sync::mpsc::unbounded_channel();
    app.lifecycle_tx = Some(lifecycle_tx);
    let mut current_run: Option<RunHandle> = None;
    let mut events = tty_events::TtyEvents::start().map_err(Error::Io)?;
    app.sync_color_scheme_reports();
    if app.theme_mode == lofi_types::ThemeMode::Auto {
        tty_events::request_color_scheme();
    }
    let mut sigwinch =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
            .map_err(Error::Io)?;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(Error::Io)?;
    let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(Error::Io)?;
    let mut last_err: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut dirty = true;
    let mut resize = Box::<ResizeState>::default();
    loop {
        if dirty && resize.deadline.is_none() {
            guard.draw(&mut app, resize.events, resize.started, resize.last)?;
            if resize.deadline.is_none() {
                resize.started = None;
                resize.last = None;
                resize.events = 0;
            }
            if let Some(event) = app.debug_after_draw.take() {
                app.debug_sample(event);
            }
            dirty = if let Some(alert) = app.take_transcript_alert() {
                app.notify(NotifyKind::Error, alert);
                true
            } else {
                false
            };
        }

        tokio::select! {
            ev = async {
                match &mut current_run {
                    Some(r) => r.rx.recv().await,
                    None => std::future::pending::<Option<AgentEvent>>().await,
                }
            } => {
                match ev {
                    Some(e) => {
                        if let Some(run) = current_run.as_ref() {
                            attach_run_cursor(&mut app, run);
                        }
                        let mut event = Some(e);
                        for index in 0..64 {
                            let Some(e) = event.take() else { break };
                            if let AgentEvent::UserShell {
                                command, output, exit_code, signal, duration_ms,
                                truncated, cancelled, exclude_from_context,
                            } = e
                            {
                                let result = lofi_core::UserShellResult::from_session(
                                    command, output, exit_code, signal, duration_ms,
                                    truncated, cancelled,
                                );
                                finish_user_shell(&mut app, result, exclude_from_context);
                            } else {
                                app.apply_event(e);
                            }
                            if index < 63 {
                                event = current_run
                                    .as_mut()
                                    .and_then(|run| run.rx.try_recv().ok());
                            }
                        }
                        // If there's a queued prompt, signal the agent to
                        // exit its round loop after the current round so the
                        // queued prompt is sent at the earliest opportunity.
                        // The flag is only checked between rounds (after tool
                        // results are in hand), so setting it during streaming
                        // or tool execution is safe.
                        if !app.prompt_queue.is_empty() {
                            if let Some(r) = &current_run {
                                r.preempt.store(true, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                    }
                    None => {
                        if let Some(r) = current_run.take() {
                            attach_run_cursor(&mut app, &r);
                            let was_user_shell = r.user_shell.is_some();
                            if let Err(error) = r.handle.await {
                                app.apply_event(AgentEvent::Error(format!(
                                    "run task failed: {error}"
                                )));
                            }
                            app.run_finished();
                            app.debug_sample(if was_user_shell { "user_shell_settled" } else { "agent_settled" });
                            if !app.should_quit {
                                if was_user_shell {
                                    if let Some(queued) = app.prompt_queue.first().cloned() {
                                        app.prompt_queue.remove(0);
                                        spawn_prompt(
                                            &mut app,
                                            agent.as_ref(),
                                            &mut current_run,
                                            queued.text,
                                            queued.kind,
                                        );
                                    }
                                } else if app.context_pressure {
                                    // Core owns hard-cap eligibility and
                                    // compaction; the UI only presents its outcome.
                                    app.context_pressure = false;
                                    app.continue_after_hard_compact = true;
                                    if let Some(outcome) = app.hard_compact() {
                                        app.continue_after_hard_compact = false;
                                        match outcome {
                                            HardCompactOutcome::Compacted(_) => {
                                                spawn_continue(
                                                    &mut app,
                                                    agent.as_ref(),
                                                    &mut current_run,
                                                );
                                            }
                                            HardCompactOutcome::Cooldown => app.notify(
                                                NotifyKind::Warn,
                                                "context exceeded the hard cap too soon after a compaction; cannot continue",
                                            ),
                                            HardCompactOutcome::NotEnoughHistory => app.notify(
                                                NotifyKind::Warn,
                                                "could not compact at the hard cap; cannot continue",
                                            ),
                                        }
                                    }
                                } else {
                                    app.maybe_auto_compact();
                                    if !app.lifecycle_busy && app.run.is_none() {
                                        if let Some(queued) = app.prompt_queue.first().cloned() {
                                            app.prompt_queue.remove(0);
                                            spawn_prompt(
                                                &mut app,
                                                agent.as_ref(),
                                                &mut current_run,
                                                queued.text,
                                                queued.kind,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                dirty = true;
            }
            _ = sigterm.recv() => {
                request_quit(&mut app, &mut current_run);
            }
            _ = sighup.recv() => {
                request_quit(&mut app, &mut current_run);
            }
            _ = sigwinch.recv() => {
                if let Ok((w, h)) = crossterm::terminal::size() {
                    handle_event(
                        &Event::Resize(w, h),
                        &mut app,
                        agent.as_ref(),
                        &mut current_run,
                    );
                    let now = Instant::now();
                    if resize.deadline.is_none() {
                        resize.started = Some(now);
                        resize.events = 0;
                    }
                    resize.events = resize.events.saturating_add(1);
                    resize.last = Some(now);
                    resize.deadline = Some(
                        tokio::time::Instant::now()
                            + Duration::from_millis(RESIZE_DEBOUNCE_MS),
                    );
                }
            }
            maybe_ev = events.recv() => {
                let defer_redraw;
                match maybe_ev {
                    Some(Ok(tty_events::TuiEvent::ColorScheme(scheme))) => {
                        defer_redraw = false;
                        if app.apply_color_scheme(scheme) {
                            dirty = true;
                        }
                    }
                    Some(Ok(tty_events::TuiEvent::Input(ev))) => {
                        defer_redraw = matches!(ev, Event::Resize(_, _));
                        if !app.should_quit {
                            handle_event(&ev, &mut app, agent.as_ref(), &mut current_run);
                        }
                    if let Some(q) = app.pending_model_switch.take() {
                        match switcher.as_ref().map_or(
                            Err(lofi_core::Error::Config("no model registry".into())),
                            |s| s.rebuild(agent.as_ref(), &q),
                        ) {
                            Ok((new_agent, model, level)) => {
                                let tier = model.service_tier.clone();
                                agent = Some(new_agent);
                                app.apply_model_switch(&model, level, tier);
                            }
                            Err(e) => app.notify(
                                NotifyKind::Error,
                                format!("switch model: {e}"),
                            ),
                        }
                    }
                    if app.jobs_receiver_stale {
                        job_notice_rx = app
                            .jobs
                            .as_ref()
                            .map(lofi_core::JobRegistry::subscribe_notices);
                        app.jobs_receiver_stale = false;
                    }
                    // Slash commands (notably /tree reconcile) can push
                    // notices into prompt_queue while at rest. The queue's
                    // usual consumer is the agent-finished branch of the
                    // current_run select arm; without a live run, that arm
                    // never fires and the notice would sit forever. Drain
                    // at rest here, matching the mpsc notice path.
                    if !app.should_quit
                        && !app.busy()
                        && !app.prompt_queue.is_empty()
                        && agent.is_some()
                    {
                        if let Some(queued) = app.prompt_queue.first().cloned() {
                            app.prompt_queue.remove(0);
                            spawn_prompt(
                                &mut app,
                                agent.as_ref(),
                                &mut current_run,
                                queued.text,
                                queued.kind,
                            );
                        }
                    }
                    }
                    Some(Err(e)) => {
                        last_err = Some(format!("input read failed: {e}"));
                        break;
                    }
                    None => {
                        last_err = Some("input stream ended".to_string());
                        break;
                    }
                }
                if defer_redraw {
                    // Reflowing a large resumed transcript can take hundreds of
                    // milliseconds. Debounce the whole resize burst so one
                    // expensive leading draw cannot block event consumption and
                    // split a single drag into repeated one-event batches.
                    let now = Instant::now();
                    if resize.deadline.is_none() {
                        resize.started = Some(now);
                        resize.events = 0;
                    }
                    resize.events = resize.events.saturating_add(1);
                    resize.last = Some(now);
                    resize.deadline = Some(
                        tokio::time::Instant::now()
                            + Duration::from_millis(RESIZE_DEBOUNCE_MS),
                    );
                } else {
                    // Explicit user input should never wait behind resize UI
                    // policy. Treat it as the end of the current resize burst.
                    resize.deadline = None;
                    dirty = true;
                }
            }
            () = async {
                match resize.deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                resize.deadline = None;
                dirty = true;
            }
            _ = tick.tick() => {
                if app.refresh_confirmations() || !app.pending_confirms.is_empty() {
                    dirty = true;
                }
                if app.run.is_some() {
                    if let Some(s) = app.run.as_mut() {
                        *s = s.wrapping_add(1);
                    }
                    dirty = true;
                }
                if app.retry.is_some() {
                    dirty = true;
                }
                if app
                    .jobs_modal
                    .as_ref()
                    .is_some_and(|modal| modal.viewing.is_some())
                {
                    dirty = true;
                }
                if let Some(t) = app.yank_notify {
                    if t.elapsed() >= YANK_NOTIFY {
                        app.yank_notify = None;
                    }
                    dirty = true;
                }
                if let Some(t) = app.ctrl_c_at {
                    if t.elapsed() >= QUIT_DOUBLE_PRESS {
                        app.ctrl_c_at = None;
                    }
                    dirty = true;
                }
                if let Some(n) = app.notify.as_ref() {
                    if n.at.elapsed() >= NOTIFY_TTL {
                        app.notify = None;
                    }
                    dirty = true;
                }
                // Drain pending height re-measure a little each tick so a
                // width change or estimate-seeded resume never stalls a
                // frame. Absolute line indices shift while the estimates
                // converge, so a Navigate/Select cursor must follow its
                // content through each step instead of drifting (the cursor
                // visibly jumps around while the user scrolls otherwise).
                let navigating = matches!(app.mode, Mode::Navigate | Mode::Select);
                if navigating && app.height_remeasure_from.is_some() {
                    let nav_anchor = app.nav_content_anchor();
                    let sel_anchor = match app.mode {
                        Mode::Select => app.sel_content_anchor(),
                        _ => None,
                    };
                    app.remeasure_heights_step(16);
                    if let Some(anchor) = nav_anchor {
                        // The converging geometry is frozen-turn-only at
                        // rest, so an empty live last turn renders correctly.
                        app.reseat_nav_cursor(anchor, &[], app.frozen_width);
                    }
                    if let Some(anchor) = sel_anchor {
                        app.reseat_sel_anchor(anchor, &[], app.frozen_width);
                    }
                    app.nav_show_cursor();
                    dirty = true;
                } else if app.remeasure_heights_step(16) {
                    dirty = true;
                }
            }
            notice = async {
                match job_notice_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(text) = notice {
                    let queued = QueuedPrompt {
                        text,
                        kind: lofi_types::PromptKind::Notice,
                    };
                    if app.busy() {
                        app.prompt_queue.push(queued);
                        if let Some(r) = &current_run {
                            r.preempt.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    } else if agent.is_some() {
                        spawn_prompt(
                            &mut app,
                            agent.as_ref(),
                            &mut current_run,
                            queued.text,
                            queued.kind,
                        );
                    }
                    dirty = true;
                }
            }
            picker_load = picker_load_rx.recv() => {
                if let Some(load) = picker_load {
                    app.apply_picker_load(load);
                    dirty = true;
                }
            }
            lifecycle_result = lifecycle_rx.recv() => {
                if let Some(result) = lifecycle_result {
                    let hard_compacted = app.apply_lifecycle_result(result);
                    let continue_run =
                        std::mem::take(&mut app.continue_after_hard_compact) && hard_compacted;
                    if continue_run {
                        spawn_continue(&mut app, agent.as_ref(), &mut current_run);
                    } else if current_run.is_none() {
                        if let Some(queued) = app.prompt_queue.first().cloned() {
                            app.prompt_queue.remove(0);
                            spawn_prompt(
                                &mut app,
                                agent.as_ref(),
                                &mut current_run,
                                queued.text,
                                queued.kind,
                            );
                        }
                    }
                    dirty = true;
                }
            }
            req = confirm_rx.recv() => {
                if let Some(req) = req {
                    if req.active.load(std::sync::atomic::Ordering::Relaxed) {
                        app.queue_confirmation(req);
                        dirty = true;
                    }
                }
            }
        }

        if app.should_quit {
            if let Some(r) = current_run.take() {
                if settle_run_for_quit(r, QUIT_FLUSH_TIMEOUT).await {
                    last_err =
                        Some("quit: run aborted before its final transcript write".to_string());
                }
            }
            // Queued prompts were never sent; quit must not silently drop
            // typed input.
            persist_unsent_prompts(&mut app);
            break;
        }
    }
    if let Some(jobs) = &app.jobs {
        jobs.shutdown();
    }
    if let Some(msg) = last_err {
        return Err(Error::Io(std::io::Error::other(msg)));
    }
    Ok(())
}
