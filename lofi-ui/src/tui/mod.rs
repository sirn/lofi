//! Terminal UI: ratatui + crossterm event loop.
//!
//! Renders the conversation as a sequence of turns, each a user prompt
//! followed by a stream of blocks (assistant text, reasoning, tool calls,
//! errors) — the Pi/Crush-style block model. Tool-result bodies are folded to
//! a 3-line preview by default and expanded in full by the `/verbose` toggle;
//! the status strip shows the model, resolved thinking level, a context-usage
//! gauge, and a spinner.
//!
//! Sessions: the transcript (`Vec<Message>`) is the source of truth, shared
//! with the agent task via an `Arc<Mutex>`. After each turn the new messages
//! are appended to a JSONL file (`lofi-core::session::store`);
//! `--continue`/`--resume` load one up front and the `/resume` picker switches
//! mid-run.
//!
//! ## The `!Send` agent future
//!
//! `Agent::run` is not `Send`: the code-mode sandbox holds an `rquickjs`
//! `AsyncContext` which is `!Send`/`!Sync`. The agent future cannot be
//! `tokio::spawn`'d on the multi-thread runtime, so the whole loop runs inside
//! a `tokio::task::LocalSet` on the current worker thread: the agent is driven
//! by `spawn_local`, and the TUI event loop runs alongside it on the same
//! thread. The history mutex is a `std::sync::Mutex` (never held across an
//! await — the task clones out, runs, and writes back in two brief locks) so
//! the sync handler can also read it.

pub mod view;

