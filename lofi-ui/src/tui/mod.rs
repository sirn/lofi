//! Sessions: the append-only JSONL transcript and its shared logical cursor
//! are durable state; an `Arc<Mutex<Vec<Message>>` holds the active agent
//! context. The engine checkpoints each round through the cursor, while
//! `--continue`/`--resume` rebuild only the indexed active lineage and the
//! `/resume` picker can switch files mid-run.
//! ## The `!Send` agent future
//! `Agent::run` is not `Send`: the code-mode sandbox holds an `rquickjs`
//! `AsyncContext` which is `!Send`/`!Sync`. The agent future cannot be
//! `tokio::spawn`'d on the multi-thread runtime, so the whole loop runs inside
//! a `tokio::task::LocalSet` on the current worker thread: the agent is driven
//! by `spawn_local`, and the TUI event loop runs alongside it on the same
//! thread. The history mutex is a `std::sync::Mutex` (never held across an
//! await — the task clones out, runs, and writes back in two brief locks) so
//! the sync handler can also read it.

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
pub mod view;

#[cfg(test)]
mod tests;

#[allow(clippy::wildcard_imports)]
use {input::*, replay::*, resume::*, text::*, tree::*};

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use lofi_core::session::store::{self, SessionEntry, SessionStore};
use lofi_types::{
    ContentBlock, Message, NativeToolRecord, Role, RunModel, SessionEvent, SessionEventKind,
    ThinkingLevel, Usage,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;
use serde::{Deserialize, Serialize};

use base64::Engine;

pub(crate) mod theme;
use std::time::Instant;
pub(crate) use theme::Theme;
use tokio::sync::mpsc::Receiver;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::MissedTickBehavior;

use crate::tui::view::HStack;
use lofi_core::{compact, compacted_history, Agent, AgentEvent, CompactOptions};
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
    choices: Vec<lofi_types::ModelChoice>,
}