use std::io::{self, Stdout, Write};
use std::path::PathBuf;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::event::{EnableMouseCapture, DisableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use lofi_core::session::store::{self, SessionEntry, SessionStore};
use lofi_types::{ContentBlock, Message, Role, SessionEvent, ThinkingLevel, Usage};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Terminal;

use base64::Engine;

pub(crate) mod theme;
pub(crate) use theme::Theme;
use std::time::Instant;
use tokio::sync::mpsc::Receiver;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::MissedTickBehavior;

use lofi_core::{Agent, AgentEvent, SessionCommit};
use lofi_error::{Error, Result};
use crate::tui::view::HStack;

/// Braille spinner frames, advanced on each tick while a run is active.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK_MS: u64 = 60;
/// Maximum height (content lines) the input box grows to before clipping.
const MAX_INPUT_LINES: usize = 8;
/// Soft context-window ceiling for the status gauge, in tokens. Per-model
/// limits are a follow-up; this is a sane default for current models.
const DEFAULT_CTX_LIMIT: u64 = 200_000;

/// A native tool call (`lofi.bash`/`lofi.read`/…) observed inside an `exec`
/// block, surfaced so the UI can render each one under its parent exec.
#[derive(Debug, Clone)]
struct NativeTool {
    id: u64,
    name: String,
    args: String,
    result: Option<String>,
    is_error: bool,
    done: bool,
}

/// A single tool call accumulated across ToolStart/ToolInput/ToolEnd.
///
/// For the `exec` tool, `input` holds the TypeScript `code`, `label` is the
/// optional `display` name shown as `Exec <label>`, and `native` is the
/// stream of native tool calls that ran inside it.
#[derive(Debug, Clone)]
struct ToolCall {
    id: String,
    name: String,
    input: String,
    label: Option<String>,
    native: Vec<NativeTool>,
    result: Option<String>,
    is_error: bool,
    done: bool,
    /// Set when the call completes (engine-stamped duration).
    elapsed: Option<Duration>,
}

/// A reasoning block. `elapsed` is `None` while the model is still
/// thinking and `Some(d)` once it moves on (or the run ends), which is what
/// flips the "Thinking..." trailing line into "Thought for Ns".
#[derive(Debug, Clone)]
struct ThinkingBlock {
    text: String,
    start: Instant,
    elapsed: Option<Duration>,
}

/// One block in a turn's response stream.
#[derive(Debug, Clone)]
enum Block {
    Text(String),
    Thinking(ThinkingBlock),
    Tool(ToolCall),
    Error(String),
    /// Crush-style turn-end rule: `<label> done in Ns` followed by a dash
    /// fill, appended when a run finishes.
    TurnEnd { label: String, elapsed: Duration },
}

/// A user prompt and the blocks produced in response.
#[derive(Debug, Clone)]
pub(crate) struct Turn {
    prompt: String,
    blocks: Vec<Block>,
}

/// Session persistence state held by the App.
#[derive(Debug, Clone)]
struct SessionState {
    store: Option<SessionStore>,
    /// Path to the open transcript file; None until the first prompt creates
    /// one (fresh sessions) or after '/new'.
    path: Option<PathBuf>,
    cwd: PathBuf,
}

/// Resolved session handed to [`run`] by the binary.
pub(crate) struct SessionConfig {
    store: Option<SessionStore>,
    path: Option<PathBuf>,
    /// Full event log loaded on resume; empty for fresh/ephemeral sessions.
    events: Vec<SessionEvent>,
    /// Byte offset of each event's line in the transcript file (parallel to
    /// `events`), and the file's total size — used to build per-turn byte
    /// ranges so frozen turns can be re-materialized from the file on demand.
    offsets: Vec<u64>,
    file_size: u64,
    cwd: PathBuf,
}

impl SessionConfig {
    /// No persistence (--no-session).
    pub(crate) fn ephemeral(cwd: PathBuf) -> Self {
        Self {
            store: None,
            path: None,
            events: Vec::new(),
            offsets: Vec::new(),
            file_size: 0,
            cwd,
        }
    }

    /// A new session, created lazily on the first prompt.
    pub(crate) fn fresh(store: SessionStore, cwd: PathBuf) -> Self {
        Self {
            store: Some(store),
            path: None,
            events: Vec::new(),
            offsets: Vec::new(),
            file_size: 0,
            cwd,
        }
    }

    /// Resume an existing transcript file.
    pub(crate) fn resumed(
        store: SessionStore,
        path: PathBuf,
        events: Vec<SessionEvent>,
        offsets: Vec<u64>,
        file_size: u64,
        cwd: PathBuf,
    ) -> Self {
        Self {
            store: Some(store),
            path: Some(path),
            events,
            offsets,
            file_size,
            cwd,
        }
    }
}

/// State for the '/resume' session-picker overlay.
#[derive(Debug, Clone)]
struct PickerState {
    entries: Vec<SessionEntry>,
    selected: usize,
}

/// A bounded FIFO cache of rendered frozen turns, keyed by turn index.
/// Only turns near the viewport are retained; the rest are re-rendered on
/// demand from `turns`. Heights for *all* frozen turns live in
/// `frozen_heights` (tiny) so the viewport can be located without fetching
/// lines, keeping the heavy styled-line copy bounded by [`FROZEN_CACHE_CAP`]
/// turns regardless of session length.
struct FrozenCache {
    order: VecDeque<usize>,
    map: HashMap<usize, (Vec<Line<'static>>, Vec<String>)>,
}

impl FrozenCache {
    /// Maximum number of frozen turns whose rendered lines are kept in memory.
    const CAP: usize = 64;

    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            map: HashMap::new(),
        }
    }

    fn contains(&self, idx: usize) -> bool {
        self.map.contains_key(&idx)
    }

    fn get(&self, idx: usize) -> Option<&(Vec<Line<'static>>, Vec<String>)> {
        self.map.get(&idx)
    }

    fn clear(&mut self) {
        self.order.clear();
        self.map.clear();
    }

    fn insert(&mut self, idx: usize, entry: (Vec<Line<'static>>, Vec<String>)) {
        if self.map.insert(idx, entry).is_none() {
            self.order.push_back(idx);
        }
        while self.order.len() > Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// The TUI's mutable state.
/// Mouse selection in the log, in visual-line + char-index space (absolute
/// indices into `log_lines`).
struct Selection {
    start: (usize, usize),
    end: (usize, usize),
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
    input: String,
    input_cursor: usize,
    history: Arc<Mutex<Vec<Message>>>,
    history_nav: Vec<String>,
    /// Index into `history_nav` while recalling; `None` while editing live input.
    history_idx: Option<usize>,
    /// Live input stashed while navigating history; restored on recall exit.
    input_stash: String,
    model_label: String,
    /// " · medium"-style suffix, or None when thinking is off.
    thinking_label: Option<String>,
    status_usage: Option<Usage>,
    /// Accumulated billed input/output tokens across turns (for the footer).
    total_in: u64,
    total_out: u64,
    /// Accumulated USD cost across turns (engine-computed, fed by `TurnEnd`).
    cost: f64,
    ctx_limit: u64,
    /// Spinner frame while a run is active; None when idle.
    run: Option<usize>,
    /// Wall-clock start of the active run; drives the live `working for Ns`
    /// indicator only — the authoritative turn duration comes from the
    /// engine's `TurnEnd` event.
    run_start: Option<Instant>,
    /// When true the log follows the latest output (pinned to the bottom).
    pinned: bool,
    /// Absolute index of the first visible log line while `pinned` is false.
    top_line: usize,
    /// Bottom scroll offset from the last render; seeds `top_line` on un-pin.
    last_base: usize,
    verbose: bool,
    should_quit: bool,
    session: SessionState,
    picker: Option<PickerState>,
    /// Hint shown in the log when no model is configured; `None` in normal runs.
    no_models_hint: Option<String>,
    theme: Theme,
    /// Kill ring for emacs-style C-k / C-u / C-w / M-d, yanked back with C-y.
    kill_ring: String,
    /// True when the previous command was `C-k` so a consecutive `C-k`
    /// appends to the kill ring instead of replacing it.
    last_kill_was_kill: bool,
    /// Screen rect of the log viewport, stashed at render time for hit-testing
    /// mouse scroll / selection.
    log_rect: Rect,
    /// Plain text of each *visible* log line (the viewport window only),
    /// stashed at render time so mouse selection can map screen coords to
    /// text. Window-relative: index 0 is the top visible line.
    log_lines: Vec<String>,
    /// Absolute index of the top visible log line (`scroll` offset).
    log_off: usize,
    /// Active mouse selection, if any.
    sel: Option<Selection>,
    /// Rendered-line cache for frozen turns (all but the live last one),
    /// bounded to [`FrozenCache::CAP`] turns near the viewport. Turns outside
    /// the cache are re-rendered from `turns` on demand. The last turn is
    /// rebuilt fresh each frame; earlier turns are immutable once a new turn
    /// is pushed, so their height is stable and only their (heavy) styled
    /// lines are evictable.
    frozen_render: FrozenCache,
    /// Line count per frozen turn (all of them), so the viewport can be
    /// located and `total` computed without fetching rendered lines. Synced
    /// to `turns.len()-1`.
    frozen_heights: Vec<usize>,
    /// Bumped whenever `turns` is replaced wholesale (resume, `/new`,
    /// `/clear`); a mismatch with `frozen_epoch` discards the cache.
    render_epoch: u64,
    /// Epoch captured when `frozen_render` was last built.
    frozen_epoch: u64,
}

impl App {
    fn new(model_label: String, thinking: ThinkingLevel, ctx_limit: u64) -> Self {
        let thinking_label = (thinking != ThinkingLevel::Off)
            .then(|| format!(" · {}", thinking.as_str()));
        Self {
            turns: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            history: Arc::new(Mutex::new(Vec::new())),
            history_nav: Vec::new(),
            history_idx: None,
            input_stash: String::new(),
            model_label,
            thinking_label,
            status_usage: None,
            ctx_limit: ctx_limit.max(DEFAULT_CTX_LIMIT),
            cost: 0.0,
            total_in: 0,
            total_out: 0,
            run: None,
            run_start: None,
            pinned: true,
            top_line: 0,
            last_base: 0,
            verbose: false,
            should_quit: false,
            session: SessionState {
                store: None,
                path: None,
                cwd: PathBuf::new(),
            },
            picker: None,
            no_models_hint: None,
            theme: Theme::default(),
            kill_ring: String::new(),
            last_kill_was_kill: false,
            log_rect: Rect::default(),
            log_lines: Vec::new(),
            log_off: 0,
            sel: None,
            frozen_render: FrozenCache::new(),
            frozen_heights: Vec::new(),
            turn_byte_ranges: Vec::new(),
            render_epoch: 0,
            frozen_epoch: 0,
        }
    }

    /// Label written into the transcript header (model + resolved level).
    fn session_model(&self) -> String {
        format!(
            "{}{}",
            self.model_label,
            self.thinking_label.as_deref().unwrap_or("")
        )
    }

    /// Number of visual rows the input occupies after soft-wrapping to the
    /// prompt width, capped at [`MAX_INPUT_LINES`]. `width` is the full
    /// terminal width; 2 cells are reserved for the `❯ `/`  ` prefix.
    fn input_lines(&self, width: usize) -> usize {
        let content_w = width.saturating_sub(2);
        self.input_visual_rows(content_w)
            .len()
            .min(MAX_INPUT_LINES)
    }

    /// Soft-wrap the input to `content_w` display cells, breaking on
    /// wide-char boundaries (not word boundaries, so the cursor maps
    /// predictably). Hard `\n` splits always start a new row. Empty input
    /// yields a single empty row so the prompt always renders one line.
    fn input_visual_rows(&self, content_w: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for line in self.input.split('\n') {
            if content_w == 0 {
                rows.push(line.to_string());
                continue;
            }
            let mut cur = String::new();
            let mut cur_w = 0usize;
            for c in line.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if cur_w + cw > content_w && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(c);
                cur_w += cw;
            }
            rows.push(cur);
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        rows
    }

    /// Map the cursor to a (visual row, x-within-content) pair for
    /// [`set_cursor_position`], accounting for soft-wrap. `x` is relative to
    /// the content area; the caller adds the 2-cell prefix.
    fn input_cursor_pos(&self, content_w: usize) -> (usize, usize) {
        let (lrow, lcol) = self.cursor_row_col();
        let mut vrow = 0usize;
        for (i, line) in self.input.split('\n').enumerate() {
            if i == lrow {
                break;
            }
            vrow += count_wrapped_rows(line, content_w);
        }
        let line = self.input.split('\n').nth(lrow).unwrap_or("");
        let prefix: String = line.chars().take(lcol).collect();
        let (sub, x) = wrap_prefix_pos(&prefix, content_w);
        (vrow + sub, x)
    }

    /// Fold an [`AgentEvent`] into the current turn's blocks / status.
    #[allow(clippy::too_many_lines, clippy::cast_precision_loss)]
    fn apply_event(&mut self, ev: AgentEvent) {
        let Some(turn) = self.turns.last_mut() else {
            return;
        };
        match ev {
            AgentEvent::Text(delta) => {
                if let Some(Block::Text(t)) = turn.blocks.last_mut() {
                    t.push_str(&delta);
                } else {
                    finalize_open_thinking(turn);
                    turn.blocks.push(Block::Text(delta));
                }
            }
            AgentEvent::Thinking(delta) => {
                if let Some(Block::Thinking(t)) = turn.blocks.last_mut() {
                    if t.elapsed.is_none() {
                        t.text.push_str(&delta);
                        return;
                    }
                }
                turn.blocks.push(Block::Thinking(ThinkingBlock {
                    text: delta,
                    start: Instant::now(),
                    elapsed: None,
                }));
            }
            AgentEvent::ToolStart { id, name } => {
                finalize_open_thinking(turn);
                turn.blocks.push(Block::Tool(ToolCall {
                    id,
                    name,
                    input: String::new(),
                    label: None,
                    native: Vec::new(),
                    result: None,
                    is_error: false,
                    done: false,
                    elapsed: None,
                }));
            }
            AgentEvent::ToolInput { id, code, label } => {
                if let Some(t) = tool_mut(&mut turn.blocks, &id) {
                    // Streamed deltas already grew `input`; the finalized event
                    // replaces it with the authoritative full code and stamps
                    // the label. Falls back to `code` when nothing streamed.
                    t.input = code;
                    if t.label.is_none() {
                        t.label = label;
                    }
                }
            }
            AgentEvent::ToolInputDelta { id, delta } => {
                if let Some(t) = tool_mut(&mut turn.blocks, &id) {
                    t.input.push_str(&delta);
                }
            }
            AgentEvent::ToolEnd { id, result, is_error, elapsed_ms } => {
                if let Some(t) = tool_mut(&mut turn.blocks, &id) {
                    t.result = Some(result);
                    t.is_error = is_error;
                    t.done = true;
                    t.elapsed = Some(Duration::from_millis(elapsed_ms));
                }
            }
            AgentEvent::NativeToolStart {
                parent,
                id,
                name,
                args,
            } => {
                if let Some(t) = tool_mut(&mut turn.blocks, &parent) {
                    t.native.push(NativeTool {
                        id,
                        name,
                        args,
                        result: None,
                        is_error: false,
                        done: false,
                    });
                }
            }
            AgentEvent::NativeToolEnd {
                parent,
                id,
                result,
                is_error,
            } => {
                if let Some(t) = tool_mut(&mut turn.blocks, &parent) {
                    if let Some(nt) = t.native.iter_mut().find(|n| n.id == id) {
                        nt.result = Some(result);
                        nt.is_error = is_error;
                        nt.done = true;
                    }
                }
            }
            AgentEvent::TurnEnd { elapsed_ms, cost, usage } => {
                let label = self.run_label();
                if let Some(turn) = self.turns.last_mut() {
                    finalize_open_thinking(turn);
                    turn.blocks.push(Block::TurnEnd {
                        label,
                        elapsed: Duration::from_millis(elapsed_ms),
                    });
                }
                self.cost += cost;
                self.total_in += usage.input_tokens;
                self.total_out += usage.output_tokens;
                self.status_usage = Some(usage);
            }
            AgentEvent::Error(msg) => {
                finalize_open_thinking(turn);
                turn.blocks.push(Block::Error(msg));
            }
            AgentEvent::TurnCommitted { byte_start, byte_end } => {
                // The just-finished turn is now durably in the transcript
                // file over this byte range. Record it so the turn becomes
                // file-backed when the next prompt freezes it.
                if let Some(r) = self.turn_byte_ranges.last_mut() {
                    *r = Some((byte_start, byte_end));
                }
            }
        }
    }

    fn run_finished(&mut self) {
        if let Some(turn) = self.turns.last_mut() {
            finalize_open_thinking(turn);
        }
        // The turn-end marker (label, elapsed, cost, usage) arrives as an
        // `AgentEvent::TurnEnd` emitted by the engine, which also writes it
        // to the transcript — so there is nothing to stamp or persist here.
        self.run_start = None;
        self.run = None;
    }

    /// Recompute cost/usage totals from the transcript's `TurnEnd` events.
    /// Called on resume, since these are no longer accumulated live.
    fn accumulate_totals(&mut self, events: &[SessionEvent]) {
        self.cost = 0.0;
        self.total_in = 0;
        self.total_out = 0;
        self.status_usage = None;
        for ev in events {
            if let SessionEvent::TurnEnd { cost, usage, .. } = ev {
                self.cost += cost;
                self.total_in += usage.input_tokens;
                self.total_out += usage.output_tokens;
                self.status_usage = Some(*usage);
            }
        }
    }

    /// Invalidate the frozen-turn cache. Call whenever `turns` is replaced
    /// wholesale (resume, `/new`, `/clear`); incremental `push` does not need
    /// it — [`ensure_frozen`] freezes the newly-superseded turn on its own.
    fn bump_render_epoch(&mut self) {
        self.render_epoch = self.render_epoch.wrapping_add(1);
    }

    /// Push a turn, keeping `turn_byte_ranges` parallel to `turns`.
    fn push_turn(&mut self, turn: Turn) {
        self.turns.push(turn);
        self.turn_byte_ranges.push(None);
    }

    /// Insert a turn at `idx`, keeping `turn_byte_ranges` parallel.
    fn insert_turn(&mut self, idx: usize, turn: Turn) {
        self.turns.insert(idx, turn);
        self.turn_byte_ranges.insert(idx, None);
    }

    /// Reconstruct a frozen turn's blocks. If the turn still holds its blocks
    /// in memory (ephemeral session, or not yet frozen), clone them. Otherwise
    /// re-parse the turn's byte range from the transcript file. On any read or
    /// parse failure the turn's prompt is preserved with empty blocks.
    fn materialize_turn(&self, idx: usize) -> Turn {
        if let Some(turn) = self.turns.get(idx) {
            if !turn.blocks.is_empty() {
                return turn.clone();
            }
        }
        let prompt = self
            .turns
            .get(idx)
            .map(|t| t.prompt.clone())
            .unwrap_or_default();
        let empty = Turn {
            prompt,
            blocks: Vec::new(),
        };
        let Some((start, end)) = self.turn_byte_ranges.get(idx).copied().flatten() else {
            return empty;
        };
        let Some(path) = &self.session.path else {
            return empty;
        };
        let bytes = {
            use std::io::{Read, Seek, SeekFrom};
            let Ok(mut f) = std::fs::File::open(path) else {
                return empty;
            };
            if f.seek(SeekFrom::Start(start)).is_err() {
                return empty;
            }
            let mut buf =
                Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
            if f.take(end - start).read_to_end(&mut buf).is_err() {
                return empty;
            }
            buf
        };
        let events: Vec<SessionEvent> = String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(|l| lofi_core::session::store::parse_event(l).ok())
            .collect();
        turns_from_events(&events)
            .into_iter()
            .next()
            .unwrap_or(empty)
    }

    /// Ensure frozen turn `idx`'s rendered lines are in the bounded cache,
    /// materializing from `turns` or the transcript file on a miss.
    /// `ensure_frozen` must have already recorded the turn's height.
    fn ensure_frozen_turn(&mut self, idx: usize, width: usize) {
        if !self.frozen_render.contains(idx) {
            let theme = self.theme;
            let turn = self.materialize_turn(idx);
            let lines = {
                let cx = view::component::Cx {
                    app: self,
                    theme,
                    width,
                    active_turn: false,
                };
                view::blocks::render_turn_lines(&cx, &turn)
            };
            let plain: Vec<String> = lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            self.frozen_render.insert(idx, (lines, plain));
        }
    }

    /// Sync the frozen-turn cache to the current `turns`. Frozen turns are all
    /// but the last (the last is the live, mutable one rebuilt each frame).
    /// On a wholesale replacement (`bump_render_epoch`) the cache is dropped;
    /// otherwise newly-superseded turns are rendered once, their height
    /// recorded permanently in `frozen_heights`, and their (heavy) styled
    /// lines entered into the bounded [`FrozenCache`] (oldest evicted). Heights
    /// are kept for every frozen turn so the viewport can be located and the
    /// scroll total computed without holding all rendered lines in memory.
    fn ensure_frozen(&mut self, width: usize) {
        if self.frozen_epoch != self.render_epoch {
            self.frozen_render.clear();
            self.frozen_heights.clear();
            self.frozen_epoch = self.render_epoch;
        }
        let n = self.turns.len();
        let target = n.saturating_sub(1);
        while self.frozen_heights.len() < target {
            let idx = self.frozen_heights.len();
            let theme = self.theme;
            let turn = self.materialize_turn(idx);
            let lines = {
                let cx = view::component::Cx {
                    app: self,
                    theme,
                    width,
                    active_turn: false,
                };
                view::blocks::render_turn_lines(&cx, &turn)
            };
            let plain: Vec<String> = lines
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect();
            self.frozen_heights.push(lines.len());
            self.frozen_render.insert(idx, (lines, plain));
        }
        // Defensive: turns shrank without an epoch bump.
        if self.frozen_heights.len() > target {
            self.frozen_heights.truncate(target);
            // Drop any cached entries beyond the new frozen range.
            self.frozen_render
                .map
                .retain(|idx, _| *idx < target);
            self.frozen_render.order.retain(|idx| *idx < target);
        }
    }

    fn run_active(&self) -> bool {
        self.run.is_some()
    }

    fn spinner_frame(&self) -> usize {
        self.run.unwrap_or(0)
    }

    /// `model` or `model · level` — the label shown on the working / turn-end
    /// lines. Mirrors [`session_model`].
    fn run_label(&self) -> String {
        self.session_model()
    }

    /// Elapsed since the current run started; zero when idle.
    fn run_elapsed(&self) -> Duration {
        self.run_start.map(|s| s.elapsed()).unwrap_or_default()
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
        self.history_idx = None;
    }

    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let i = self.input[..self.input_cursor]
            .char_indices()
            .last()
            .map_or(0, |(i, _)| i);
        self.input.replace_range(i..self.input_cursor, "");
        self.input_cursor = i;
        self.history_idx = None;
    }

    fn move_left(&mut self) {
        if let Some((i, _)) = self.input[..self.input_cursor].char_indices().last() {
            self.input_cursor = i;
        }
    }

    fn move_right(&mut self) {
        if let Some((_, c)) = self.input[self.input_cursor..].char_indices().next() {
            self.input_cursor += c.len_utf8();
        }
    }

    fn move_up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            return;
        }
        self.input_cursor =
            line_start_byte(&self.input, row - 1) + char_index_to_byte(&self.input, row - 1, col);
    }

    fn move_down(&mut self) {
        let (row, col) = self.cursor_row_col();
        let last_row = self.input.matches('\n').count();
        if row >= last_row {
            return;
        }
        self.input_cursor =
            line_start_byte(&self.input, row + 1) + char_index_to_byte(&self.input, row + 1, col);
    }

    fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.input[..self.input_cursor];
        let row = before.matches('\n').count();
        let col = before
            .rsplit_once('\n')
            .map_or(before.chars().count(), |(_, last)| last.chars().count());
        (row, col)
    }

    fn move_line_start(&mut self) {
        let before = &self.input[..self.input_cursor];
        self.input_cursor = before.rsplit_once('\n').map_or(0, |(b, _)| b.len() + 1);
    }

    fn move_line_end(&mut self) {
        let after = &self.input[self.input_cursor..];
        self.input_cursor += after.find('\n').unwrap_or(after.len());
    }

    fn move_word_back(&mut self) {
        self.input_cursor = prev_word_start(&self.input, self.input_cursor);
    }

    fn move_word_fwd(&mut self) {
        self.input_cursor = next_word_end(&self.input, self.input_cursor);
    }

    fn delete_forward_char(&mut self) {
        if let Some((i, c)) = self.input[self.input_cursor..].char_indices().next() {
            let end = self.input_cursor + i + c.len_utf8();
            self.input.replace_range(self.input_cursor..end, "");
        }
        self.history_idx = None;
    }

    /// Remove `input[start..end]` into the kill ring. When `append` is true the
    /// removed text is appended (consecutive `C-k`); otherwise it replaces.
    fn kill_range(&mut self, start: usize, end: usize, append: bool) {
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        if start >= end {
            return;
        }
        let removed = self.input[start..end].to_string();
        if append {
            self.kill_ring.push_str(&removed);
        } else {
            self.kill_ring = removed;
        }
        self.input.replace_range(start..end, "");
        self.input_cursor = start;
        self.history_idx = None;
    }

    fn kill_line_end(&mut self, append: bool) {
        let after = &self.input[self.input_cursor..];
        let nl = after.find('\n').unwrap_or(after.len());
        // Emacs kills the newline itself when invoked on an empty remainder,
        // so repeated `C-k` on blank lines pulls them in one at a time.
        let end = if nl == 0 {
            self.input_cursor + 1
        } else {
            self.input_cursor + nl
        };
        self.kill_range(self.input_cursor, end, append);
        self.last_kill_was_kill = true;
    }

    fn kill_line_start(&mut self) {
        let before = &self.input[..self.input_cursor];
        let start = before.rsplit_once('\n').map_or(0, |(b, _)| b.len() + 1);
        self.kill_range(start, self.input_cursor, false);
    }

    fn kill_word_back(&mut self) {
        let start = prev_word_start(&self.input, self.input_cursor);
        self.kill_range(start, self.input_cursor, false);
    }

    fn kill_word_fwd(&mut self) {
        let end = next_word_end(&self.input, self.input_cursor);
        self.kill_range(self.input_cursor, end, false);
    }

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        let s = self.kill_ring.clone();
        self.input.insert_str(self.input_cursor, &s);
        self.input_cursor += s.len();
        self.history_idx = None;
    }

    /// Up arrow / `Ctrl+P`: move to the previous line, or — when already on
    /// the first line — jump to its start, and once at the very first cell
    /// recall the previous history entry. Mirrors zsh `up-line-or-history`
    /// with a start-of-line intermediate step.
    fn cursor_up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row > 0 {
            self.move_up();
        } else if col > 0 {
            self.move_line_start();
        } else {
            self.recall_prev();
        }
    }

    /// Down arrow / `Ctrl+N`: the symmetric counterpart — next line, then end
    /// of the last line, then recall the next history entry.
    fn cursor_down(&mut self) {
        let (row, _col) = self.cursor_row_col();
        let last_row = self.input.matches('\n').count();
        if row < last_row {
            self.move_down();
        } else if self.input_cursor < self.input.len() {
            self.move_line_end();
        } else {
            self.recall_next();
        }
    }

    fn scroll_up(&mut self) {
        if self.pinned {
            self.pinned = false;
            self.top_line = self.last_base.saturating_sub(1);
        } else {
            self.top_line = self.top_line.saturating_sub(1);
        }
    }

    fn scroll_down(&mut self) {
        if self.pinned {
            return;
        }
        self.top_line = self.top_line.saturating_add(1);
        if self.top_line >= self.last_base {
            self.pinned = true;
        }
    }

    fn recall_prev(&mut self) {
        if self.history_nav.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.input_stash = self.input.clone();
                let idx = self.history_nav.len() - 1;
                self.history_idx = Some(idx);
                self.set_input(self.history_nav[idx].clone());
                self.input_cursor = 0;
            }
            Some(0) => {}
            Some(i) => {
                let idx = i - 1;
                self.history_idx = Some(idx);
                self.set_input(self.history_nav[idx].clone());
                self.input_cursor = 0;
            }
        }
    }

    fn recall_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) => {
                if i + 1 >= self.history_nav.len() {
                    self.history_idx = None;
                    let stash = std::mem::take(&mut self.input_stash);
                    self.set_input(stash);
                } else {
                    let idx = i + 1;
                    self.history_idx = Some(idx);
                    self.set_input(self.history_nav[idx].clone());
                }
            }
        }
    }

    fn set_input(&mut self, s: String) {
        self.input = s;
        self.input_cursor = self.input.len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.history_idx = None;
    }

    fn toggle_verbose(&mut self) {
        self.verbose = !self.verbose;
        // Folded tool bodies are baked into the frozen-render cache at freeze
        // time, so a toggle must invalidate it — otherwise only the live
        // (last) turn would react and earlier turns would keep the preview.
        self.bump_render_epoch();
        self.push_turn(Turn {
            prompt: "/verbose".to_string(),
            blocks: vec![Block::Text(
                if self.verbose {
                    "tool detail: expanded"
                } else {
                    "tool detail: preview"
                }
                .to_string(),
            )],
        });
    }

    /// Multi-line scroll for the mouse wheel; negative scrolls up (towards
    /// older output), positive scrolls down. Scrolling up un-pins follow mode.
    fn scroll_by(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        // Scrolling moves the viewport; a mouse selection no longer maps to
        // the visible lines, so drop it (terminals clear selection on scroll).
        self.sel = None;
        if delta < 0 {
            let n = delta.unsigned_abs() as usize;
            if self.pinned {
                self.pinned = false;
                self.top_line = self.last_base.saturating_sub(n);
            } else {
                self.top_line = self.top_line.saturating_sub(n);
            }
        } else {
            let n = usize::try_from(delta).unwrap_or(0);
            if self.pinned {
                return;
            }
            self.top_line = self.top_line.saturating_add(n);
            if self.top_line >= self.last_base {
                self.pinned = true;
            }
        }
    }

    /// Plain text of the current mouse selection, or `None` when the selection
    /// is empty (a bare click with no drag). Lines are joined with `\n`.
    #[allow(clippy::needless_range_loop)]
    fn selection_text(&self) -> Option<String> {
        let sel = self.sel.as_ref()?;
        let (sl, sc) = sel.start;
        let (el, ec) = sel.end;
        let ((sl, sc), (el, ec)) = if (sl, sc) <= (el, ec) {
            ((sl, sc), (el, ec))
        } else {
            ((el, ec), (sl, sc))
        };
        if sl == el && sc == ec {
            return None;
        }
        // `log_lines` holds only the visible window (window-relative); map the
        // absolute selection bounds into it and clamp to what's on screen.
        let lines = &self.log_lines;
        let off = self.log_off;
        let vis_len = lines.len();
        if vis_len == 0 || el < off || sl >= off + vis_len {
            return None;
        }
        let lo = sl.max(off) - off;
        let hi = el.min(off + vis_len - 1) - off;
        let mut out = String::new();
        for rel in lo..=hi {
            let li_abs = off + rel;
            let s = &lines[rel];
            let cs = if li_abs == sl { sc } else { 0 };
            let ce = if li_abs == el { ec } else { s.chars().count() };
            let chars: Vec<(usize, char)> = s.char_indices().collect();
            let b0 = if cs == 0 { 0 } else { chars[cs - 1].0 + chars[cs - 1].1.len_utf8() };
            let b1 = if ce == 0 { 0 } else { chars[ce - 1].0 + chars[ce - 1].1.len_utf8() };
            out.push_str(&s[b0..b1]);
            if rel < hi {
                out.push('\n');
            }
        }
        Some(out)
    }

    fn clear_log(&mut self) {
        self.turns.clear();
        self.turn_byte_ranges.clear();
        self.pinned = true;
        self.top_line = 0;
        self.bump_render_epoch();
    }

    /// Handle a submitted line starting with '/'. Returns true if it was a
    /// recognized command (so the caller does not start a run).
    fn slash_command(&mut self, line: &str) -> bool {
        let cmd = line.trim();
        match cmd {
            "/clear" => {
                self.clear_log();
                true
            }
            "/quit" | "/exit" => {
                self.should_quit = true;
                true
            }
            "/help" => {
                self.push_help();
                true
            }
            "/new" => {
                self.start_new_session();
                true
            }
            "/session" => {
                self.push_session_info();
                true
            }
            "/resume" => {
                self.open_picker();
                true
            }
            "/verbose" => {
                self.toggle_verbose();
                true
            }
            _ if cmd.starts_with('/') => {
                self.push_turn(Turn {
                    prompt: cmd.to_string(),
                    blocks: vec![Block::Error(format!(
                        "unknown command: {cmd} (try /help)"
                    ))],
                });
                true
            }
            _ => false,
        }
    }

    fn push_help(&mut self) {
        let help = "Keys\n  Enter        send  ·  Alt+Enter / Ctrl+J  newline\n  ↑ / ↓        move line, recall at edge  ·  Ctrl+↑/↓  move across lines\n  PgUp / PgDn  scroll log  ·  Esc  clear input\n  Ctrl+C       cancel run  ·  Ctrl+D  quit\nCommands\n  /help        this help  ·  /clear  clear log\n  /new         start a fresh session  ·  /resume  pick a past session\n  /session     show session info  ·  /verbose  toggle tool detail\n  /quit        exit";
        self.push_turn(Turn {
            prompt: "/help".to_string(),
            blocks: vec![Block::Text(help.to_string())],
        });
    }

    fn push_session_info(&mut self) {
        let info = match &self.session.path {
            Some(p) => format!(
                "session: {}\nmessages: {}\nmodel: {}",
                p.display(),
                self.history.lock().map_or(0, |m| m.len()),
                self.session_model(),
            ),
            None => "no session file (ephemeral or not yet started)".to_string(),
        };
        self.push_turn(Turn {
            prompt: "/session".to_string(),
            blocks: vec![Block::Text(info)],
        });
    }

    /// '/new': drop the transcript and start a fresh session file on the next
    /// prompt. The store is retained; only the path/name/log are reset.
    fn start_new_session(&mut self) {
        if let Ok(mut m) = self.history.lock() {
            m.clear();
        }
        self.turns.clear();
        self.turn_byte_ranges.clear();
        self.session.path = None;
        self.pinned = true;
        self.top_line = 0;
        self.bump_render_epoch();
    }

    /// Populate the '/resume' picker with sessions for this workspace.
    fn open_picker(&mut self) {
        let Some(store) = &self.session.store else {
            self.push_turn(Turn {
                prompt: "/resume".to_string(),
                blocks: vec![Block::Error("sessions are disabled (--no-session)".into())],
            });
            return;
        };
        match store.list_for_cwd(&self.session.cwd) {
            Ok(entries) if entries.is_empty() => {
                self.push_turn(Turn {
                    prompt: "/resume".to_string(),
                    blocks: vec![Block::Text("no saved sessions for this workspace".into())],
                });
            }
            Ok(entries) => {
                self.picker = Some(PickerState {
                    entries,
                    selected: 0,
                });
            }
            Err(e) => {
                self.push_turn(Turn {
                    prompt: "/resume".to_string(),
                    blocks: vec![Block::Error(format!("list sessions: {e}"))],
                });
            }
        }
    }

    fn picker_up(&mut self) {
        if let Some(p) = &mut self.picker {
            if p.selected > 0 {
                p.selected -= 1;
            }
        }
    }

    fn picker_down(&mut self) {
        if let Some(p) = &mut self.picker {
            p.selected = (p.selected + 1).min(p.entries.len().saturating_sub(1));
        }
    }

    fn picker_cancel(&mut self) {
        self.picker = None;
    }

    /// Load the selected session into the transcript and close the picker.
    fn picker_confirm(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        let entry = picker.entries.into_iter().nth(picker.selected);
        let Some(entry) = entry else {
            return;
        };
        match store::load(&entry.path) {
            Ok((_meta, events, offsets, file_size)) => {
                let messages = messages_from_events(&events);
                if let Ok(mut m) = self.history.lock() {
                    *m = messages;
                }
                self.turns = turns_from_events(&events);
                self.turn_byte_ranges =
                    turn_byte_ranges_from_events(&events, &offsets, file_size);
                self.accumulate_totals(&events);
                // Freeze all but the last turn (file-backed; see `run_loop`).
                if self.turns.len() > 1 {
                    let n = self.turns.len();
                    for turn in &mut self.turns[..n - 1] {
                        turn.blocks.clear();
                    }
                }
                self.bump_render_epoch();
                self.session.path = Some(entry.path);
                self.pinned = true;
                self.top_line = 0;
            }
            Err(e) => {
                self.push_turn(Turn {
                    prompt: "/resume".to_string(),
                    blocks: vec![Block::Error(format!("load session: {e}"))],
                });
            }
        }
    }

    /// Header line: `lofi` wordmark at the left, the working directory
    /// (abbreviated to fit) right-aligned. The model and cost live in the
    /// footer; the header carries no status tag.
    pub(crate) fn render_header_line(&self, width: usize) -> Line<'static> {
        let t = self.theme;
        let wordmark = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
        let muted = Style::new().fg(t.muted);
        let lofi_w = unicode_width::UnicodeWidthStr::width("lofi");
        // Reserve a 2-cell gap between the wordmark and the path; HStack owns
        // the actual gutter from the leftover width.
        let budget = width.saturating_sub(lofi_w).saturating_sub(2);
        let cwd = abbreviate_path(&self.session.cwd, budget);
        HStack::new(width)
            .left([Span::styled("lofi", wordmark)])
            .right([Span::styled(cwd, muted)])
            .build()
    }

    /// Bottom-left footer: `↑in ↓out · ctx: used/limit · $cost`. The path
    /// lives in the header now; the footer carries usage, context, and cost.
    pub(crate) fn render_footer_left(&self, _width: usize) -> Line<'static> {
        let t = self.theme;
        let sep = " · ";
        let mut segments: Vec<String> = Vec::new();
        if self.total_in > 0 || self.total_out > 0 {
            segments.push(format!(
                "↑{} ↓{}",
                compact_count(self.total_in),
                compact_count(self.total_out)
            ));
        }
        let used = self
            .status_usage
            .map_or(0, |u| u.input_tokens + u.output_tokens);
        segments.push(format!(
            "ctx: {}/{}",
            compact_count(used),
            compact_count(self.ctx_limit)
        ));
        // Show only when the cost rounds above $0.00, so trivial sub-cent
        // runs do not clutter the footer.
        let c = fmt_cost(self.cost);
        if c != "$0.00" {
            segments.push(c);
        }
        Line::styled(segments.join(sep), Style::new().fg(t.subtle))
    }

    /// Bottom-right footer: the model badge (with thinking level).
    pub(crate) fn render_footer_right(&self) -> Line<'static> {
        let mut label = self.model_label.clone();
        if let Some(tl) = &self.thinking_label {
            label.push_str(tl);
        }
        Line::styled(label, Style::new().fg(self.theme.muted))
    }

    }

/// Borrow the last Tool block matching id (newest-first).
/// Stamp the elapsed duration on the trailing thinking block, if any is
/// still open. Called whenever the stream moves on to a different block
/// kind or the run ends.
fn finalize_open_thinking(turn: &mut Turn) {
    if let Some(Block::Thinking(t)) = turn.blocks.last_mut() {
        if t.elapsed.is_none() {
            t.elapsed = Some(t.start.elapsed());
        }
    }
}

fn tool_mut<'a>(blocks: &'a mut [Block], id: &str) -> Option<&'a mut ToolCall> {
    blocks.iter_mut().rev().find_map(|b| match b {
        Block::Tool(t) if t.id == id => Some(t),
        _ => None,
    })
}

/// Byte offset of the start of row (0-indexed) in s.
fn char_is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the start of the word before `cursor` (emacs
/// `backward-word`): skip non-word chars backwards, then word chars.
fn prev_word_start(s: &str, cursor: usize) -> usize {
    let chars: Vec<(usize, char)> = s[..cursor].char_indices().collect();
    let mut i = chars.len();
    while i > 0 && !char_is_word(chars[i - 1].1) {
        i -= 1;
    }
    while i > 0 && char_is_word(chars[i - 1].1) {
        i -= 1;
    }
    chars.get(i).map_or(0, |(b, _)| *b)
}

/// Byte offset just past the end of the word at/after `cursor` (emacs
/// `forward-word`): skip non-word forwards, then word chars.
fn next_word_end(s: &str, cursor: usize) -> usize {
    let mut byte = cursor;
    let mut in_word = false;
    for (i, c) in s[cursor..].char_indices() {
        let at = cursor + i;
        if char_is_word(c) {
            in_word = true;
            byte = at + c.len_utf8();
        } else if in_word {
            break;
        } else {
            byte = at + c.len_utf8();
        }
    }
    byte
}

/// Compact token/byte count: `9.7M`, `119k`, `500`.
#[allow(clippy::cast_precision_loss)]
fn compact_count(n: u64) -> String {
    if n >= 1_000_000 {
        let v = n as f64 / 1_000_000.0;
        let s = format!("{v:.1}");
        let s = s.trim_end_matches('0').trim_end_matches('.');
        format!("{s}M")
    } else if n >= 1_000 {
        format!("{}k", n / 1_000)
    } else {
        n.to_string()
    }
}

/// USD cost with trailing zeros trimmed: `$81.4`, `$81`.
fn fmt_cost(c: f64) -> String {
    format!("${c:.2}")
}

/// One component of an abbreviated path: `Dev` -> `D`, `~sirn` -> `~s`.
fn abbrev_component(c: &str) -> String {
    if let Some(rest) = c.strip_prefix('~') {
        let head = rest.chars().next().map(|ch| ch.to_string()).unwrap_or_default();
        format!("~{head}")
    } else {
        c.chars().next().map(|ch| ch.to_string()).unwrap_or_default()
    }
}

/// Render a path as `~/...` when under the home dir, then abbreviate to fit
/// `max` display cells: keep the first and last component, shorten the middle
/// to one char each; if that still does not fit, fall back to the basename.
fn abbreviate_path(path: &std::path::Path, max: usize) -> String {
    let full: String = match dirs::home_dir() {
        Some(h) if path.starts_with(&h) => match path.strip_prefix(&h) {
            Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Ok(rest) => format!("~/{}", rest.to_string_lossy()),
            Err(_) => path.to_string_lossy().to_string(),
        },
        _ => path.to_string_lossy().to_string(),
    };
    let w = |s: &str| unicode_width::UnicodeWidthStr::width(s);
    if w(&full) <= max {
        return full;
    }
    let comps: Vec<&str> = full.split('/').collect();
    let abbr = if comps.len() > 2 {
        let first = comps[0];
        let last = comps[comps.len() - 1];
        let mid: Vec<String> = comps[1..comps.len() - 1]
            .iter()
            .map(|c| abbrev_component(c))
            .collect();
        format!("{}/{}/{}", first, mid.join("/"), last)
    } else {
        full.clone()
    };
    if w(&abbr) <= max {
        return abbr;
    }
    comps.last().copied().unwrap_or("").to_string()
}

fn line_start_byte(s: &str, row: usize) -> usize {
    if row == 0 {
        return 0;
    }
    s.char_indices()
        .filter(|(_, c)| *c == '\n')
        .nth(row - 1)
        .map_or(s.len(), |(i, _)| i + 1)
}

/// Convert a char column to bytes within a given logical line of s.
fn char_index_to_byte(s: &str, row: usize, col: usize) -> usize {
    let start = line_start_byte(s, row);
    let line_end = s[start..].find('\n').map_or(s.len(), |i| start + i);
    s[start..line_end]
        .char_indices()
        .nth(col)
        .map_or(line_end - start, |(i, _)| i)
}

/// Number of visual rows a single logical line occupies when soft-wrapped to
/// `content_w` cells. Mirrors the wrap loop in [`App::input_visual_rows`].
fn count_wrapped_rows(line: &str, content_w: usize) -> usize {
    if content_w == 0 || line.is_empty() {
        return 1;
    }
    let mut rows = 1usize;
    let mut cur_w = 0usize;
    for c in line.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w + cw > content_w && cur_w > 0 {
            rows += 1;
            cur_w = 0;
        }
        cur_w += cw;
    }
    rows
}