impl ModelSwitcher {
    pub(crate) fn new(
        registry: lofi_core::ModelRegistry,
        config: lofi_types::Config,
        root: PathBuf,
    ) -> Self {
        let choices = registry.choices();
        Self {
            registry,
            config,
            root,
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
        )
    }
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK_MS: u64 = 60;
const RESIZE_DEBOUNCE_MS: u64 = 50;
const AUTO_MODE_UI_GRACE: Duration = Duration::from_secs(3);
const YANK_NOTIFY: Duration = Duration::from_secs(2);
const NOTIFY_TTL: Duration = Duration::from_secs(5);
const MAX_INPUT_LINES: usize = 8;
const QUIT_DOUBLE_PRESS: Duration = Duration::from_secs(2);
const DEFAULT_CTX_LIMIT: u64 = 200_000;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "clear the transcript log"),
    ("/compact", "fold older history into a summary"),
    ("/debug", "toggle resource diagnostics"),
    ("/exit", "exit lofi"),
    ("/help", "show keybindings and commands"),
    ("/new", "start a fresh session"),
    ("/quit", "exit lofi"),
    ("/resume", "pick a past session to resume"),
    ("/session", "show session info"),
    ("/tree", "roll back to a past turn"),
    ("/model", "switch the active model"),
    ("/thinking", "switch the thinking level"),
    ("/verbose", "toggle tool detail"),
];

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolCall {
    id: String,
    name: String,
    input: String,
    label: Option<String>,
    native: Vec<NativeTool>,
    result: Option<String>,
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
    UserBash {
        command: String,
        output: String,
        exit_code: Option<i32>,
        signal: Option<i32>,
        duration: Duration,
        truncated: bool,
        cancelled: bool,
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
    Compaction {
        summarized: usize,
        kept: usize,
        summary: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Turn {
    prompt: String,
    blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
struct SessionState {
    store: Option<SessionStore>,
    cursor: Option<store::SessionCursor>,
    cwd: PathBuf,
}

impl SessionState {
    fn path(&self) -> Option<&Path> {
        self.cursor.as_ref().map(store::SessionCursor::path)
    }

    fn cursor_or_create(&mut self, model: &RunModel) -> Option<store::SessionCursor> {
        if self.cursor.is_none() {
            self.cursor = self
                .store
                .as_ref()
                .and_then(|store| store.create_cursor(&self.cwd, model).ok());
        }
        self.cursor.clone()
    }
}

pub(crate) struct SessionConfig {
    store: Option<SessionStore>,
    cursor: Option<store::SessionCursor>,
    index: Vec<store::EventIndex>,
    file_size: u64,
    cwd: PathBuf,
}

impl SessionConfig {
    pub(crate) fn ephemeral(cwd: PathBuf) -> Self {
        Self {
            store: None,
            cursor: None,
            index: Vec::new(),
            file_size: 0,
            cwd,
        }
    }

    pub(crate) fn fresh(store: SessionStore, cwd: PathBuf) -> Self {
        Self {
            store: Some(store),
            cursor: None,
            index: Vec::new(),
            file_size: 0,
            cwd,
        }
    }

    pub(crate) fn resumed(
        store: SessionStore,
        cursor: store::SessionCursor,
        index: Vec<store::EventIndex>,
        file_size: u64,
        cwd: PathBuf,
    ) -> Self {
        Self {
            store: Some(store),
            cursor: Some(cursor),
            index,
            file_size,
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
    lines: Vec<Line<'static>>,
    scroll: usize,
    total: usize,
    view_h: usize,
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

fn info_section(t: Theme, label: &str) -> Line<'static> {
    Line::from(Span::styled(
        label.to_string(),
        Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
    ))
}

/// `key  value` row: key bold in `fg`, value muted, key padded to a fixed
/// column so the values line up. Longer keys just overflow the column.
fn info_kv(t: Theme, key: &str, value: &str) -> Line<'static> {
    const COL: usize = 12;
    let pad = COL.saturating_sub(key.chars().count());
    Line::from(vec![
        Span::styled(
            format!("{}{}", key, " ".repeat(pad)),
            Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled(value.to_string(), Style::new().fg(t.muted)),
    ])
}

fn info_note(t: Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::new().fg(t.muted)))
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

#[derive(Debug, Clone)]
struct TreeEntry {
    branch_point: String,
    label: String,
    prefix: String,
    prefill: String,
    is_active: bool,
    source_index: usize,
    source_offset: u64,
    source_kind: store::IndexKind,
    hydrated: bool,
}

/// Retains compressed, width-independent display models so resizing a resumed
/// transcript does not rescan its JSONL file. Successful native results are
/// reduced to their collapsed previews before insertion; per-turn compression
/// avoids retaining the full Rust object graph. A hard budget preserves bounded
/// memory for transcripts whose visible content itself is unusually large.
struct CollapsedTurnCache {
    map: HashMap<usize, Box<[u8]>>,
    retained_bytes: usize,
    frame_hits: usize,
    frame_misses: usize,
    frame_materialize_us: u128,
}

impl CollapsedTurnCache {
    const MAX_RETAINED_BYTES: usize = 64 * 1024 * 1024;

    fn new() -> Self {
        Self {
            map: HashMap::new(),
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
                .map(Arc::new)
        });
        if turn.is_some() {
            self.frame_hits += 1;
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
        if bytes > Self::MAX_RETAINED_BYTES.saturating_sub(self.retained_bytes) {
            return;
        }
        self.retained_bytes += bytes;
        self.map.insert(idx, data);
    }

    fn clear(&mut self) {
        self.map.clear();
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
        index: Arc<Vec<store::EventIndex>>,
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
    history: Arc<Mutex<Vec<Message>>>,
    history_nav: Vec<String>,
    history_idx: Option<usize>,
    input_stash: String,
    model_label: String,
    thinking_label: Option<String>,
    thinking: ThinkingLevel,
    status_usage: Option<Usage>,
    total_in: u64,
    total_out: u64,
    prompt_queue: Vec<String>,
    cost: f64,
    turn_cost: f64,
    turn_has_round_usage: bool,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    /// Last observed context input-token count, for the auto-compaction
    /// hysteresis: the trigger fires only on the upward crossing of the
    /// threshold, not on every above-threshold turn. `None` until the
    /// first round reports usage, and reset to `None` after a compaction
    /// or a session rollback so the baseline re-evaluates cleanly.
    prev_ctx_tokens: Option<u64>,
    settled_usage_fresh: bool,
    compacted: bool,
    /// Set by `ContextPressure` when the engine force-stopped the run at the
    /// hard context cap. The run loop reads (and clears) it on channel close
    /// to drive the force-compact + silent continue, instead of the soft
    /// `agent_settled` path.
    context_pressure: bool,
    run: Option<usize>,
    run_start: Option<Instant>,
    retry: Option<RetryState>,
    pinned: bool,
    top_line: usize,
    last_base: usize,
    verbose: bool,
    debug_after_draw: Option<&'static str>,
    debug: Option<debug_stats::DebugState>,
    should_quit: bool,
    session: SessionState,
    picker: Option<PickerState>,
    tree_picker: Option<TreePickerState>,
    tree_picker_index: Option<Arc<Vec<store::EventIndex>>>,
    tree_picker_pending: std::collections::HashSet<usize>,
    picker_load_tx: Option<tokio::sync::mpsc::UnboundedSender<PickerLoad>>,
    picker_generation: Arc<AtomicU64>,
    model_picker: Option<ModelPickerState>,
    thinking_picker: Option<ThinkingPickerState>,
    model_choices: Vec<lofi_types::ModelChoice>,
    pending_model_switch: Option<String>,
    info: Option<InfoModal>,
    slash_complete: Option<SlashComplete>,
    no_models_hint: Option<String>,
    theme: Theme,
    kill_ring: String,
    /// True when the previous command was `C-k` so a consecutive `C-k`
    /// appends to the kill ring instead of replacing it.
    last_kill_was_kill: bool,
    ctrl_c_at: Option<Instant>,
    log_rect: Rect,
    input_rect: Rect,
    log_vis: Vec<view::VisLine>,
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
    /// Line count per frozen turn (all of them), so the viewport can be
    /// located and `total` computed without fetching rendered lines. Synced
    /// to the file-backed prefix (which may be all turns) for the active mode.
    frozen_heights: Vec<usize>,
    frozen_heights_other_mode: Vec<usize>,
    render_epoch: u64,
    frozen_epoch: u64,
    /// Viewport width the frozen cache was last built at. A resize changes
    /// the wrap width, so a mismatch discards the cache just like an epoch
    /// bump — otherwise background-padded lines keep the old (narrower)
    /// width after the terminal grows.
    frozen_width: usize,
    render_profile: Box<RenderProfile>,
}

impl App {
    fn restore_indexed_session(
        &mut self,
        cursor: &store::SessionCursor,
        index: &[store::EventIndex],
        file_size: u64,
    ) -> Result<()> {
        let messages = history_from_index(cursor, index, &self.compaction.edit)?;
        if let Ok(mut history) = self.history.lock() {
            *history = messages;
        }
        self.turns.clear();
        self.collapsed_turns.get_mut().clear();
        self.turn_byte_ranges.clear();
        self.turn_event_offsets.clear();
        self.cost = 0.0;
        self.total_in = 0;
        self.total_out = 0;
        self.reset_compaction_gauges();
        replay_indexed_session(self, cursor, index, file_size)?;
        restore_compaction_from_index(self, cursor, index);
        Ok(())
    }
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
    user_bash: Option<(String, bool)>,
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
            DisableMouseCapture,
            LeaveAlternateScreen
        );
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
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    switcher: Option<ModelSwitcher>,
) -> Result<()> {
    enable_raw_mode().map_err(Error::Io)?;
    let setup = (|| -> std::io::Result<_> {
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
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
                DisableMouseCapture,
                LeaveAlternateScreen
            );
            let _ = disable_raw_mode();
            return Err(Error::Io(e));
        }
    };

    let mut guard = TerminalGuard { terminal };
    let local = LocalSet::new();
    let result = local
        .run_until(async move {
            run_loop(
                &mut guard,
                agent,
                model_label,
                thinking,
                session,
                no_models_hint,
                ctx_limit,
                compaction,
                switcher,
            )
            .await
        })
        .await;
    result
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_loop(
    guard: &mut TerminalGuard,
    mut agent: Option<Agent>,
    model_label: String,
    thinking: ThinkingLevel,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
    compaction: lofi_types::CompactionConfig,
    switcher: Option<ModelSwitcher>,
) -> Result<()> {
    let SessionConfig {
        store,
        cursor,
        index,
        file_size,
        cwd,
    } = session;
    let model_choices = switcher
        .as_ref()
        .map_or(Vec::new(), |s| s.choices().to_vec());
    let mut app = App::new(model_label, thinking, ctx_limit, compaction);
    app.model_choices = model_choices;
    app.session = SessionState { store, cursor, cwd };
    if let Some(cursor) = app.session.cursor.clone() {
        app.restore_indexed_session(&cursor, &index, file_size)?;
    }
    // The resume index is startup scratch. All persistent UI backing uses the
    // compact turn ranges/offsets built above and opens a fresh cursor snapshot
    // only for branch operations. Explicitly drop it before the event loop so
    // an async state-machine frame cannot retain thousands of ID strings.
    drop(index);
    // Freeze every turn except the last: its blocks are backed by the
    // transcript file (see `materialize_turn`), so drop them to keep memory
    // bounded by the viewport rather than the whole session. The last turn
    // keeps its blocks so it renders without a file read each frame.
    if app.turns.len() > 1 {
        let n = app.turns.len();
        for turn in &mut app.turns[..n - 1] {
            turn.blocks.clear();
        }
    }
    app.bump_render_epoch();
    app.no_models_hint = no_models_hint.clone();
    if let Some(hint) = no_models_hint {
        // Lead with the hint so the user sees why nothing will run.
        app.insert_turn(
            0,
            Turn {
                prompt: String::new(),
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
        agent = Some(a.with_confirm_tx(confirm_tx));
    }

    let (picker_load_tx, mut picker_load_rx) = tokio::sync::mpsc::unbounded_channel();
    app.picker_load_tx = Some(picker_load_tx);
    let mut current_run: Option<RunHandle> = None;
    let mut events = EventStream::new();
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
            dirty = false;
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
                        if let AgentEvent::UserBash {
                            command, output, exit_code, signal, duration_ms,
                            truncated, cancelled, exclude_from_context,
                        } = e
                        {
                            let result = lofi_core::UserBashResult::from_session(
                                command, output, exit_code, signal, duration_ms,
                                truncated, cancelled,
                            );
                            finish_user_bash(&mut app, result, exclude_from_context);
                        } else {
                            app.apply_event(e);
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
                            let was_user_bash = r.user_bash.is_some();
                            r.handle.abort();
                            app.run_finished();
                            app.debug_sample(if was_user_bash { "user_bash_settled" } else { "agent_settled" });
                            if was_user_bash {
                                if let Some(prompt) = app.prompt_queue.first().cloned() {
                                    app.prompt_queue.remove(0);
                                    spawn_prompt(&mut app, agent.as_ref(), &mut current_run, prompt);
                                }
                            } else if app.context_pressure {
                                // Hard cap: force-compact + silent
                                // continue, gated by the cooldown so a run
                                // that re-crosses the hard cap too soon
                                // after a compact errors out instead of
                                // looping.
                                app.context_pressure = false;
                                let cooled = app.messages_since_last_compact()
                                    >= app.compaction.min_messages_between_hard_compacts;
                                if !cooled {
                                    app.notify(
                                        NotifyKind::Warn,
                                        "context exceeded the hard cap too soon after a compaction; cannot continue".to_string(),
                                    );
                                } else if !app.compact_now() {
                                    app.notify(
                                        NotifyKind::Warn,
                                        "could not compact at the hard cap; cannot continue".to_string(),
                                    );
                                } else {
                                    spawn_continue(&mut app, agent.as_ref(), &mut current_run);
                                }
                            } else {
                                app.maybe_auto_compact();
                                if app.run.is_none() {
                                    if let Some(prompt) = app.prompt_queue.first().cloned() {
                                        app.prompt_queue.remove(0);
                                        spawn_prompt(
                                            &mut app,
                                            agent.as_ref(),
                                            &mut current_run,
                                            prompt,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                dirty = true;
            }
            maybe_ev = events.next() => {
                let defer_redraw;
                match maybe_ev {
                    Some(Ok(ev)) => {
                        defer_redraw = matches!(ev, Event::Resize(_, _));
                        handle_event(&ev, &mut app, agent.as_ref(), &mut current_run);
                    if let Some(q) = app.pending_model_switch.take() {
                        match switcher.as_ref().map_or(
                            Err(lofi_core::Error::Config("no model registry".into())),
                            |s| s.rebuild(agent.as_ref(), &q),
                        ) {
                            Ok((new_agent, model, level)) => {
                                agent = Some(new_agent);
                                app.apply_model_switch(&model, level);
                            }
                            Err(e) => app.notify(
                                NotifyKind::Error,
                                format!("switch model: {e}"),
                            ),
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
            }
            picker_load = picker_load_rx.recv() => {
                if let Some(load) = picker_load {
                    app.apply_picker_load(load);
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
            break;
        }
    }

    if let Some(r) = current_run.take() {
        r.cancel.store(true, Ordering::Relaxed);
        r.handle.abort();
    }
    if let Some(msg) = last_err {
        return Err(Error::Io(std::io::Error::other(msg)));
    }
    Ok(())
}