/// Wrap a cursor prefix (the text before the cursor on its logical line) and
/// return `(sub_rows_before, x_on_final_row)`, matching the wrap loop in
/// [`App::input_visual_rows`] so the cursor lands exactly where the text
/// would break.
fn wrap_prefix_pos(prefix: &str, content_w: usize) -> (usize, usize) {
    if content_w == 0 {
        return (0, unicode_width::UnicodeWidthStr::width(prefix));
    }
    let mut sub = 0usize;
    let mut cur_w = 0usize;
    for c in prefix.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cur_w + cw > content_w && cur_w > 0 {
            sub += 1;
            cur_w = 0;
        }
        cur_w += cw;
    }
    (sub, cur_w)
}

/// Build per-turn byte ranges from a transcript event log and the byte
/// offset of each event's line. A turn starts at a `User` message that isn't
/// a tool-result (mirroring [`turns_from_events`]); its byte range runs from
/// that line's offset to the next turn's start, or to `file_size` for the
/// last turn. Parallel to the `Vec<Turn>` returned by [`turns_from_events`].
fn turn_byte_ranges_from_events(
    events: &[SessionEvent],
    offsets: &[u64],
    file_size: u64,
) -> Vec<Option<(u64, u64)>> {
    let mut ranges: Vec<Option<(u64, u64)>> = Vec::new();
    let mut cur_start: Option<u64> = None;
    for (i, ev) in events.iter().enumerate() {
        let is_turn_start = matches!(
            ev,
            SessionEvent::Message(m)
                if m.role == Role::User
                    && !m.blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        );
        if is_turn_start {
            if let Some(start) = cur_start {
                if let Some(last) = ranges.last_mut() {
                    *last = Some((start, offsets[i]));
                }
            }
            cur_start = Some(offsets[i]);
            ranges.push(None);
        }
    }
    if let Some(start) = cur_start {
        if let Some(last) = ranges.last_mut() {
            *last = Some((start, file_size));
        }
    }
    ranges
}

/// Reconstruct the TUI turn view from a transcript event log. Tool timings
/// and native-tool calls are gathered first (they are written after the
/// messages) so each `ToolUse` block can be stamped as it is built; a
/// `TurnEnd` event attaches the `◇ label done in Ns` block to the turn it
/// follows.
#[allow(clippy::too_many_lines)]
fn turns_from_events(events: &[SessionEvent]) -> Vec<Turn> {
    let mut tool_elapsed: HashMap<String, Duration> = HashMap::new();
    let mut native_by_parent: HashMap<String, Vec<NativeTool>> = HashMap::new();
    for ev in events {
        match ev {
            SessionEvent::ToolTiming { id, elapsed_ms } => {
                tool_elapsed.insert(id.clone(), Duration::from_millis(*elapsed_ms));
            }
            SessionEvent::NativeTool(rec) => {
                native_by_parent
                    .entry(rec.parent.clone())
                    .or_default()
                    .push(NativeTool {
                        id: rec.id,
                        name: rec.name.clone(),
                        args: rec.args.clone(),
                        result: Some(rec.result.clone()),
                        is_error: rec.is_error,
                        done: true,
                    });
            }
            _ => {}
        }
    }
    let mut turns: Vec<Turn> = Vec::new();
    for ev in events {
        match ev {
            SessionEvent::Message(msg) => match msg.role {
                Role::User => {
                    // ToolResult blocks attach to the current turn's tool blocks.
                    if msg.blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })) {
                        if let Some(turn) = turns.last_mut() {
                            for b in &msg.blocks {
                                if let ContentBlock::ToolResult { tool_use_id, content, is_error } = b {
                                    if let Some(Block::Tool(t)) =
                                        turn.blocks.iter_mut().rev().find(|bl| {
                                            matches!(bl, Block::Tool(tc) if tc.id == *tool_use_id)
                                        }) {
                                        let label = if *is_error {
                                            format!("error: {content}")
                                        } else {
                                            content.clone()
                                        };
                                        t.result = Some(label);
                                        t.is_error = *is_error;
                                        t.done = true;
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    // Otherwise a prompt: start a new turn from its first text.
                    let prompt = msg
                        .blocks
                        .iter()
                        .find_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    turns.push(Turn {
                        prompt,
                        blocks: Vec::new(),
                    });
                }
                Role::Assistant => {
                    let Some(turn) = turns.last_mut() else {
                        continue;
                    };
                    for b in &msg.blocks {
                        match b {
                            ContentBlock::Text { text } => {
                                turn.blocks.push(Block::Text(text.clone()));
                            }
                            ContentBlock::Thinking { text, .. } => {
                                turn.blocks.push(Block::Thinking(ThinkingBlock {
                                    text: text.clone(),
                                    start: Instant::now(),
                                    elapsed: Some(Duration::ZERO),
                                }));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                // For a restored `exec`, split the stored input
                                // JSON back into the code (shown with line
                                // numbers) and the `display` label.
                                let (code, label) = if name == "exec" {
                                    lofi_core::exec_input_code_and_label(input)
                                } else {
                                    (input.to_string(), None)
                                };
                                turn.blocks.push(Block::Tool(ToolCall {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: code,
                                    label,
                                    native: native_by_parent
                                        .remove(id)
                                        .unwrap_or_default(),
                                    result: None,
                                    is_error: false,
                                    done: true,
                                    elapsed: tool_elapsed.get(id).copied(),
                                }));
                            }
                            ContentBlock::ToolResult { .. } => {}
                        }
                    }
                }
                Role::System | Role::Tool => {}
            },
            SessionEvent::NativeTool(_) | SessionEvent::ToolTiming { .. } => {}
            SessionEvent::TurnEnd { label, elapsed_ms, .. } => {
                if let Some(turn) = turns.last_mut() {
                    turn.blocks.push(Block::TurnEnd {
                        label: label.clone(),
                        elapsed: Duration::from_millis(*elapsed_ms),
                    });
                }
            }
        }
    }
    turns
}

/// Extract the conversation messages from a transcript event log, for the
/// agent's in-memory history on resume.
fn messages_from_events(events: &[SessionEvent]) -> Vec<Message> {
    events
        .iter()
        .filter_map(|ev| match ev {
            SessionEvent::Message(m) => Some(m.clone()),
            _ => None,
        })
        .collect()
}

struct RunHandle {
    handle: JoinHandle<()>,
    rx: Receiver<AgentEvent>,
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn draw(&mut self, app: &mut App) -> Result<()> {
        self.terminal
            .draw(|f| view::render(f, app))
            .map_err(Error::Io)?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            self.terminal.backend_mut(),
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = disable_raw_mode();
    }
}

/// Enter the interactive TUI.
///
/// `agent` is `None` when no model is configured: the UI still launches and
/// shows `no_models_hint` in the log; submitting a prompt re-surfaces the
/// hint instead of running.
pub(crate) async fn run(
    agent: Option<Agent>,
    model_label: String,
    thinking: ThinkingLevel,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
) -> Result<()> {
    enable_raw_mode().map_err(Error::Io)?;
    let setup = (|| -> std::io::Result<_> {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    let terminal = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
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
                agent.as_ref(),
                model_label,
                thinking,
                session,
                no_models_hint,
                ctx_limit,
            )
            .await
        })
        .await;
    result
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_loop(
    guard: &mut TerminalGuard,
    agent: Option<&Agent>,
    model_label: String,
    thinking: ThinkingLevel,
    session: SessionConfig,
    no_models_hint: Option<String>,
    ctx_limit: u64,
) -> Result<()> {
    let SessionConfig {
        store,
        path,
        events,
        offsets,
        file_size,
        cwd,
    } = session;
    let mut app = App::new(model_label, thinking, ctx_limit);
    app.session = SessionState {
        store,
        path,
        cwd,
    };
    let messages = messages_from_events(&events);
    if let Ok(mut m) = app.history.lock() {
        m.clone_from(&messages);
    }
    app.turns = turns_from_events(&events);
    app.turn_byte_ranges = turn_byte_ranges_from_events(&events, &offsets, file_size);
    app.accumulate_totals(&events);
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
    // The event log was only needed to rebuild the view and history; free it
    // now so a large transcript isn't held for the session's lifetime.
    drop(events);
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

    let mut current_run: Option<RunHandle> = None;
    let mut events = EventStream::new();
    let mut last_err: Option<String> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut dirty = true;
    loop {
        if dirty {
            guard.draw(&mut app)?;
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
                    Some(e) => app.apply_event(e),
                    None => {
                        if let Some(r) = current_run.take() {
                            r.handle.abort();
                            app.run_finished();
                        }
                    }
                }
                dirty = true;
            }
            maybe_ev = events.next() => {
                match maybe_ev {
                    Some(Ok(ev)) => {
                        handle_event(&ev, &mut app, agent, &mut current_run);
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
                dirty = true;
            }
            _ = tick.tick() => {
                // Only the spinner needs periodic redraw; an idle session
                // has nothing to animate, so skip the draw entirely.
                if app.run.is_some() {
                    if let Some(s) = app.run.as_mut() {
                        *s = s.wrapping_add(1);
                    }
                    dirty = true;
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    if let Some(r) = current_run.take() {
        r.handle.abort();
    }
    if let Some(msg) = last_err {
        return Err(Error::Io(std::io::Error::other(msg)));
    }
    Ok(())
}

// A single large key dispatcher; splitting per-key handlers would fragment
// the picker/submit/run-creation flow and hurt readability more than the line
// count helps.
#[allow(clippy::too_many_lines, clippy::match_same_arms)]
fn handle_event(
    ev: &Event,
    app: &mut App,
    agent: Option<&Agent>,
    current_run: &mut Option<RunHandle>,
) {
    if let Event::Mouse(m) = ev {
        handle_mouse(*m, app);
        return;
    }
    let Event::Key(k) = ev else {
        return;
    };
    if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    // Any key press clears an active mouse selection (tmux-style).
    app.sel = None;
    // Capture before reset so consecutive `C-k` appends to the kill ring.
    let append_kill = app.last_kill_was_kill;
    app.last_kill_was_kill = false;

    // The '/resume' picker intercepts keys while open.
    if app.picker.is_some() {
        match k.code {
            KeyCode::Up => app.picker_up(),
            KeyCode::Down => app.picker_down(),
            KeyCode::Enter => app.picker_confirm(),
            KeyCode::Esc => app.picker_cancel(),
            _ => {}
        }
        return;
    }

    if k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::ALT) {
        app.insert_newline();
        return;
    }
    // Ctrl+J is a newline in readline / Emacs; treat it like Alt+Enter.
    if k.code == KeyCode::Char('j') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.insert_newline();
        return;
    }
    match k.code {
        KeyCode::Enter if current_run.is_none() && !app.input.is_empty() => {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.history_idx = None;
            if app.slash_command(&prompt) {
                return;
            }
            app.history_nav.push(prompt.clone());
            // Lazily create the transcript file on the first persisted prompt.
            if app.session.path.is_none() {
                if let Some(store) = &app.session.store {
                    if let Ok(p) =
                        store.create(&app.session.cwd, &app.session_model())
                    {
                        app.session.path = Some(p);
                    }
                }
            }
            // Freeze the previous turn: if it was committed to the
            // transcript (has a byte range), its blocks are now file-backed
            // (see `materialize_turn`) — drop them so memory stays bounded.
            // UI-only turns (slash commands) have no byte range and keep blocks.
            let prev = app.turns.len();
            if prev > 0
                && app
                    .turn_byte_ranges
                    .get(prev - 1)
                    .is_some_and(Option::is_some)
            {
                app.turns[prev - 1].blocks.clear();
            }
            app.push_turn(Turn {
                prompt: prompt.clone(),
                blocks: Vec::new(),
            });
            let Some(agent) = agent else {
                // No model configured: re-surface the hint on this turn.
                if let Some(hint) = &app.no_models_hint {
                    if let Some(turn) = app.turns.last_mut() {
                        turn.blocks.push(Block::Error(hint.clone()));
                    }
                }
                return;
            };
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let history = Arc::clone(&app.history);
            let session_path = app.session.path.clone();
            let commit = session_path.as_ref().map(|p| SessionCommit {
                path: p.clone(),
                label: app.session_model(),
            });
            let agent_clone = agent.clone();
            let err_tx = tx.clone();
            let handle = tokio::task::spawn_local(async move {
                let mut messages = history.lock().map(|m| m.clone()).unwrap_or_default();
                // The engine owns the timers and cost, and writes the turn's
                // events (messages + timings + turn-end) to the transcript.
                let result = agent_clone
                    .run_continuation(&mut messages, prompt, tx, commit.as_ref())
                    .await;
                if let Ok(mut g) = history.lock() {
                    *g = messages;
                }
                if let Err(e) = result {
                    let _ = err_tx.send(AgentEvent::Error(e.to_string())).await;
                }
            });
            *current_run = Some(RunHandle { handle, rx });
            app.run = Some(0);
            app.run_start = Some(Instant::now());
            app.pinned = true;
        }
        KeyCode::Backspace if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_back(),
        KeyCode::Backspace => app.backspace(),
        KeyCode::Delete => app.delete_forward_char(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_up(),
        KeyCode::Down if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_down(),
        KeyCode::Up => app.cursor_up(),
        KeyCode::Down => app.cursor_down(),
        KeyCode::PageUp => app.scroll_up(),
        KeyCode::PageDown => app.scroll_down(),
        KeyCode::Esc => app.clear_input(),
        // Emacs / readline navigation.
        KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_line_start(),
        KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_line_end(),
        KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_left(),
        KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::CONTROL) => app.move_right(),
        KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_down(),
        KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => app.cursor_up(),
        KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::CONTROL) => app.backspace(),
        KeyCode::Char('k') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_line_end(append_kill),
        KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_line_start(),
        KeyCode::Char('w') if k.modifiers.contains(KeyModifiers::CONTROL) => app.kill_word_back(),
        KeyCode::Char('y') if k.modifiers.contains(KeyModifiers::CONTROL) => app.yank(),
        KeyCode::Char('b') if k.modifiers.contains(KeyModifiers::ALT) => app.move_word_back(),
        KeyCode::Char('f') if k.modifiers.contains(KeyModifiers::ALT) => app.move_word_fwd(),
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::ALT) => app.kill_word_fwd(),
        KeyCode::Char('<') if k.modifiers.contains(KeyModifiers::ALT) => app.input_cursor = 0,
        KeyCode::Char('>') if k.modifiers.contains(KeyModifiers::ALT) => app.input_cursor = app.input.len(),
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(r) = current_run.take() {
                r.handle.abort();
                if let Some(turn) = app.turns.last_mut() {
                    turn.blocks.push(Block::Error("cancelled".to_string()));
                }
                app.run_finished();
            }
        }
        // Readline `C-d`: delete the char under the cursor, or send EOF (quit)
        // when the input is empty.
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if app.input.is_empty() {
                app.should_quit = true;
            } else {
                app.delete_forward_char();
            }
        }
        
        // Insert printable chars unless a control/alt combo is claimed by an
        // arm above; Shift is already folded into `c` so it must be allowed.
        KeyCode::Char(c) if !k.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            app.insert_char(c);
        }
        _ => {}
    }
}

fn handle_mouse(m: MouseEvent, app: &mut App) {
    let in_log = m.row >= app.log_rect.y
        && m.row < app.log_rect.y + app.log_rect.height
        && m.column >= app.log_rect.x
        && m.column < app.log_rect.x + app.log_rect.width;
    match m.kind {
        MouseEventKind::ScrollUp if in_log => app.scroll_by(-3),
        MouseEventKind::ScrollDown if in_log => app.scroll_by(3),
        MouseEventKind::Down(MouseButton::Left) => {
            app.sel = None;
            if in_log {
                let cell = log_cell(app, m.row, m.column);
                app.sel = Some(Selection { start: cell, end: cell });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if in_log => {
            let cell = log_cell(app, m.row, m.column);
            if let Some(sel) = app.sel.as_mut() {
                sel.end = cell;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(text) = app.selection_text() {
                let b64 = base64::engine::general_purpose::STANDARD.encode(text);
                // OSC 52 clipboard copy: the terminal writes the selection to
                // the system clipboard, matching tmux / Crush behavior. Native
                // selection isn't available while we own the mouse.
                let _ = write!(io::stdout(), "\x1b]52;c;{b64}\x07");
                let _ = io::stdout().flush();
            }
        }
        _ => {}
    }
}

/// Map a screen cell inside the log viewport to (visual-line index, char
/// index) in `log_lines`, using display width so wide chars land correctly.
fn log_cell(app: &App, row: u16, column: u16) -> (usize, usize) {
    let rel_y = row.saturating_sub(app.log_rect.y) as usize;
    let rel_x = column.saturating_sub(app.log_rect.x) as usize;
    let line_idx = app.log_off.saturating_add(rel_y);
    // `log_lines` is the visible window only (window-relative); index by `rel_y`.
    let line = app.log_lines.get(rel_y);
    let col = line.map_or(rel_x, |s| col_to_char_idx(s, rel_x));
    (line_idx, col)
}

/// Index of the char whose display column is `col` (i.e. the cursor position
/// `col` cells from the left). Never splits a wide char: it lands on the
/// boundary before it.
fn col_to_char_idx(s: &str, col: usize) -> usize {
    let mut w = 0usize;
    let mut count = 0usize;
    for c in s.chars() {
        if w >= col {
            return count;
        }
        w += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{ContentBlock, Role, Usage};

    fn app() -> App {
        App::new("openai/gpt-4o".to_string(), ThinkingLevel::Medium, 0)
    }

    fn push_turn(app: &mut App) {
        app.turns.push(Turn {
            prompt: "p".to_string(),
            blocks: Vec::new(),
        });
    }

    #[test]
    fn native_tool_events_nest_under_their_exec() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::ToolStart {
            id: "e1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolInput {
            id: "e1".to_string(),
            code: "lofi.bash('ls')".to_string(),
            label: Some("list files".to_string()),
        });
        a.apply_event(AgentEvent::NativeToolStart {
            parent: "e1".to_string(),
            id: 0,
            name: "bash".to_string(),
            args: "ls".to_string(),
        });
        a.apply_event(AgentEvent::NativeToolEnd {
            parent: "e1".to_string(),
            id: 0,
            result: "file.txt".to_string(),
            is_error: false,
        });
        let Block::Tool(exec) = &a.turns[0].blocks[0] else {
            panic!("expected an exec tool block");
        };
        assert_eq!(exec.name, "exec");
        assert_eq!(exec.label.as_deref(), Some("list files"));
        assert_eq!(exec.input, "lofi.bash('ls')");
        assert_eq!(exec.native.len(), 1);
        let nt = &exec.native[0];
        assert_eq!(nt.name, "bash");
        assert_eq!(nt.args, "ls");
        assert!(nt.done);
        assert_eq!(nt.result.as_deref(), Some("file.txt"));
        assert!(!nt.is_error);
    }

    #[test]
    fn render_tree_smoke() {
        use crate::tui::view::blocks::render_turns;
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::Text("Hello, this is Lofi.".to_string()));
        a.apply_event(AgentEvent::ToolStart {
            id: "e1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolInput {
            id: "e1".to_string(),
            code: "const x = 1;\nlofi.read('a.txt')".to_string(),
            label: Some("read a".to_string()),
        });
        a.apply_event(AgentEvent::NativeToolStart {
            parent: "e1".to_string(),
            id: 0,
            name: "read".to_string(),
            args: "a.txt".to_string(),
        });
        a.apply_event(AgentEvent::NativeToolEnd {
            parent: "e1".to_string(),
            id: 0,
            result: "line one\nline two".to_string(),
            is_error: false,
        });
        a.apply_event(AgentEvent::ToolEnd {
            id: "e1".to_string(),
            result: "{\"value\":null}".to_string(),
            is_error: false,
            elapsed_ms: 0,
        });
        // Turn is done (run not active): must not panic and must render.
        let text = render_turns(&a, 80);
        assert!(!text.lines.is_empty());
    }

    #[test]
    fn empty_text_blocks_leave_no_gap() {
        use crate::tui::view::blocks::render_turns;
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::Thinking("Let me explore.".to_string()));
        // Reasoning models often emit a whitespace-only content block between
        // the reasoning trace and a tool call; it must not render as a gap.
        a.apply_event(AgentEvent::Text("\n".to_string()));
        a.apply_event(AgentEvent::Text("  \n  ".to_string()));
        a.apply_event(AgentEvent::ToolStart {
            id: "e1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolInput {
            id: "e1".to_string(),
            code: "return 1;".to_string(),
            label: None,
        });
        a.apply_event(AgentEvent::ToolEnd {
            id: "e1".to_string(),
            result: "{\"value\":1}".to_string(),
            is_error: false,
            elapsed_ms: 0,
        });
        let text = render_turns(&a, 80);
        let blanks: Vec<bool> = text
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .trim()
                    .is_empty()
            })
            .collect();
        let max_run = blanks
            .iter()
            .fold((0usize, 0usize), |(mx, cur), &b| {
                if b {
                    (mx.max(cur + 1), cur + 1)
                } else {
                    (mx, 0)
                }
            })
            .0;
        // At most the Stack separator plus one exec-padding fill (= 2) should
        // ever appear consecutively; whitespace-only text blocks leave nothing.
        assert!(
            max_run <= 2,
            "max blank run {max_run}; lines:\n{:#?}",
            text.lines
        );
    }

    fn last_text(app: &App) -> Option<&str> {
        app.turns.last()?.blocks.iter().rev().find_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
    }

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn text_deltas_accumulate_into_one_text_block() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::Text("hel".to_string()));
        a.apply_event(AgentEvent::Text("lo".to_string()));
        assert_eq!(last_text(&a), Some("hello"));
        assert_eq!(a.turns[0].blocks.len(), 1);
    }

    #[test]
    fn thinking_deltas_accumulate_into_their_own_block() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::Thinking("hm".to_string()));
        a.apply_event(AgentEvent::Text("hi".to_string()));
        a.apply_event(AgentEvent::Thinking("more".to_string()));
        let blocks = &a.turns[0].blocks;
        assert!(matches!(blocks[0], Block::Thinking(_)));
        assert!(matches!(blocks[1], Block::Text(_)));
        assert!(matches!(blocks[2], Block::Thinking(_)));
        assert_eq!(blocks.len(), 3);
    }

    #[test]
    fn tool_start_then_text_starts_new_text_block() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::Text("first".to_string()));
        a.apply_event(AgentEvent::ToolStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::Text("second".to_string()));
        let blocks = &a.turns[0].blocks;
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0], Block::Text(_)));
        assert!(matches!(blocks[1], Block::Tool(_)));
        assert!(matches!(blocks[2], Block::Text(_)));
    }

    #[test]
    fn tool_input_and_end_land_under_matching_id() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::ToolStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolStart {
            id: "t2".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolInput {
            id: "t1".to_string(),
            code: "code-1".to_string(),
            label: None,
        });
        a.apply_event(AgentEvent::ToolEnd {
            id: "t2".to_string(),
            result: "r2".to_string(),
            is_error: false,
            elapsed_ms: 0,
        });
        a.apply_event(AgentEvent::ToolEnd {
            id: "t1".to_string(),
            result: "r1".to_string(),
            is_error: false,
            elapsed_ms: 0,
        });
        let blocks = &a.turns[0].blocks;
        let Block::Tool(t1) = &blocks[0] else {
            unreachable!()
        };
        let Block::Tool(t2) = &blocks[1] else {
            unreachable!()
        };
        assert_eq!(t1.input, "code-1");
        assert_eq!(t1.result.as_deref(), Some("r1"));
        assert!(t1.done);
        assert_eq!(t2.result.as_deref(), Some("r2"));
        assert!(t2.done);
    }

    #[test]
    fn turn_end_updates_usage() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::TurnEnd {
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        assert_eq!(a.status_usage.unwrap().output_tokens, 20);
    }

    #[test]
    fn multi_line_input_editing() {
        let mut a = app();
        a.insert_char('a');
        a.insert_newline();
        a.insert_char('b');
        assert_eq!(a.input, "a\nb");
        assert_eq!(a.cursor_row_col(), (1, 1));
        a.move_up();
        assert_eq!(a.cursor_row_col().0, 0);
        a.move_down();
        assert_eq!(a.cursor_row_col().0, 1);
    }

    #[test]
    fn input_lines_counts_logical_lines() {
        let mut a = app();
        assert_eq!(a.input_lines(120), 1);
        a.insert_newline();
        a.insert_newline();
        assert_eq!(a.input_lines(120), 3);
    }

    #[test]
    fn cursor_up_recalls_at_first_cell() {
        let mut a = app();
        a.history_nav.push("first".to_string());
        a.history_nav.push("second".to_string());
        a.set_input("Hello".to_string());
        // first line, not at start -> jump to start (no recall yet)
        a.cursor_up();
        assert_eq!(a.input, "Hello");
        assert_eq!(a.input_cursor, 0);
        // at first cell -> recall previous ("second"), cursor parked at start
        a.cursor_up();
        assert_eq!(a.input, "second");
        assert_eq!(a.input_cursor, 0);
        // recall again -> "first"
        a.cursor_up();
        assert_eq!(a.input, "first");
        assert_eq!(a.input_cursor, 0);
        // at the oldest entry, another up is a no-op
        a.cursor_up();
        assert_eq!(a.input, "first");
    }

    #[test]
    fn cursor_down_recalls_at_last_cell() {
        let mut a = app();
        a.history_nav.push("first".to_string());
        a.history_nav.push("second".to_string());
        a.set_input("Hello".to_string());
        // walk back to "first" (cursor at start)
        a.cursor_up();
        a.cursor_up();
        a.cursor_up();
        a.cursor_up();
        assert_eq!(a.input, "first");
        // last line, not at end -> jump to end
        a.cursor_down();
        assert_eq!(a.input, "first");
        assert_eq!(a.input_cursor, "first".len());
        // at end -> recall next ("second"), cursor at end
        a.cursor_down();
        assert_eq!(a.input, "second");
        assert_eq!(a.input_cursor, "second".len());
        // recall next -> restored stash "Hello"
        a.cursor_down();
        assert_eq!(a.input, "Hello");
        assert_eq!(a.input_cursor, "Hello".len());
    }

    #[test]
    fn cursor_up_down_navigate_multiline() {
        let mut a = app();
        a.set_input("line1\nline2\nline3".to_string());
        // cursor at end (row 2). up -> row 1
        a.cursor_up();
        assert_eq!(a.cursor_row_col().0, 1);
        a.cursor_up();
        assert_eq!(a.cursor_row_col().0, 0);
        // up on first line (col>0) -> start of line
        a.cursor_up();
        assert_eq!(a.cursor_row_col(), (0, 0));
        // down -> row 1
        a.cursor_down();
        assert_eq!(a.cursor_row_col().0, 1);
        a.cursor_down();
        assert_eq!(a.cursor_row_col().0, 2);
    }

    #[test]
    fn history_recall_round_trip() {
        let mut a = app();
        a.set_input("first".to_string());
        a.history_nav.push("first".to_string());
        a.set_input("second".to_string());
        a.history_nav.push("second".to_string());
        a.clear_input();
        a.recall_prev();
        assert_eq!(a.input, "second");
        a.recall_prev();
        assert_eq!(a.input, "first");
        a.recall_next();
        assert_eq!(a.input, "second");
        a.recall_next();
        assert_eq!(a.input, "");
    }

    #[test]
    fn slash_clear_help_session_resume_new() {
        let mut a = app();
        push_turn(&mut a);
        assert!(a.slash_command("/clear"));
        assert!(a.turns.is_empty());

        a.slash_command("/help");
        assert_eq!(a.turns.len(), 1);
        assert!(matches!(a.turns[0].blocks[0], Block::Text(_)));

        assert!(a.slash_command("/nope"));
        assert!(matches!(a.turns.last().unwrap().blocks[0], Block::Error(_)));

        // /resume with no store pushes an error block, no picker.
        assert!(a.slash_command("/resume"));
        assert!(a.picker.is_none());
        assert!(matches!(a.turns.last().unwrap().blocks[0], Block::Error(_)));

        assert!(a.slash_command("/new"));
        assert!(a.turns.is_empty());
        assert!(a.session.path.is_none());

        assert!(a.slash_command("/quit"));
        assert!(a.should_quit);
    }

    #[test]
    fn verbose_toggles() {
        let mut a = app();
        assert!(!a.verbose);
        a.toggle_verbose();
        assert!(a.verbose);
    }

    #[test]
    fn footer_shows_model_and_thinking() {
        let a = App::new("openai/gpt-5.6-sol".to_string(), ThinkingLevel::XHigh, 0);
        let r: String = a
            .render_footer_right()
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert!(r.contains("gpt-5.6-sol"));
        assert!(r.contains("· xhigh"));
    }

    #[test]
    fn footer_hides_thinking_when_off() {
        let a = App::new("openai/gpt-4o".to_string(), ThinkingLevel::Off, 0);
        let r: String = a
            .render_footer_right()
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert_eq!(r, "openai/gpt-4o");
    }

    #[test]
    fn footer_shows_ctx_after_usage() {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::TurnEnd {
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage {
                input_tokens: 40_000,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        let r: String = a
            .render_footer_left(120)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert!(r.contains("ctx: 40k/200k"), "footer: {r}");
    }

    #[test]
    fn turns_from_events_round_trip() {
        let events = vec![
            SessionEvent::Message(user("hello")),
            SessionEvent::Message(assistant("hi there")),
            SessionEvent::Message(user("again")),
            SessionEvent::Message(assistant("yep")),
        ];
        let turns = turns_from_events(&events);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].prompt, "hello");
        assert_eq!(turns[1].prompt, "again");
        assert!(matches!(turns[0].blocks[0], Block::Text(_)));
    }

    #[test]
    fn turns_from_events_links_tool_results() {
        let messages = vec![
            user("run it"),
            Message {
                role: Role::Assistant,
                blocks: vec![
                    ContentBlock::Text { text: "ok".to_string() },
                    ContentBlock::ToolUse {
                        id: "t1".to_string(),
                        name: "exec".to_string(),
                        input: serde_json::json!({"cmd": "ls"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "file.txt".to_string(),
                    is_error: false,
                }],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "done".to_string() }],
            },
        ];
        let events: Vec<SessionEvent> = messages.into_iter().map(SessionEvent::Message).collect();
        let turns = turns_from_events(&events);
        assert_eq!(turns.len(), 1);
        let blocks = &turns[0].blocks;
        // Text, Tool, Text.
        assert!(matches!(blocks[0], Block::Text(_)));
        let Block::Tool(t) = &blocks[1] else {
            unreachable!()
        };
        assert_eq!(t.result.as_deref(), Some("file.txt"));
        assert!(t.done);
        assert!(matches!(blocks[2], Block::Text(_)));
    }

    #[test]
    fn turns_from_events_restores_timings() {
        let events = vec![
            SessionEvent::Message(user("run it")),
            SessionEvent::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"code": "return 1"}),
                }],
            }),
            SessionEvent::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "1".to_string(),
                    is_error: false,
                }],
            }),
            SessionEvent::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "done".to_string() }],
            }),
            SessionEvent::ToolTiming { id: "t1".into(), elapsed_ms: 7 },
            SessionEvent::TurnEnd {
                label: "plexus/gemini-3-flash · medium".into(),
                elapsed_ms: 2000,
                cost: 0.0,
                usage: Usage::default(),
            },
        ];
        let turns = turns_from_events(&events);
        assert_eq!(turns.len(), 1);
        let blocks = &turns[0].blocks;
        // Tool elapsed is restored from the ToolTiming event.
        let Block::Tool(t) = &blocks[0] else { unreachable!() };
        assert_eq!(t.elapsed, Some(Duration::from_millis(7)));
        // The trailing block is the restored turn-end marker.
        match blocks.last() {
            Some(Block::TurnEnd { label, elapsed }) => {
                assert_eq!(label, "plexus/gemini-3-flash · medium");
                assert_eq!(*elapsed, Duration::from_secs(2));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

    #[test]
    fn compact_count_formats() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(500), "500");
        assert_eq!(compact_count(119_000), "119k");
        assert_eq!(compact_count(200_000), "200k");
        assert_eq!(compact_count(2_000_000), "2M");
        assert_eq!(compact_count(9_700_000), "9.7M");
    }

    #[test]
    fn fmt_cost_two_decimals() {
        assert_eq!(fmt_cost(0.0), "$0.00");
        assert_eq!(fmt_cost(0.01), "$0.01");
        assert_eq!(fmt_cost(81.0), "$81.00");
        assert_eq!(fmt_cost(81.40), "$81.40");
        assert_eq!(fmt_cost(81.45), "$81.45");
    }

    #[test]
    fn abbreviate_path_progressive() {
        let home = dirs::home_dir().unwrap();
        let p = home.join("Dev/src/proj");
        assert_eq!(abbreviate_path(&p, 100), "~/Dev/src/proj");
        assert_eq!(abbreviate_path(&p, 10), "~/D/s/proj");
        assert_eq!(abbreviate_path(&p, 4), "proj");
        // A component starting with `~` keeps the tilde: `~sirn` -> `~s`.
        let p2 = home.join("Dev/~sirn/lofi");
        assert_eq!(abbreviate_path(&p2, 12), "~/D/~s/lofi");
    }

    #[test]
    fn footer_and_header_show_cost_and_usage() {
        let mut a = App::new(
            "plexus/deepseek-v4-flash".to_string(),
            ThinkingLevel::Off,
            200_000,
        );
        push_turn(&mut a);
        a.apply_event(AgentEvent::TurnEnd {
            elapsed_ms: 0,
            cost: 18.0,
            usage: Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        let footer: String = a
            .render_footer_left(120)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert!(footer.contains("↑1M ↓1M"), "footer: {footer}");
        assert!(footer.contains("$18.00"), "footer: {footer}");
        assert!(footer.contains("ctx: 2M/200k"), "footer: {footer}");
        let header: String = a
            .render_header_line(120)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert!(header.contains("lofi"), "header: {header}");
        // Cost lives in the footer now, not the header.
        assert!(!header.contains('$'), "header should not show cost: {header}");
    }

    /// With no model configured, submitting a prompt must not start a run;
    /// it re-surfaces the configuration hint on the prompt's turn instead.
    #[test]
    fn no_model_submit_surfaces_hint_without_running() {
        let mut a = app();
        a.no_models_hint = Some("set OPENAI_API_KEY".to_string());
        a.input = "hello".to_string();
        let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::empty(),
            KeyEventKind::Press,
        ));
        let mut run = None;
        handle_event(&ev, &mut a, None, &mut run);
        assert!(run.is_none(), "no run should be started without a model");
        assert!(a.run.is_none());
        let last = a.turns.last().unwrap();
        assert_eq!(last.prompt, "hello");
        assert!(
            last.blocks.iter().any(|b| matches!(b, Block::Error(m) if m == "set OPENAI_API_KEY")),
            "the hint should be attached to the turn"
        );
    }
}