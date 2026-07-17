//! Terminal UI: ratatui + crossterm event loop.
//!
//! Renders the conversation as a sequence of turns, each a user prompt
//! followed by a stream of blocks (assistant text, reasoning, tool calls,
//! errors) — the block model. Tool-result bodies are folded to
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
use std::path::{Path, PathBuf};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::execute;
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use lofi_core::session::store::{self, SessionEntry, SessionStore};
use lofi_types::{ContentBlock, Message, NativeToolRecord, Role, SessionEvent, SessionEventKind, ThinkingLevel, Usage};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
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
/// How long the "Copied to clipboard" badge stays on the footer rule.
const YANK_NOTIFY: Duration = Duration::from_secs(2);
/// Maximum height (content lines) the input box grows to before clipping.
const MAX_INPUT_LINES: usize = 8;
/// Window for a double `C-c` on an empty prompt to register as quit.
const QUIT_DOUBLE_PRESS: Duration = Duration::from_secs(2);
/// Fallback context-window ceiling for the status gauge when a model
/// reports no `context_window`. A reported non-zero value is always used as-is.
const DEFAULT_CTX_LIMIT: u64 = 200_000;

/// The slash commands offered by the autocomplete popover, in display
/// order. Kept in sync with [`App::slash_command`]. Each entry is
/// `(command, short description)`; the description is shown muted to the
/// right of the command in the popover.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/clear", "clear the transcript log"),
    ("/exit", "exit lofi"),
    ("/help", "show keybindings and commands"),
    ("/new", "start a fresh session"),
    ("/quit", "exit lofi"),
    ("/resume", "pick a past session to resume"),
    ("/session", "show session info"),
    ("/tree", "roll back to a past turn"),
    ("/verbose", "toggle tool detail"),
];

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

/// A transient provider error is being retried with exponential backoff.
/// Shown in the status line as `⟳ retry 1/3 in 2.0s: <error>`.
#[derive(Debug, Clone)]
struct RetryState {
    attempt: u32,
    max_attempts: u32,
    /// Instant the backoff ends; the status line counts down to it.
    deadline: Instant,
    error: String,
}

impl RetryState {
    /// Time remaining until the backoff elapses.
    fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }
}

/// One block in a turn's response stream.
#[derive(Debug, Clone)]
enum Block {
    Text(String),
    Thinking(ThinkingBlock),
    Tool(ToolCall),
    Error(String),
    /// Turn-end rule: `<label> done in Ns` followed by a dash
    /// fill, appended when a run finishes.
    TurnEnd { label: String, elapsed: Duration },
    /// Turn-failed rule: `<label> failed in Ns · <error>` in the error
    /// tint, appended when a run ends in a non-retryable error or is
    /// cancelled. The turn's partial messages precede it; the marker is the
    /// leaf of the failed branch.
    TurnFailed { label: String, elapsed: Duration, error: String },
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

/// Slash-command autocomplete popover state. Active while the input is a
/// prefix of one or more entries in [`SLASH_COMMANDS`] (e.g. `/`, `/tr`);
/// dismissed by `Esc`, a non-matching edit, or selecting a candidate with
/// `Tab`. `↑/↓` or `Ctrl+N`/`Ctrl+P` move the selection; `j`/`k`/`q` are
/// not intercepted so they stay printable (the popover floats over a text
/// input, unlike a [`Modal`]).
#[derive(Debug, Clone)]
struct SlashComplete {
    /// Indices into [`SLASH_COMMANDS`] of the matching candidates, in the
    /// order they appear there.
    candidates: Vec<usize>,
    selected: usize,
}

/// State for the '/tree' branch-picker overlay. Lists the branch points in
/// the active session — every user prompt ("edit and resend") and every
/// `turn_end` ("continue from after this turn") — so the user can roll the
/// transcript back to any point, Pi-style. Confirming an entry rebuilds the
/// visible turns and history from the rolled-back active path, sets the
/// `branch_hint` so the next run chains off the chosen point, and (for
/// user-prompt entries) prefills the input with the original prompt text.
/// Items are chronological; `selected` starts at the last entry.
#[derive(Debug, Clone)]
struct TreePickerState {
    entries: Vec<TreeEntry>,
    selected: usize,
}

/// A list-style modal overlay (the `/resume` and `/tree` pickers). The
/// shared key dispatch — up/down (`↑/↓` or `j`/`k`), `Ctrl+N`/`Ctrl+P`,
/// `Enter` to confirm, `Esc`/`q` to cancel — lives in one place
/// ([`App::handle_modal_key`]); each modal implements `confirm` for its own
/// side effects via the per-slot `_confirm_inner` methods (they need
/// `&mut App`, which a trait method can't borrow cleanly while the modal is
/// also borrowed). Modals are full overlays with a header.
trait Modal {
    fn len(&self) -> usize;
    fn selected(&self) -> usize;
    fn set_selected(&mut self, n: usize);
}

/// A list-style popover (the slash-command autocomplete). Unlike a
/// [`Modal`], a popover floats over a text input, so its keymap excludes
/// `j`/`k`/`q` (those must stay printable) and has no header. Navigation is
/// `↑/↓` or `Ctrl+N`/`Ctrl+P`; `Tab` accepts; `Esc` dismisses. Dispatch
/// lives in [`App::handle_popover_key`].
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

/// One row in the '/tree' picker. `branch_point` is the event id the next
/// run chains off (becomes the new turn's parent); `label` is the node text
/// (`user: ...` or `agent: ...`); `prefix` is the ASCII tree art (`|- `,
/// ``- `, `|  `, `   `); `prefill` is loaded into the input box on confirm
/// (empty for `turn_end` entries, since those continue rather than re-edit);
/// `is_active` marks nodes on the active path — the conversation currently
/// displayed in the transcript (root → active leaf). After a revert the
/// active leaf is the branch point (via `branch_hint`), so rolled-back turns
/// appear as unhighlighted branches; after continuing, the new turns join
/// the active path and are highlighted too.
#[derive(Debug, Clone)]
struct TreeEntry {
    branch_point: String,
    label: String,
    prefix: String,
    prefill: String,
    is_active: bool,
}

/// A bounded FIFO cache of rendered frozen turns, keyed by turn index.
/// Only turns near the viewport are retained; the rest are re-rendered on
/// demand from `turns`. Heights for *all* frozen turns live in
/// `frozen_heights` (tiny) so the viewport can be located without fetching
/// lines, keeping the heavy styled-line copy bounded by [`FROZEN_CACHE_CAP`]
/// turns regardless of session length.
struct FrozenCache {
    order: VecDeque<usize>,
    map: HashMap<usize, Vec<view::RenderLine>>,
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
        while self.order.len() > Self::CAP {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
}

/// The TUI's mutable state.
/// Mouse selection in the log, in select-line + char-index space (absolute
/// indices into `log_lines`).
struct Selection {
    start: (usize, usize),
    end: (usize, usize),
}

/// Editor-style modal focus.
///
/// `Input` is the default prompt typing mode. `Navigate` and `Select` move a
/// cursor over the transcript instead: `Navigate` scrolls, `Select` (entered
/// with `v`) extends a selection from an anchor. Tab switches modes; `i`
/// returns to `Input`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Input,
    Navigate,
    Select,
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
    /// Most recent turn's usage, for the context-window gauge (input
    /// + output + cache read/write of the latest round = current fill).
    status_usage: Option<Usage>,
    total_in: u64,
    total_out: u64,
    /// Accumulated USD cost across turns (engine-computed, fed by
    /// `RoundUsage` per round and folded by `TurnEnd`).
    cost: f64,
    /// Cumulative USD cost within the current turn, refreshed by each
    /// `RoundUsage` event. Folded into `cost` at `TurnEnd` and reset, so
    /// the footer can show a live running cost during a multi-round turn
    /// without double-counting on the final `TurnEnd`.
    turn_cost: f64,
    /// Whether the current turn has emitted any `RoundUsage` events. When
    /// true, `TurnEnd` skips re-accumulating `total_in`/`total_out`/
    /// `status_usage` (already applied per round) and only folds `turn_cost`;
    /// when false (the resume path, which has no `RoundUsage` events),
    /// `TurnEnd` applies its bundled totals as before.
    turn_has_round_usage: bool,
    /// Explicit branch point for the next run, set by a UI gesture (e.g.
    /// resuming from a selected entry in the tree picker). Taken and cleared
    /// when the run starts so a single gesture applies to a single turn;
    /// `None` means append to the file's active leaf (linear continuation).
    branch_hint: Option<String>,
    ctx_limit: u64,
    /// Spinner frame while a run is active; None when idle.
    run: Option<usize>,
    /// Wall-clock start of the active run; drives the live `working for Ns`
    /// indicator only — the authoritative turn duration comes from the
    /// engine's `TurnEnd` event.
    run_start: Option<Instant>,
    /// In-flight retry: the attempt number (1-indexed), the backoff deadline,
    /// and the triggering error, shown in the status line while the agent
    /// waits out an exponential backoff before retrying a transient failure.
    retry: Option<RetryState>,
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
    /// '/tree' overlay state, when open. See [`TreePickerState`].
    tree_picker: Option<TreePickerState>,
    /// Slash-command autocomplete popover, active while the input is a
    /// prefix of a known command.
    slash_complete: Option<SlashComplete>,
    /// Hint shown in the log when no model is configured; `None` in normal runs.
    no_models_hint: Option<String>,
    theme: Theme,
    /// Kill ring for emacs-style C-k / C-u / C-w / M-d, yanked back with C-y.
    kill_ring: String,
    /// True when the previous command was `C-k` so a consecutive `C-k`
    /// appends to the kill ring instead of replacing it.
    last_kill_was_kill: bool,
    /// Timestamp of the last `C-c` on an empty prompt with no run active.
    /// A second `C-c` within [`QUIT_DOUBLE_PRESS`] quits, mirroring shells.
    ctrl_c_at: Option<Instant>,
    /// Screen rect of the log viewport, stashed at render time for hit-testing
    /// mouse scroll / selection.
    log_rect: Rect,
    /// The prompt (input) area rect from the last render, so overlays like
    /// the slash-complete popover can anchor above the cursor.
    input_rect: Rect,
    /// Plain text of each *visible* log line (the viewport window only),
    /// stashed at render time so mouse selection can map screen coords to
    /// text. Window-relative: index 0 is the top visible line.
    log_lines: Vec<String>,
    /// Selectable content char range of each *visible* log line, parallel to
    /// [`log_lines`]. Selection (highlight and copy) is clamped to this range
    /// so it covers content only — never the decorative gutter/rails in front
    /// or the background-padding tail at the end — while preserving content's
    /// own leading spaces (indentation).
    log_content: Vec<(usize, usize)>,
    /// Absolute index of the top visible log line (`scroll` offset).
    log_off: usize,
    /// Top visible select row of the prompt input when it overflows its
    /// capped height ([`MAX_INPUT_LINES`]). Synced each frame to keep the
    /// cursor on screen.
    input_scroll: usize,
    /// Active mouse selection, if any.
    sel: Option<Selection>,
    /// Current modal focus.
    mode: Mode,
    /// When the yank-to-clipboard badge was last triggered; shown on the
    /// footer rule's left for a short window after a yank.
    yank_notify: Option<Instant>,
    /// Absolute index of the transcript line under the Navigate/Select cursor.
    nav_cursor: usize,
    /// Character column (absolute char index in the cursor line) under the
    /// Navigate/Select cursor.
    nav_col: usize,
    /// (`line`, `col`) where `v` was pressed; the selection extends from here
    /// to (`nav_cursor`, `nav_col`). Only meaningful in [`Mode::Select`].
    select_anchor: (usize, usize),
    /// Total transcript line count (frozen + last turn + separators), stashed
    /// at render time so the Navigate cursor can be clamped between events.
    log_total: usize,
    /// Line count of the live (last) turn, stashed at render time so turn
    /// boundaries can be computed between events for `[`/`]` jumps.
    last_turn_height: usize,
    /// Log viewport height at last render, for cursor-follow scrolling.
    log_view_h: usize,
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
            ctx_limit: if ctx_limit > 0 { ctx_limit } else { DEFAULT_CTX_LIMIT },
            cost: 0.0,
            turn_cost: 0.0,
            turn_has_round_usage: false,
            branch_hint: None,
            total_in: 0,
            total_out: 0,
            run: None,
            run_start: None,
            retry: None,
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
            tree_picker: None,
            slash_complete: None,
            no_models_hint: None,
            theme: Theme::default(),
            kill_ring: String::new(),
            last_kill_was_kill: false,
            ctrl_c_at: None,
            log_rect: Rect::default(),
            input_rect: Rect::default(),
            log_lines: Vec::new(),
            log_content: Vec::new(),
            log_off: 0,
            input_scroll: 0,
            sel: None,
            mode: Mode::Input,
            yank_notify: None,
            nav_cursor: 0,
            nav_col: 0,
            select_anchor: (0, 0),
            log_total: 0,
            last_turn_height: 0,
            log_view_h: 0,
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

    /// Set the explicit branch point for the next run. A UI gesture (e.g.
    /// resuming from a selected entry in the tree picker) calls this with the
    /// target entry's id; the next run branches off that id as a sibling of
    /// its existing children instead of appending to the active leaf. The
    /// hint is consumed by the run launcher, so a single gesture applies to a
    /// single turn and a subsequent run without a gesture continues
    /// linearly.
    fn branch_from(&mut self, id: String) {
        self.branch_hint = Some(id);
    }

    /// Number of select rows the input occupies after soft-wrapping to the
    /// prompt width, capped at [`MAX_INPUT_LINES`]. `width` is the full
    /// terminal width; 2 cells are reserved for the `❯ `/`  ` prefix.
    fn input_lines(&self, width: usize) -> usize {
        let content_w = width.saturating_sub(2);
        self.input_select_rows(content_w)
            .len()
            .min(MAX_INPUT_LINES)
    }

    /// Keep [`input_scroll`] within bounds and clamp it so the cursor's
    /// select row stays inside the visible `[scroll, scroll + vis_h)` window.
    /// Called each frame before the prompt is rendered.
    fn sync_input_scroll(&mut self, content_w: usize, vis_h: usize) {
        let total = self.input_select_rows(content_w).len();
        let max_top = total.saturating_sub(vis_h);
        let mut scroll = self.input_scroll.min(max_top);
        let vrow = self.input_cursor_pos(content_w).0;
        if vrow < scroll {
            scroll = vrow;
        } else if vis_h > 0 && vrow >= scroll + vis_h {
            scroll = vrow.saturating_sub(vis_h) + 1;
        }
        self.input_scroll = scroll.min(max_top);
    }

    /// Soft-wrap the input to `content_w` display cells, breaking on
    /// wide-char boundaries (not word boundaries, so the cursor maps
    /// predictably). Hard `\n` splits always start a new row. Empty input
    /// yields a single empty row so the prompt always renders one line.
    fn input_select_rows(&self, content_w: usize) -> Vec<String> {
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

    /// Map the cursor to a (select row, x-within-content) pair for
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
        // TurnStart is the turn boundary: push a fresh turn. Unlike the other
        // arms, it does not assume a current turn exists — it creates one.
        if let AgentEvent::TurnStart { prompt } = ev {
            self.push_turn(Turn {
                prompt,
                blocks: Vec::new(),
            });
            // Reset the per-turn accumulators: the live stream feeds these
            // via `RoundUsage` events, and `TurnEnd` folds them once.
            self.turn_cost = 0.0;
            self.turn_has_round_usage = false;
            return;
        }
        // Status-only events the turn builder doesn't own.
        match ev {
            AgentEvent::RetryStart { attempt, max_attempts, delay_ms, error } => {
                self.retry = Some(RetryState {
                    attempt,
                    max_attempts,
                    deadline: Instant::now() + Duration::from_millis(delay_ms),
                    error,
                });
                return;
            }
            AgentEvent::RetryEnd { success, .. } => {
                self.retry = None;
                if !success {
                    // The retry budget was exhausted; the triggering error
                    // surfaces via the subsequent `Error` event from the
                    // engine, so no block is pushed here.
                }
                return;
            }
            AgentEvent::TurnCommitted { byte_start, byte_end } => {
                // The just-finished turn is now durably in the transcript
                // file over this byte range. Record it so the turn becomes
                // file-backed when the next prompt freezes it.
                if let Some(r) = self.turn_byte_ranges.last_mut() {
                    *r = Some((byte_start, byte_end));
                }
                return;
            }
            AgentEvent::RoundUsage { cost, usage } => {
                // Per-round refresh of the context gauge and cost counter.
                // `cost` is the turn's cumulative cost so far; track it in
                // `turn_cost` (folded into `cost` at `TurnEnd`) so the
                // footer can show a live running total. Tokens accumulate
                // directly into the session totals; `TurnEnd` skips
                // re-adding them when `turn_has_round_usage` is set.
                self.turn_cost = cost;
                self.turn_has_round_usage = true;
                self.total_in += usage.input_tokens;
                self.total_out += usage.output_tokens;
                self.status_usage = Some(usage);
                return;
            }
            AgentEvent::TurnEnd { cost, usage, .. } => {
                // Totals are owned by the App, not the turn builder. On the
                // live path `RoundUsage` already applied this turn's tokens
                // and `turn_cost` holds its cumulative cost; fold `turn_cost`
                // and skip the bundled totals. On the resume path (no
                // `RoundUsage` events) apply the bundled totals as before.
                if self.turn_has_round_usage {
                    self.cost += self.turn_cost;
                } else {
                    self.cost += cost;
                    self.total_in += usage.input_tokens;
                    self.total_out += usage.output_tokens;
                    self.status_usage = Some(usage);
                }
                self.turn_cost = 0.0;
                self.turn_has_round_usage = false;
            }
            AgentEvent::TurnFailed { cost, usage, .. } => {
                // A failed turn's consumed tokens count honestly. Same
                // fold logic as `TurnEnd`: the live path already applied
                // per-round tokens via `RoundUsage` and `turn_cost` holds
                // the cumulative cost; the resume path applies the bundled
                // totals. `status_usage` updates either way so the gauge
                // reflects the failed turn's last round.
                if self.turn_has_round_usage {
                    self.cost += self.turn_cost;
                    self.status_usage = Some(usage);
                } else {
                    self.cost += cost;
                    self.total_in += usage.input_tokens;
                    self.total_out += usage.output_tokens;
                    self.status_usage = Some(usage);
                }
                self.turn_cost = 0.0;
                self.turn_has_round_usage = false;
            }
            _ => {}
        }
        // Everything else (and the block-building part of `TurnEnd`) goes
        // through the shared turn builder, so live and resume share one path.
        apply_event_to_turns(&mut self.turns, ev);
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
        self.retry = None;
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
        turns_from_session_events(&events)
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
            self.frozen_render.insert(idx, lines);
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
            self.frozen_heights.push(lines.len());
            self.frozen_render.insert(idx, lines);
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

    /// The active retry state, if the agent is waiting out a backoff.
    #[must_use]
    fn retry_state(&self) -> Option<&RetryState> {
        self.retry.as_ref()
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

    /// Insert a (possibly multi-line) string at the cursor in one operation.
    /// Used by bracketed-paste so a paste of any size lands atomically
    /// instead of character-by-character. Line endings are normalized to
    /// `\n` so a paste carrying `\r\n` or bare `\r` (which varies by terminal)
    /// renders and submits as real newlines, not a single garbled line.
    fn insert_str(&mut self, s: &str) {
        let normalized: String = s.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.input_cursor, &normalized);
        self.input_cursor += normalized.len();
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
        // End of buffer with no trailing newline: nothing to kill. Emacs
        // leaves the buffer untouched here, and the naive `cursor + 1` would
        // slice one past the end and panic.
        if after.is_empty() {
            return;
        }
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

    /// Enter Navigate mode, parking the viewport where it is (stop following
    /// new output) and placing the cursor on the last visible line.
    fn enter_nav(&mut self) {
        self.mode = Mode::Navigate;
        self.sel = None;
        self.pinned = false;
        self.top_line = self.log_off;
        let last = self
            .log_off
            .saturating_add(self.log_view_h)
            .saturating_sub(1);
        self.nav_cursor = last.min(self.log_total.saturating_sub(1));
        // Park the column at the content start; h/l snap it into range.
        self.nav_col = 0;
    }

    /// Return to Input mode, dropping any selection.
    fn enter_input(&mut self) {
        self.mode = Mode::Input;
        self.sel = None;
    }

    /// Snap the viewport to the bottom (latest) transcript line. Used when
    /// leaving Navigate/Select so the user lands on the newest output.
    fn pin_to_latest(&mut self) {
        self.pinned = true;
        self.top_line = self.last_base;
    }

    /// From Navigate, start a charwise selection at the cursor.
    fn enter_select(&mut self) {
        self.mode = Mode::Select;
        self.select_anchor = (self.nav_cursor, self.nav_col);
        self.sel = Some(self.select_sel());
    }

    /// Charwise selection from the anchor to the cursor (inclusive of both
    /// endpoints, vim-style). The max-end column is shifted by +1 so the
    /// content-aware clamp treats it as an exclusive bound for rendering and
    /// [`selection_text`].
    fn select_sel(&self) -> Selection {
        let a = self.select_anchor;
        let c = (self.nav_cursor, self.nav_col);
        let (s, mut e) = if a <= c { (a, c) } else { (c, a) };
        e.1 = e.1.saturating_add(1);
        Selection { start: s, end: e }
    }

    /// Content char range [cstart, cend] of the cursor line (absolute char
    /// indices), from the last render's visible window. Valid because
    /// `nav_show_cursor` keeps the cursor on screen between events.
    fn cursor_content_range(&self) -> (usize, usize) {
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        self.log_content.get(rel).copied().unwrap_or((0, 0))
    }

    /// Move the cursor column by `delta` chars, clamped to the cursor line's
    /// content. In Select the selection follows.
    fn nav_col_delta(&mut self, delta: i32) {
        let (cstart, cend) = self.cursor_content_range();
        let raw = if delta > 0 {
            self.nav_col.saturating_add(1)
        } else {
            self.nav_col.saturating_sub(1)
        };
        self.nav_col = raw.clamp(cstart, cend);
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
    }

    /// Set the cursor column to `target`, clamped to the cursor line's
    /// content. In Select the selection follows.
    fn nav_set_col(&mut self, target: usize) {
        let (cstart, cend) = self.cursor_content_range();
        self.nav_col = if cend > cstart {
            target.clamp(cstart, cend - 1)
        } else {
            cstart
        };
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
    }

    /// Column of the first non-blank content character on the cursor line
    /// (vim `^`). Falls back to the content start when all blank.
    fn first_nonblank_col(&self) -> usize {
        let (cstart, cend) = self.cursor_content_range();
        if cend <= cstart {
            return cstart;
        }
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let Some(s) = self.log_lines.get(rel) else {
            return cstart;
        };
        for (i, c) in s.chars().enumerate() {
            if i >= cend {
                break;
            }
            if i >= cstart && !c.is_whitespace() {
                return i;
            }
        }
        cstart
    }

    /// Vim word/WORD motion target column for the cursor line.
    fn nav_word_target(&self, motion: WordMotion) -> usize {
        let (cstart, cend) = self.cursor_content_range();
        if cend <= cstart {
            return cstart;
        }
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let Some(s) = self.log_lines.get(rel) else {
            return cstart;
        };
        let chars: Vec<char> = s.chars().collect();
        let content = &chars[cstart..cend];
        let n = content.len();
        let p = self.nav_col.clamp(cstart, cend - 1) - cstart;
        let class = |c: char, big: bool| -> u8 {
            if c.is_whitespace() {
                0
            } else if big || c.is_alphanumeric() || c == '_' {
                1
            } else {
                2
            }
        };
        let np = match motion {
            WordMotion::NextStart { big } => {
                let mut i = p;
                let cur = class(content[i], big);
                if cur != 0 {
                    while i < n && class(content[i], big) == cur {
                        i += 1;
                    }
                }
                while i < n && class(content[i], big) == 0 {
                    i += 1;
                }
                i.min(n.saturating_sub(1))
            }
            WordMotion::PrevStart { big } => {
                let mut i = p.saturating_sub(1);
                while i > 0 && class(content[i], big) == 0 {
                    i -= 1;
                }
                let cur = class(content[i], big);
                while i > 0 && class(content[i - 1], big) == cur {
                    i -= 1;
                }
                i
            }
            WordMotion::NextEnd { big } => {
                let mut i = (p + 1).min(n.saturating_sub(1));
                while i < n && class(content[i], big) == 0 {
                    i += 1;
                }
                if i >= n {
                    n.saturating_sub(1)
                } else {
                    let cur = class(content[i], big);
                    while i + 1 < n && class(content[i + 1], big) == cur {
                        i += 1;
                    }
                    i
                }
            }
        };
        cstart + np
    }

    fn nav_word_motion(&mut self, motion: WordMotion) {
        let target = self.nav_word_target(motion);
        self.nav_set_col(target);
    }

    /// Move the Navigate/Select cursor by `delta` lines, clamping to the
    /// transcript. In Select the selection follows; the viewport scrolls only
    /// when the cursor leaves it.
    fn nav_move(&mut self, delta: i32) {
        let max = self.log_total.saturating_sub(1);
        let step = delta.unsigned_abs() as usize;
        self.nav_cursor = if delta > 0 {
            self.nav_cursor.saturating_add(step).min(max)
        } else {
            self.nav_cursor.saturating_sub(step)
        };
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    fn nav_top(&mut self) {
        self.nav_cursor = 0;
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    fn nav_bottom(&mut self) {
        self.nav_cursor = self.log_total.saturating_sub(1);
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    /// Adjust [`top_line`] so the cursor is visible. Navigate never auto-pins
    /// (it doesn't follow new output); only the minimal scroll needed to keep
    /// the cursor on screen is applied.
    fn nav_show_cursor(&mut self) {
        let h = self.log_view_h;
        let total = self.log_total;
        if h == 0 || total == 0 {
            return;
        }
        let base = total.saturating_sub(h);
        let cur = self.nav_cursor;
        let off = self.log_off;
        let new_top = if cur < off {
            cur
        } else if cur >= off + h {
            cur + 1 - h
        } else {
            off
        };
        self.pinned = false;
        self.top_line = new_top.min(base);
    }

    /// First transcript line of turn `i` (0-based). Turns are laid out as
    /// `turn0, blank, turn1, blank, ...`, so turn `i` starts at the sum of all
    /// preceding turn heights plus one blank separator per preceding turn.
    fn turn_start_line(&self, i: usize) -> usize {
        let n = self.turns.len();
        if i == 0 || n == 0 {
            return 0;
        }
        let mut start = 0usize;
        for j in 0..i.min(n) {
            let h = if j + 1 < n {
                self.frozen_heights.get(j).copied().unwrap_or(0)
            } else {
                self.last_turn_height
            };
            start += h + 1;
        }
        start
    }

    /// Jump the cursor to the start of the next (`dir > 0`) or previous turn.
    /// In Select the selection extends; otherwise it is cleared.
    fn nav_jump_turn(&mut self, dir: i32) {
        let n = self.turns.len();
        if n == 0 {
            return;
        }
        let mut cur = 0;
        for i in 0..n {
            if self.nav_cursor < self.turn_start_line(i) {
                break;
            }
            cur = i;
        }
        let target_turn = if dir > 0 {
            (cur + 1).min(n - 1)
        } else {
            cur.saturating_sub(1)
        };
        let max = self.log_total.saturating_sub(1);
        self.nav_cursor = self.turn_start_line(target_turn).min(max);
        self.nav_col = 0;
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        } else {
            self.sel = None;
        }
        self.nav_show_cursor();
    }

    /// Move by one viewport page. In [`Mode::Input`] this scrolls the
    /// transcript (unpinning from the bottom on `PgUp`); in
    /// [`Mode::Navigate`]/[`Mode::Select`] the cursor moves a page and the
    /// viewport follows.
    fn page_up(&mut self) {
        let h = self.log_view_h;
        if h == 0 {
            return;
        }
        let step: i32 = h.try_into().unwrap_or(i32::MAX);
        match self.mode {
            Mode::Input => {
                self.top_line = self.top_line.saturating_sub(h);
                self.pinned = false;
            }
            Mode::Navigate | Mode::Select => self.nav_move(-step)
        }
    }

    fn page_down(&mut self) {
        let h = self.log_view_h;
        if h == 0 {
            return;
        }
        let step: i32 = h.try_into().unwrap_or(i32::MAX);
        match self.mode {
            Mode::Input => {
                let base = self.last_base;
                let new = self.top_line.saturating_add(h);
                if new >= base {
                    self.pinned = true;
                } else {
                    self.top_line = new;
                    self.pinned = false;
                }
            }
            Mode::Navigate | Mode::Select => self.nav_move(step),
        }
    }

    /// Copy the current selection to the system clipboard via OSC 52.
    fn yank_selection(&mut self) {
        if let Some(text) = self.selection_text() {
            Self::osc52(&text);
            self.yank_notify = Some(Instant::now());
        }
    }

    /// Yank the cursor line's content (decoration excluded) to the clipboard.
    /// Used by Navigate's `y`.
    fn yank_line(&mut self) {
        if let Some(text) = self.current_line_text() {
            Self::osc52(&text);
            self.yank_notify = Some(Instant::now());
        }
    }

    fn osc52(text: &str) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        let _ = write!(io::stdout(), "\x1b]52;c;{b64}\x07");
        let _ = io::stdout().flush();
    }

    /// Content slice of the cursor line (char range `[cstart, cend)` mapped to
    /// bytes), excluding the decorative gutter and trailing padding.
    fn current_line_text(&self) -> Option<String> {
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let s = self.log_lines.get(rel)?;
        let n = s.chars().count();
        let (cstart, cend) = self.log_content.get(rel).copied().unwrap_or((0, n));
        let cstart = cstart.min(n);
        let cend = cend.min(n);
        if cstart >= cend {
            return None;
        }
        let b0 = s.char_indices().nth(cstart).map_or(s.len(), |(b, _)| b);
        let b1 = s.char_indices().nth(cend).map_or(s.len(), |(b, _)| b);
        Some(s[b0..b1].to_string())
    }

    /// Footer mode tag shown at the far left of the status line.
    pub(crate) fn mode_badge(&self) -> (&'static str, Color) {
        let t = self.theme;
        match self.mode {
            Mode::Input => ("INPUT", t.muted),
            Mode::Navigate => ("NAV", t.primary),
            Mode::Select => ("SELECT", t.warn),
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
        self.slash_complete = None;
    }

    fn toggle_verbose(&mut self) {
        self.verbose = !self.verbose;
        // Folded tool bodies are baked into the frozen-render cache at freeze
        // time, so a toggle must invalidate it — otherwise only the live
        // (last) turn would react and earlier turns would keep the preview.
        // The state itself surfaces as the `[VERBOSE]` tag on the rule line
        // rather than a chat turn, so toggling stays out of the transcript.
        self.bump_render_epoch();
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

    /// Viewport offset that will be used at the next render: `base` when
    /// pinned, otherwise `top_line` clamped to `base`.
    fn view_off(&self) -> usize {
        let base = self.last_base;
        if self.pinned { base } else { self.top_line.min(base) }
    }

    /// Mouse-wheel scroll: enter Navigate and move the viewport, clamping the
    /// cursor to the near edge when it leaves the viewport. Scrolling up parks
    /// the cursor on the bottom edge (it falls below as older lines enter);
    /// scrolling down parks it on the top edge. A no-op scroll (already at the
    /// boundary) leaves the mode untouched.
    fn scroll_nav(&mut self, delta: i32) {
        let before = self.view_off();
        self.scroll_by(delta);
        let after = self.view_off();
        if after == before {
            return;
        }
        self.mode = Mode::Navigate;
        self.sel = None;
        let last = after
            .saturating_add(self.log_view_h)
            .saturating_sub(1)
            .min(self.log_total.saturating_sub(1));
        if self.nav_cursor < after {
            self.nav_cursor = after;
        } else if self.nav_cursor > last {
            self.nav_cursor = last;
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
        let content = &self.log_content;
        let off = self.log_off;
        let vis_len = lines.len();
        if vis_len == 0 || el < off || sl >= off + vis_len {
            return None;
        }
        let lo = sl.max(off) - off;
        let hi = el.min(off + vis_len - 1) - off;
        // Selection is content-aware: each line's char range is clamped to its
        // content bounds, so the decorative gutter indent (leading) and the
        // background-padding tail (trailing) are never copied. Cell-level
        // selection with the gutter/padding intact is left to the terminal's
        // native copy.
        let mut out = String::new();
        for rel in lo..=hi {
            let li_abs = off + rel;
            let s = &lines[rel];
            let (cstart, cend) = content.get(rel).copied().unwrap_or((0, s.chars().count()));
            let cs = if li_abs == sl { sc } else { 0 };
            let ce = if li_abs == el { ec } else { s.chars().count() };
            let cs = cs.clamp(cstart, cend);
            let ce = ce.clamp(cstart, cend);
            let chars: Vec<(usize, char)> = s.char_indices().collect();
            let b0 = if cs == 0 || cs >= ce {
                0
            } else {
                chars[cs - 1].0 + chars[cs - 1].1.len_utf8()
            };
            let b1 = if ce == 0 || cs >= ce {
                0
            } else {
                chars[ce - 1].0 + chars[ce - 1].1.len_utf8()
            };
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
            "/tree" => {
                self.open_tree_picker();
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

    /// Recompute the slash-command autocomplete popover from the current
    /// input. The popover is active while the input is a non-empty prefix
    /// of one or more [`SLASH_COMMANDS`] entries (e.g. `/`, `/tr`). A bare
    /// `/` matches everything; once the full command is typed exactly, the
    /// popover dismisses (nothing left to complete). Preserves the selected
    /// candidate when it's still in the new match set.
    fn refresh_slash_complete(&mut self) {
        let input = self.input.as_str();
        if !input.starts_with('/') || input.is_empty() {
            self.slash_complete = None;
            return;
        }
        // Don't offer completion once the user has typed a full command plus
        // trailing text (e.g. `/help foo`) — there's nothing to complete.
        let candidates: Vec<usize> = SLASH_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (cmd, _))| cmd.starts_with(input))
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() || (candidates.len() == 1 && SLASH_COMMANDS[candidates[0]].0 == input) {
            self.slash_complete = None;
            return;
        }
        // Preserve the selection if the previously-selected command is still
        // a candidate; otherwise reset to the first match.
        let prev = self.slash_complete.as_ref().and_then(|sc| {
            sc.candidates
                .get(sc.selected)
                .and_then(|&idx| candidates.iter().position(|&c| c == idx))
        });
        let selected = prev.unwrap_or(0);
        self.slash_complete = Some(SlashComplete { candidates, selected });
    }

    /// Accept the selected autocomplete candidate: replace the input with
    /// the command, position the cursor at the end, and dismiss the popover.
    fn slash_complete_accept(&mut self) {
        if let Some(sc) = self.slash_complete.take() {
            if let Some(&idx) = sc.candidates.get(sc.selected) {
                self.input = SLASH_COMMANDS[idx].0.to_string();
                self.input_cursor = self.input.chars().count();
            }
        }
        self.slash_complete = None;
    }

    fn slash_complete_up(&mut self) {
        if let Some(sc) = self.slash_complete.as_mut() {
            if sc.selected > 0 {
                sc.selected -= 1;
            }
        }
    }

    fn slash_complete_down(&mut self) {
        if let Some(sc) = self.slash_complete.as_mut() {
            if sc.selected + 1 < sc.candidates.len() {
                sc.selected += 1;
            }
        }
    }

    fn push_help(&mut self) {
        let help = "Keys\n  Enter        send  ·  Alt+Enter / Ctrl+J  newline\n  ↑ / ↓        move line, recall at edge  ·  Ctrl+↑/↓  move across lines\n  PgUp/PgDn    scroll a page (Input) · move cursor a page (Nav/Select)\n  Tab          switch mode: Input ↔ Navigate / back from Select\n  Esc          clear input\n  Ctrl+C       Input: cancel run · clear · 2× quit  ·  Nav/Select: back to Input + latest  ·  Ctrl+D  del-char / quit on empty\nNavigate      Tab to enter · j/k or ↑/↓ scroll · h/l or ←/→ move col · 0/^/$ · w/b/e · g/G top/bottom · [ ] jump turns · v select · y yank line · i back\nSelect        move extends selection · y or Enter yank → Input · Tab or Esc back\nCommands\n  /help        this help  ·  /clear  clear log\n  /new         start a fresh session  ·  /resume  pick a past session\n  /tree        roll back to a past turn (edit + resend, or continue)\n  /session     show session info  ·  /verbose  toggle tool detail\n  /quit        exit\nSlash commands autocomplete: type / then ↑/↓ and Tab to complete";
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

    /// Load the selected session into the transcript and close the picker.
    fn picker_confirm_inner(&mut self, picker: PickerState) {
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
                // Replay the durable event log through `apply_event` so the
                // resume path and the live path share one builder. Totals
                // (cost/usage) are restored by the replayed `TurnEnd` events,
                // not by a separate accumulator — reset them first.
                self.turns = Vec::new();
                self.turn_byte_ranges = Vec::new();
                self.cost = 0.0;
                self.total_in = 0;
                self.total_out = 0;
                self.status_usage = None;
                for ev in replay_session_events(&events) {
                    self.apply_event(ev);
                }
                self.turn_byte_ranges =
                    turn_byte_ranges_from_events(&events, &offsets, file_size);
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

    /// '/tree': open the branch-picker overlay over the active session's
    /// event log. Lists every user-prompt event (the natural branch points)
    /// with its preview. Confirmed entry feeds the prompt text back into the
    /// input (for editing) and sets the branch hint so the next run starts as
    /// a sibling of that prompt rather than appending to the active leaf.
    fn open_tree_picker(&mut self) {
        let Some(path) = &self.session.path else {
            self.push_turn(Turn {
                prompt: "/tree".to_string(),
                blocks: vec![Block::Error("no session file (ephemeral or --no-session)".into())],
            });
            return;
        };
        // Lightweight index scan (id + parent_id + kind only — no
        // ContentBlock deserialization) so /tree stays fast on large
        // sessions. Labels are loaded on demand by offset.
        let indices = match store::load_index(path) {
            Ok((_meta, indices, _size)) => indices,
            Err(e) => {
                self.push_turn(Turn {
                    prompt: "/tree".to_string(),
                    blocks: vec![Block::Error(format!("load session for /tree: {e}"))],
                });
                return;
            }
        };
        let entries = build_tree_entries(&indices, self.branch_hint.as_deref(), path);
        if entries.is_empty() {
            self.push_turn(Turn {
                prompt: "/tree".to_string(),
                blocks: vec![Block::Text("no branch points in this session yet".into())],
            });
            return;
        }
        let selected = entries.len().saturating_sub(1);
        self.tree_picker = Some(TreePickerState { entries, selected });
    }

    /// Confirm the hovered entry: roll the transcript back to the chosen
    /// branch point, set the branch hint so the next run chains off it, and
    /// (for "edit and resend" entries) load the original prompt into the
    /// input box. The visual rollback replaces the old "branch ready" badge —
    /// the user sees the conversation up to the branch point immediately.
    fn tree_picker_confirm_inner(&mut self, picker: TreePickerState) {
        let Some(entry) = picker.entries.get(picker.selected).cloned() else {
            return;
        };
        let Some(path) = self.session.path.clone() else {
            return;
        };
        // Reload events from disk (the picker was built from a snapshot; the
        // file is the source of truth for the active-path walk).
        let events = match store::load(&path) {
            Ok((_meta, events, _offsets, _size)) => events,
            Err(e) => {
                self.push_turn(Turn {
                    prompt: "/tree".to_string(),
                    blocks: vec![Block::Error(format!("load session for /tree: {e}"))],
                });
                return;
            }
        };
        self.rollback_to(&events, &entry.branch_point);
        self.branch_from(entry.branch_point);
        if !entry.prefill.is_empty() {
            self.input = entry.prefill;
            self.input_cursor = self.input.chars().count();
        }
    }

    /// Unified key dispatch for list-style modal overlays (`/resume` and
    /// `/tree`). `↑/↓` or `j`/`k` or `Ctrl+N`/`Ctrl+P` move the selection
    /// (clamped); `Tab`/`Shift+Tab` cycle with wrap-around; `Enter`
    /// confirms; `Esc`/`q` cancels. Returns `true` if a modal handled the
    /// key (so the caller skips normal Input-mode processing).
    fn handle_modal_key(&mut self, k: &KeyEvent) -> bool {
        /// Which overlay slot is active, for per-slot confirm/cancel.
        enum Slot { Picker, Tree }
        let slot = if self.picker.is_some() {
            Slot::Picker
        } else if self.tree_picker.is_some() {
            Slot::Tree
        } else {
            return false;
        };
        let len = self.active_modal_mut().map_or(0, |m| m.len());
        // Confirm/cancel (and the single-item Tab shortcut) take `&mut self`
        // (or take the picker) and are handled before borrowing the modal for
        // navigation.
        match k.code {
            KeyCode::Enter => match slot {
                Slot::Picker => {
                    if let Some(picker) = self.picker.take() {
                        self.picker_confirm_inner(picker);
                    }
                }
                Slot::Tree => {
                    if let Some(picker) = self.tree_picker.take() {
                        self.tree_picker_confirm_inner(picker);
                    }
                }
            },
            // With a single entry, Tab/Shift+Tab confirm outright instead of
            // cycling (a no-op) — same as pressing Enter.
            KeyCode::Tab | KeyCode::BackTab if len == 1 => match slot {
                Slot::Picker => {
                    if let Some(picker) = self.picker.take() {
                        self.picker_confirm_inner(picker);
                    }
                }
                Slot::Tree => {
                    if let Some(picker) = self.tree_picker.take() {
                        self.tree_picker_confirm_inner(picker);
                    }
                }
            },
            KeyCode::Esc | KeyCode::Char('q') => match slot {
                Slot::Picker => self.picker = None,
                Slot::Tree => self.tree_picker = None,
            },
            _ => {}
        }
        if (matches!(slot, Slot::Picker) && self.picker.is_none())
            || (matches!(slot, Slot::Tree) && self.tree_picker.is_none())
        {
            // Confirm/cancel consumed the overlay; nothing left to navigate.
            return true;
        }
        let Some(m) = self.active_modal_mut() else { return true };
        if len == 0 {
            return true;
        }
        let s = m.selected();
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                m.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                m.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            // Ctrl+N / Ctrl+P — readline-style next/previous, matching the
            // popover and the Input-mode cursor keys.
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                m.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                m.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            // Tab/Shift+Tab cycle with wrap-around (last ↔ first).
            KeyCode::Tab => {
                m.set_selected((s + 1) % len);
            }
            KeyCode::BackTab => {
                m.set_selected(if s == 0 { len - 1 } else { s - 1 });
            }
            _ => {}
        }
        true
    }

    /// Borrow whichever modal overlay is currently active, for shared
    /// navigation. Only one slot is ever non-`None` at a time.
    fn active_modal_mut(&mut self) -> Option<&mut dyn Modal> {
        if self.picker.is_some() {
            self.picker.as_mut().map(|p| p as &mut dyn Modal)
        } else {
            self.tree_picker.as_mut().map(|t| t as &mut dyn Modal)
        }
    }

    /// Unified key dispatch for the slash-command autocomplete popover.
    /// `↑/↓` or `Ctrl+N`/`Ctrl+P` move the selection (clamped); `Tab`/
    /// `Shift+Tab` cycle with wrap-around (last ↔ first); `Enter` accepts
    /// the selection (auto-completes); `Esc` dismisses. Unlike
    /// [`handle_modal_key`], `j`/`k`/`q` are not intercepted — the popover
    /// floats over a text input, so those must stay printable. Returns
    /// `true` if the popover handled the key.
    fn handle_popover_key(&mut self, k: &KeyEvent) -> bool {
        if self.slash_complete.is_none() {
            return false;
        }
        let len = self.slash_complete.as_ref().map_or(0, |p| p.len());
        // Accept/dismiss (and the single-item Tab shortcut) take `&mut self`
        // and are handled before borrowing the popover for navigation.
        match k.code {
            KeyCode::Enter => {
                self.slash_complete_accept();
                return true;
            }
            // With a single candidate, Tab/Shift+Tab accept it outright
            // instead of cycling (a no-op) — same as pressing Enter.
            KeyCode::Tab | KeyCode::BackTab if len == 1 => {
                self.slash_complete_accept();
                return true;
            }
            KeyCode::Esc => {
                self.slash_complete = None;
                return true;
            }
            _ => {}
        }
        let Some(popover) = self.slash_complete.as_mut().map(|p| p as &mut dyn Popover)
        else {
            return false;
        };
        if len == 0 {
            return false;
        }
        let s = popover.selected();
        match k.code {
            KeyCode::Up => {
                popover.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            KeyCode::Down => {
                popover.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            // Ctrl+N / Ctrl+P — readline-style next/previous, matching the
            // Input-mode cursor keys.
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                popover.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                popover.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            // Tab/Shift+Tab cycle with wrap-around (last ↔ first).
            KeyCode::Tab => {
                popover.set_selected((s + 1) % len);
            }
            KeyCode::BackTab => {
                popover.set_selected(if s == 0 { len - 1 } else { s - 1 });
            }
            _ => return false,
        }
        true
    }

    /// Test helper: confirm the active tree picker. In production,
    /// [`handle_modal_key`] dispatches Enter through the per-slot
    /// `tree_picker_confirm_inner`.
    #[cfg(test)]
    fn tree_picker_confirm(&mut self) {
        if let Some(picker) = self.tree_picker.take() {
            self.tree_picker_confirm_inner(picker);
        }
    }

    /// Rebuild the visible turns and the agent's message history from the
    /// active path root → `leaf_id` (inclusive), discarding everything after
    /// that point from the in-memory view. Cost/usage are reset and
    /// re-accumulated from the replayed `turn_end` events. Byte ranges are
    /// dropped: after a rollback the visible turns are rendered from
    /// in-memory blocks, not the file-backed frozen-turn cache (the cache is
    /// invalidated by `bump_render_epoch`). The on-disk file is untouched —
    /// the rolled-back branches remain and are reachable via `/tree` again.
    fn rollback_to(&mut self, events: &[SessionEvent], leaf_id: &str) {
        let path = store::active_path(events, leaf_id);
        let rolled_back: Vec<SessionEvent> =
            path.iter().map(|&i| events[i].clone()).collect();
        let messages = messages_from_events(&rolled_back);
        if let Ok(mut m) = self.history.lock() {
            *m = messages;
        }
        self.turns = Vec::new();
        self.turn_byte_ranges = Vec::new();
        self.cost = 0.0;
        self.total_in = 0;
        self.total_out = 0;
        self.status_usage = None;
        for ev in replay_session_events(&rolled_back) {
            self.apply_event(ev);
        }
        self.bump_render_epoch();
        self.pinned = true;
        self.top_line = 0;
    }

    /// Header line: `lofi` wordmark at the left, the working directory
    /// (abbreviated to fit) right-aligned. The model and cost live in the
    /// footer; the header carries no status tag.
    pub(crate) fn render_header_line(&self, width: usize) -> Line<'static> {
        let t = self.theme;
        let wordmark = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
        let muted = Style::new().fg(t.muted);
        let lofi_w = unicode_width::UnicodeWidthStr::width("lofi");
        // Model label sits at the right edge; the cwd follows the wordmark
        // on the left, abbreviated to whatever the model leaves behind.
        let model = self.render_footer_right();
        let model_w: usize = model
            .spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let budget = width
            .saturating_sub(lofi_w)
            .saturating_sub(1)
            .saturating_sub(model_w)
            .saturating_sub(2);
        let cwd = abbreviate_path(&self.session.cwd, budget);
        HStack::new(width)
            .left([
                Span::styled("lofi", wordmark),
                Span::raw(" "),
                Span::styled(cwd, muted),
            ])
            .right(model.spans)
            .build()
    }

    /// Bottom-left footer: `↑in ↓out · context used/limit · N% cached`.
    /// separately on the right via [`render_footer_cost`].
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
        // Context gauge: the latest turn's full prompt size (input + output +
        // cache read + cache write).
        // Cache tokens are included so the gauge reflects the real window usage
        // rather than only the non-cached slice. When the provider reports
        // cache activity, append the hit rate as `N% cached`.
        let used = self.status_usage.map_or(0, |u| {
            u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens
        });
        let cached_suffix = self.status_usage.and_then(|u| {
            let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
            if prompt > 0 && (u.cache_read_tokens > 0 || u.cache_write_tokens > 0) {
                let rate = u.cache_read_tokens as f64 / prompt as f64 * 100.0;
                Some(format!(" · {:.0}% cached", rate))
            } else {
                None
            }
        });
        segments.push(format!(
            "context {}/{}{}",
            compact_count(used),
            compact_count(self.ctx_limit),
            cached_suffix.unwrap_or_default()
        ));
        Line::from(vec![Span::styled(segments.join(sep), Style::new().fg(t.muted))])
    }

    /// Text for the transient "Copied to clipboard" badge, or `None` if the
    /// yank notification has expired.
    pub(crate) fn yank_badge(&self) -> Option<&'static str> {
        match self.yank_notify {
            Some(t) if t.elapsed() < YANK_NOTIFY => Some("Copied to clipboard"),
            _ => None,
        }
    }

    /// Text for the transient "Press Ctrl-C again to quit" badge shown after a
    /// first `C-c` on an empty prompt, or `None` once the double-press window
    /// has elapsed.
    pub(crate) fn quit_badge(&self) -> Option<&'static str> {
        match self.ctrl_c_at {
            Some(t) if t.elapsed() < QUIT_DOUBLE_PRESS => Some("Press Ctrl-C again to quit"),
            _ => None,
        }
    }

    /// Footer cost, shown on the right edge of the usage line. Includes the
    /// current turn's running cost (`turn_cost`) so a multi-round turn shows
    /// a live total before `TurnEnd` folds it into `cost`.
    pub(crate) fn render_footer_cost(&self) -> Line<'static> {
        Line::from(vec![Span::styled(
            fmt_cost(self.cost + self.turn_cost),
            Style::new().fg(self.theme.muted),
        )])
    }

    /// Bottom-right footer: the model badge (with thinking level).
    pub(crate) fn render_footer_right(&self) -> Line<'static> {
        let mut label = self.model_label.clone();
        if let Some(tl) = &self.thinking_label {
            label.push_str(tl);
        }
        Line::from(vec![Span::styled(label, Style::new().fg(self.theme.muted))])
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

/// Number of select rows a single logical line occupies when soft-wrapped to
/// `content_w` cells. Mirrors the wrap loop in [`App::input_select_rows`].
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
/// [`App::input_select_rows`] so the cursor lands exactly where the text
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
            &ev.kind,
            SessionEventKind::Message(m)
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

/// The turn-building core of [`App::apply_event`], free of `App`-owned state
/// (cost/usage totals, retry indicator, byte ranges). Shared by the live
/// `apply_event` and by [`turns_from_session_events`] (used to materialize a
/// frozen turn from its byte range on demand) so there is exactly one place
/// that maps an `AgentEvent` to `Block`s.
///
/// `TurnStart` pushes a new turn; every other event mutates the last turn.
/// Status-only events (`RetryStart`/`RetryEnd`/`TurnCommitted`) are no-ops
/// here — the caller (`App::apply_event`) handles them before calling this.
fn apply_event_to_turns(turns: &mut Vec<Turn>, ev: AgentEvent) {
    if let AgentEvent::TurnStart { prompt } = ev {
        turns.push(Turn {
            prompt,
            blocks: Vec::new(),
        });
        return;
    }
    let Some(turn) = turns.last_mut() else {
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
        AgentEvent::ThinkingEnd { elapsed_ms } => {
            // The engine owns the thinking-block timer; stamp it here so the
            // "Thought for Ns" marker matches the persisted `ThinkingTiming`
            // on resume (the UI's own `start` Instant is only a fallback while
            // the block is still open).
            if let Some(Block::Thinking(t)) = turn.blocks.last_mut() {
                if t.elapsed.is_none() {
                    t.elapsed = Some(Duration::from_millis(elapsed_ms));
                }
            }
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
                // replaces it with the authoritative full code and stamps the
                // label. Falls back to `code` when nothing streamed.
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
        AgentEvent::TurnEnd { label, elapsed_ms, .. } => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::TurnEnd {
                label,
                elapsed: Duration::from_millis(elapsed_ms),
            });
        }
        AgentEvent::TurnFailed { label, elapsed_ms, error, .. } => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::TurnFailed {
                label,
                elapsed: Duration::from_millis(elapsed_ms),
                error,
            });
        }
        AgentEvent::Error(msg) => {
            finalize_open_thinking(turn);
            turn.blocks.push(Block::Error(msg));
        }
        // Status-only events are handled by `App::apply_event` before
        // reaching this builder; they are no-ops here.
        AgentEvent::RetryStart { .. }
        | AgentEvent::RetryEnd { .. }
        | AgentEvent::TurnCommitted { .. }
        | AgentEvent::RoundUsage { .. }
        | AgentEvent::TurnStart { .. } => {}
    }
}

/// Build a `Vec<Turn>` from a transcript event log by replaying it through
/// [`apply_event_to_turns`]. Used to materialize a frozen turn from its byte
/// range on demand (`materialize_turn`); the live path and full-session
/// resume go through `App::apply_event` instead, which also updates totals.
fn turns_from_session_events(events: &[SessionEvent]) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();
    for ev in replay_session_events(events) {
        apply_event_to_turns(&mut turns, ev);
    }
    turns
}

/// Reconstruct a faithful [`AgentEvent`] stream from a transcript event log,
/// so the resume path and the live path share one builder ([`App::apply_event`]).
///
/// Tool timings, thinking timings, and native-tool records are gathered first
/// (they are written after the messages) so each `ToolUse` block can be
/// stamped as it is replayed. A `SessionEvent::TurnEnd` becomes the matching
/// `AgentEvent::TurnEnd`, attaching the `◇ label done in Ns` block to the
/// turn it follows.
///
/// Tool results in the durable log travel as a separate `Message` with
/// `Role::Tool` (or, for legacy entries, `Role::User` carrying `ToolResult`
/// blocks) *after* the assistant's `ToolUse`. The live stream delivers the
/// result inline via `AgentEvent::ToolEnd`, so the replay adapter defers each
/// `ToolEnd` until the matching tool-result message arrives — keeping
/// `apply_event`'s assumption that `ToolEnd` carries the result.
#[allow(clippy::too_many_lines)]
fn replay_session_events(events: &[SessionEvent]) -> Vec<AgentEvent> {
    use std::collections::HashMap as Map;
    let mut tool_elapsed: Map<String, u64> = Map::new();
    let mut native_by_parent: Map<String, Vec<NativeToolRecord>> = Map::new();
    // Thinking-block durations in emission order, matched positionally to
    // assistant `Thinking` blocks as they are replayed.
    let mut thinking_timing: Vec<u64> = Vec::new();
    for ev in events {
        match &ev.kind {
            SessionEventKind::ToolTiming { tool_call_id, elapsed_ms } => {
                tool_elapsed.insert(tool_call_id.clone(), *elapsed_ms);
            }
            SessionEventKind::ThinkingTiming { elapsed_ms } => {
                thinking_timing.push(*elapsed_ms);
            }
            SessionEventKind::NativeTool(rec) => {
                native_by_parent
                    .entry(rec.parent.clone())
                    .or_default()
                    .push(rec.clone());
            }
            _ => {}
        }
    }
    let mut out: Vec<AgentEvent> = Vec::new();
    let mut thinking_idx = 0usize;
    for ev in events {
        match &ev.kind {
            SessionEventKind::Message(msg) => match msg.role {
                Role::User => {
                    // ToolResult blocks attach to the current turn's pending
                    // tool calls as deferred `ToolEnd` events.
                    if msg.blocks.iter().any(|b| matches!(b, ContentBlock::ToolResult { .. })) {
                        for b in &msg.blocks {
                            if let ContentBlock::ToolResult { tool_use_id, content, is_error } = b {
                                out.push(AgentEvent::ToolEnd {
                                    id: tool_use_id.clone(),
                                    result: if *is_error {
                                        format!("error: {content}")
                                    } else {
                                        content.clone()
                                    },
                                    is_error: *is_error,
                                    elapsed_ms: tool_elapsed.get(tool_use_id).copied().unwrap_or(0),
                                });
                            }
                        }
                        continue;
                    }
                    // Otherwise a prompt: start a new turn.
                    let prompt = msg
                        .blocks
                        .iter()
                        .find_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    out.push(AgentEvent::TurnStart { prompt });
                }
                Role::Assistant => {
                    for b in &msg.blocks {
                        match b {
                            ContentBlock::Text { text } => {
                                out.push(AgentEvent::Text(text.clone()));
                            }
                            ContentBlock::Thinking { text, .. } => {
                                out.push(AgentEvent::Thinking(text.clone()));
                                let elapsed = thinking_timing
                                    .get(thinking_idx)
                                    .copied()
                                    .unwrap_or(0);
                                thinking_idx += 1;
                                out.push(AgentEvent::ThinkingEnd { elapsed_ms: elapsed });
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                out.push(AgentEvent::ToolStart {
                                    id: id.clone(),
                                    name: name.clone(),
                                });
                                // For a restored `exec`, split the stored
                                // input JSON back into the code (shown with
                                // line numbers) and the `display` label.
                                let (code, label) = if name == "exec" {
                                    lofi_core::exec_input_code_and_label(input)
                                } else {
                                    (input.to_string(), None)
                                };
                                out.push(AgentEvent::ToolInput {
                                    id: id.clone(),
                                    code,
                                    label,
                                });
                                // Replay the native tool calls that ran inside
                                // this exec, in order, before the `ToolEnd`
                                // (which is deferred to the tool-result
                                // message below).
                                if let Some(natives) = native_by_parent.get(id) {
                                    for rec in natives {
                                        out.push(AgentEvent::NativeToolStart {
                                            parent: id.clone(),
                                            id: rec.call_id,
                                            name: rec.name.clone(),
                                            args: rec.args.clone(),
                                        });
                                        out.push(AgentEvent::NativeToolEnd {
                                            parent: id.clone(),
                                            id: rec.call_id,
                                            result: rec.result.clone(),
                                            is_error: rec.is_error,
                                        });
                                    }
                                }
                            }
                            ContentBlock::ToolResult { .. } => {}
                        }
                    }
                }
                // The engine writes tool results as `Role::Tool`; replay
                // them as deferred `ToolEnd` events, matching the live
                // stream's inline-result semantics.
                Role::Tool => {
                    for b in &msg.blocks {
                        if let ContentBlock::ToolResult { tool_use_id, content, is_error } = b {
                            out.push(AgentEvent::ToolEnd {
                                id: tool_use_id.clone(),
                                result: if *is_error {
                                    format!("error: {content}")
                                } else {
                                    content.clone()
                                },
                                is_error: *is_error,
                                elapsed_ms: tool_elapsed.get(tool_use_id).copied().unwrap_or(0),
                            });
                        }
                    }
                }
                Role::System => {}
            },
            SessionEventKind::NativeTool(_) | SessionEventKind::ToolTiming { .. } | SessionEventKind::ThinkingTiming { .. } => {}
            SessionEventKind::TurnEnd { label, elapsed_ms, cost, usage, .. } => {
                out.push(AgentEvent::TurnEnd {
                    label: label.clone(),
                    elapsed_ms: *elapsed_ms,
                    cost: *cost,
                    usage: *usage,
                });
            }
            SessionEventKind::TurnFailed { label, elapsed_ms, error, cost, usage, .. } => {
                out.push(AgentEvent::TurnFailed {
                    label: label.clone(),
                    elapsed_ms: *elapsed_ms,
                    error: error.clone(),
                    cost: *cost,
                    usage: *usage,
                });
            }
        }
    }
    out
}

/// Build the '/tree' picker entries from a lightweight event index.
///
/// Uses [`store::load_index`] (id + parent_id + kind discriminant only —
/// no ContentBlock deserialization) to build the tree shape, then loads
/// labels on demand via [`store::load_event_at`]. This keeps `/tree` fast
/// on large sessions: the full [`store::load`] is avoided entirely.
///
/// The active path (root → `leaf_id`, or the file's last event when
/// `leaf_id` is `None`) is the trunk — rendered flat. Only actual branches
/// (non-active sibling turns) create indentation, so the common case is two
/// levels deep regardless of conversation length.
///
/// Node kinds:
/// - `user:` — a user-prompt event. Selecting rolls back to BEFORE the
///   prompt and prefills the input (edit and resend).
/// - `agent:` — a `turn_end`/`turn_failed`. Selecting rolls back to AFTER
///   the turn (inclusive), input empty (continue from here).
fn build_tree_entries(
    indices: &[store::EventIndex],
    leaf_id: Option<&str>,
    path: &Path,
) -> Vec<TreeEntry> {
    let mut children_by_parent: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_id: HashMap<&str, usize> = HashMap::new();
    for (i, ix) in indices.iter().enumerate() {
        if !ix.id.is_empty() {
            by_id.insert(ix.id.as_str(), i);
        }
        if let Some(p) = ix.parent_id.as_deref() {
            if !p.is_empty() {
                children_by_parent.entry(p).or_default().push(i);
            }
        }
    }
    let active_path: Vec<usize> = match leaf_id {
        Some(id) if !id.is_empty() => active_path_from_index(indices, &by_id, id),
        // `leaf_id` is `None` (normal linear continuation) or `Some("")`
        // (rolled back to before the root prompt). For `None`, walk from
        // the file's last event. For `Some("")`, the active path is
        // empty — the trunk loop below renders nothing, and we instead
        // treat the root events as branch roots so the whole tree is
        // visible (nothing highlighted).
        None => indices
            .last()
            .map(|ix| active_path_from_index(indices, &by_id, &ix.id))
            .unwrap_or_default(),
        Some(_) => Vec::new(),
    };
    let active_set: std::collections::HashSet<usize> =
        active_path.iter().copied().collect();

    // Trunk = active path filtered to displayable nodes.
    let trunk: Vec<usize> = active_path
        .iter()
        .copied()
        .filter(|&i| is_tree_node(&indices[i].kind))
        .collect();

    let n = trunk.len();
    let mut out = Vec::new();
    for (pos, &idx) in trunk.iter().enumerate() {
        let is_last = pos == n - 1;
        let connector = if is_last { "└─ " } else { "├─ " };
        let child_indent = if is_last { "   " } else { "│  " };
        push_tree_entry(
            idx, connector, active_set.contains(&idx),
            indices, &by_id, path, &mut out,
        );
        let mut branches: Vec<usize> = Vec::new();
        if indices[idx].kind == store::IndexKind::UserPrompt {
            if let Some(te_idx) = find_turn_outcome(idx, indices, &children_by_parent) {
                if !active_set.contains(&te_idx) {
                    branches.push(te_idx);
                }
            }
        }
        let user_branches: Vec<usize> = children_by_parent
            .get(indices[idx].id.as_str())
            .into_iter()
            .flatten()
            .copied()
            .filter(|&i| {
                indices[i].kind == store::IndexKind::UserPrompt && !active_set.contains(&i)
            })
            .collect();
        branches.extend(user_branches);
        if !branches.is_empty() {
            render_branch_subtree(
                &branches, indices, &children_by_parent, &by_id, path,
                child_indent, &mut out,
            );
        }
    }
    // Rolled back to before the root prompt: the trunk is empty, so
    // render every top-level tree node (a tree node whose nearest
    // tree-node ancestor — walking up the parent chain — is absent) as a
    // branch. Nothing is active. The file's root may be a system message
    // (not a tree node), so we can't just take parent_id.is_none().
    if trunk.is_empty() && out.is_empty() {
        let roots: Vec<usize> = indices
            .iter()
            .enumerate()
            .filter(|&(_, ix)| {
                if !is_tree_node(&ix.kind) {
                    return false;
                }
                // Walk up the parent chain; this is a top-level tree node
                // iff no ancestor is a tree node.
                let mut cur = ix.parent_id.as_deref();
                while let Some(pid) = cur {
                    let Some(&pidx) = by_id.get(pid) else { break };
                    if is_tree_node(&indices[pidx].kind) {
                        return false;
                    }
                    cur = indices[pidx].parent_id.as_deref();
                }
                true
            })
            .map(|(i, _)| i)
            .collect();
        if !roots.is_empty() {
            render_branch_subtree(
                &roots, indices, &children_by_parent, &by_id, path, "", &mut out,
            );
        }
    }
    out
}

/// Active path (root-first indices) from a leaf id, using the lightweight
/// index instead of fully-loaded events.
fn active_path_from_index(
    indices: &[store::EventIndex],
    by_id: &HashMap<&str, usize>,
    leaf_id: &str,
) -> Vec<usize> {
    let mut path = Vec::new();
    let mut cur = by_id.get(leaf_id).copied();
    while let Some(i) = cur {
        path.push(i);
        cur = indices[i]
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied());
    }
    path.reverse();
    path
}

/// Render branch subtrees. Each root is rendered as its own subtree:
/// the root's linear chain (root → turn outcome → next user prompt → …)
/// sits under the root's connector at this indentation level, and sibling
/// roots are siblings of each other — not flattened into one list. Only
/// actual sub-branches (divergences within a chain) create further
/// indentation.
fn render_branch_subtree(
    roots: &[usize],
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&str, Vec<usize>>,
    by_id: &HashMap<&str, usize>,
    path: &Path,
    prefix: &str,
    out: &mut Vec<TreeEntry>,
) {
    let n = roots.len();
    for (pos, &root) in roots.iter().enumerate() {
        let is_last = pos == n - 1;
        let connector = if is_last { "└─ " } else { "├─ " };
        let child_indent = format!("{prefix}{}", if is_last { "   " } else { "│  " });
        // Chain = this root + its linear descendants (flat at this level,
        // under the root's connector).
        let chain = walk_chain(root, indices, children_by_parent);
        let chain_set: std::collections::HashSet<usize> = chain.iter().copied().collect();
        let cn = chain.len();
        for (cpos, &idx) in chain.iter().enumerate() {
            let cprefix = if cpos == 0 {
                format!("{prefix}{connector}")
            } else {
                let cis_last = cpos == cn - 1;
                format!("{child_indent}{}", if cis_last { "└─ " } else { "├─ " })
            };
            let sub_indent = format!("{child_indent}{}", if cpos == cn - 1 { "   " } else { "│  " });
            push_tree_entry(idx, &cprefix, false, indices, by_id, path, out);
            let mut sub_branches: Vec<usize> = Vec::new();
            if indices[idx].kind == store::IndexKind::UserPrompt {
                if let Some(te_idx) = find_turn_outcome(idx, indices, children_by_parent) {
                    if !chain_set.contains(&te_idx) {
                        sub_branches.push(te_idx);
                    }
                }
            }
            let user_children: Vec<usize> = children_by_parent
                .get(indices[idx].id.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|&i| {
                    indices[i].kind == store::IndexKind::UserPrompt
                        && !chain_set.contains(&i)
                })
                .collect();
            sub_branches.extend(user_children);
            if !sub_branches.is_empty() {
                render_branch_subtree(
                    &sub_branches, indices, &children_by_parent, &by_id, path,
                    &sub_indent, out,
                );
            }
        }
    }
}

/// Walk the linear chain from `start`: user → turn outcome → next user
/// prompt → …, following the first user-prompt child at each turn_end and
/// the turn outcome at each user prompt.
fn walk_chain(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&str, Vec<usize>>,
) -> Vec<usize> {
    let mut chain = vec![start];
    let mut visited = std::collections::HashSet::new();
    visited.insert(start);
    let mut cur = start;
    loop {
        let next = if indices[cur].kind == store::IndexKind::UserPrompt {
            find_turn_outcome(cur, indices, children_by_parent)
        } else {
            children_by_parent
                .get(indices[cur].id.as_str())
                .into_iter()
                .flatten()
                .copied()
                .find(|&i| indices[i].kind == store::IndexKind::UserPrompt)
        };
        match next {
            Some(n) if visited.insert(n) => {
                chain.push(n);
                cur = n;
            }
            _ => break,
        }
    }
    chain
}

/// Append one `TreeEntry` for index `idx`, loading the label lazily from
/// disk via the event's byte offset.
fn push_tree_entry(
    idx: usize,
    prefix: &str,
    is_active: bool,
    indices: &[store::EventIndex],
    by_id: &HashMap<&str, usize>,
    path: &Path,
    out: &mut Vec<TreeEntry>,
) {
    let ix = &indices[idx];
    let (label, prefill, branch_point) = match ix.kind {
        store::IndexKind::UserPrompt => {
            let prompt = load_prompt_text(path, ix.offset);
            (
                format!("user: {}", one_line(&prompt)),
                prompt,
                ix.parent_id.clone().unwrap_or_default(),
            )
        }
        store::IndexKind::TurnEnd => {
            let preview = load_assistant_preview(idx, indices, by_id, path);
            (
                format!(
                    "agent: {}",
                    if preview.is_empty() {
                        "(turn end)".to_string()
                    } else {
                        preview
                    }
                ),
                String::new(),
                ix.id.clone(),
            )
        }
        store::IndexKind::TurnFailed => {
            let error = load_failed_error(path, ix.offset);
            (
                format!("agent: {} (failed)", one_line(&error)),
                String::new(),
                ix.id.clone(),
            )
        }
        store::IndexKind::AssistantMessage | store::IndexKind::Other => return,
    };
    out.push(TreeEntry {
        prefix: prefix.to_string(),
        label,
        prefill,
        branch_point,
        is_active,
    });
}

/// Whether a kind is a displayable tree node (user prompt or turn outcome).
fn is_tree_node(kind: &store::IndexKind) -> bool {
    matches!(
        kind,
        store::IndexKind::UserPrompt
            | store::IndexKind::TurnEnd
            | store::IndexKind::TurnFailed
    )
}

/// Walk the descendant chain from `start` (a user-prompt event) to find the
/// first turn_end/turn_failed — the outcome of this turn. Follows the
/// in-turn chain (assistant → tool → thinking → …), skipping user-prompt
/// children that are branches.
fn find_turn_outcome(
    start: usize,
    indices: &[store::EventIndex],
    children_by_parent: &HashMap<&str, Vec<usize>>,
) -> Option<usize> {
    let mut cur = start;
    let mut visited = std::collections::HashSet::new();
    loop {
        if !visited.insert(cur) {
            return None;
        }
        match indices[cur].kind {
            store::IndexKind::TurnEnd | store::IndexKind::TurnFailed => return Some(cur),
            _ => {}
        }
        let children = children_by_parent.get(indices[cur].id.as_str())?;
        cur = *children
            .iter()
            .find(|&&i| indices[i].kind != store::IndexKind::UserPrompt)?;
    }
}

/// Preview of the last assistant text in the turn ending at `turn_end_idx`:
/// walk the parent chain (using the index) back to the user prompt, loading
/// only assistant-message events to find the first text block.
fn load_assistant_preview(
    turn_end_idx: usize,
    indices: &[store::EventIndex],
    by_id: &HashMap<&str, usize>,
    path: &Path,
) -> String {
    let mut cur = turn_end_idx;
    let mut visited = std::collections::HashSet::new();
    while let Some(parent_id) = indices[cur].parent_id.as_deref() {
        if !visited.insert(cur) {
            break;
        }
        let Some(&pidx) = by_id.get(parent_id) else { break };
        let pix = &indices[pidx];
        if pix.kind == store::IndexKind::UserPrompt {
            break;
        }
        if pix.kind == store::IndexKind::AssistantMessage {
            if let Some(text) = load_assistant_text(path, pix.offset) {
                return one_line(&text);
            }
        }
        cur = pidx;
    }
    String::new()
}

/// Load a user-prompt event and extract its first text block.
fn load_prompt_text(path: &Path, offset: u64) -> String {
    let Ok(ev) = store::load_event_at(path, offset) else {
        return String::new();
    };
    if let SessionEventKind::Message(m) = ev.kind {
        if m.role == Role::User {
            return m
                .blocks
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
        }
    }
    String::new()
}

/// Load an assistant-message event and extract its first text block.
fn load_assistant_text(path: &Path, offset: u64) -> Option<String> {
    let ev = store::load_event_at(path, offset).ok()?;
    let SessionEventKind::Message(m) = ev.kind else { return None };
    if m.role != Role::Assistant {
        return None;
    }
    m.blocks
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
}

/// Load a turn_failed event and extract its error message.
fn load_failed_error(path: &Path, offset: u64) -> String {
    let Ok(ev) = store::load_event_at(path, offset) else {
        return String::new();
    };
    if let SessionEventKind::TurnFailed { error, .. } = ev.kind {
        error
    } else {
        String::new()
    }
}fn one_line(s: &str) -> String {
    const MAX: usize = 60;
    let collapsed = s.replace('\n', " ⏎ ");
    if collapsed.chars().count() <= MAX {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(MAX).collect();
        out.push('…');
        out
    }
}

fn messages_from_events(events: &[SessionEvent]) -> Vec<Message> {
    let path = store::active_path_from_leaf(events);
    let mut out: Vec<Message> = Vec::new();
    let mut skipping = false;
    // Iterate leaf-first so the `TurnFailed` boundary is seen before its
    // ancestors; `path` is root-first, so reverse.
    for &i in path.iter().rev() {
        match &events[i].kind {
            SessionEventKind::TurnFailed { .. } => {
                skipping = true;
            }
            SessionEventKind::TurnEnd { .. } => {
                skipping = false;
            }
            SessionEventKind::Message(m) if !skipping => {
                out.push(m.clone());
            }
            _ => {}
        }
    }
    // `out` is leaf-first; reverse to root-first for the model.
    out.reverse();
    out
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
            DisableBracketedPaste,
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
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableBracketedPaste)?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    let terminal = match setup {
        Ok(t) => t,
        Err(e) => {
            let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture, LeaveAlternateScreen);
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
    // Replay the durable event log through `apply_event` — the same builder
    // the live stream uses — so resume and live share one code path. Totals
    // are restored by the replayed `TurnEnd` events.
    for ev in replay_session_events(&events) {
        app.apply_event(ev);
    }
    app.turn_byte_ranges = turn_byte_ranges_from_events(&events, &offsets, file_size);
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
                // The spinner and the retry countdown both animate and need
                // periodic redraw; an idle session has nothing to draw — except
                // while the yank badge is on screen, which must expire.
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
                // The quit-confirmation badge must expire on its own.
                if let Some(t) = app.ctrl_c_at {
                    if t.elapsed() >= QUIT_DOUBLE_PRESS {
                        app.ctrl_c_at = None;
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
    if let Event::Paste(s) = ev {
        // Paste only types into the prompt; ignored in Navigate/Select.
        if app.mode == Mode::Input {
            app.sel = None;
            app.insert_str(s);
        }
        return;
    }
    let Event::Key(k) = ev else {
        return;
    };
    if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    // Modal overlays (`/resume` and `/tree`) intercept keys while open:
    // up/down (`↑/↓` or `j`/`k`), Enter to confirm, `Esc`/`q` to cancel.
    if app.handle_modal_key(k) {
        return;
    }

    // Ctrl+C cancels a run, clears the draft, or quits on double-press in
    // Input. In Navigate/Select it returns to Input and snaps the viewport
    // to the latest transcript line (a run, if active, keeps running — press
    // Ctrl+C again in Input to cancel it).
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        if app.mode != Mode::Input {
            app.enter_input();
            app.pin_to_latest();
            return;
        }
        handle_ctrl_c(app, current_run);
        return;
    }

    // Navigate / Select carry their own keymaps.
    if app.mode != Mode::Input {
        app.last_kill_was_kill = false;
        match app.mode {
            Mode::Navigate => handle_nav_key(k, app),
            Mode::Select => handle_select_key(k, app),
            Mode::Input => {}
        }
        return;
    }

    // --- Input mode ---
    // Any key press clears an active mouse selection (tmux-style).
    app.sel = None;
    // Capture before reset so consecutive `C-k` appends to the kill ring.
    let append_kill = app.last_kill_was_kill;
    app.last_kill_was_kill = false;

    if k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::ALT) {
        app.insert_newline();
        app.refresh_slash_complete();
        return;
    }
    // Ctrl+J is a newline in readline / Emacs; treat it like Alt+Enter.
    if k.code == KeyCode::Char('j') && k.modifiers.contains(KeyModifiers::CONTROL) {
        app.insert_newline();
        app.refresh_slash_complete();
        return;
    }
    // Slash-command autocomplete popover intercepts navigation/accept/dismiss
    // keys while active. Typing and other edits fall through to the normal
    // Input handlers and re-filter the popover via `refresh_slash_complete`.
    if app.handle_popover_key(k) {
        return;
    }
    match k.code {
        KeyCode::Enter if current_run.is_none() && !app.input.is_empty() => {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.history_idx = None;
            app.slash_complete = None;
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
            let Some(agent) = agent else {
                // No model configured: there is no agent to emit `TurnStart`,
                // so push the turn manually and surface the hint on it.
                app.push_turn(Turn {
                    prompt: prompt.clone(),
                    blocks: Vec::new(),
                });
                if let Some(hint) = &app.no_models_hint {
                    if let Some(turn) = app.turns.last_mut() {
                        turn.blocks.push(Block::Error(hint.clone()));
                    }
                }
                return;
            };
            // The new turn is pushed by `AgentEvent::TurnStart` when the
            // engine begins the run — keeping turn creation in one place
            // (the event handler) for both live and resumed sessions.
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let history = Arc::clone(&app.history);
            let session_path = app.session.path.clone();
            let commit = session_path.as_ref().map(|p| SessionCommit {
                path: p.clone(),
                label: app.session_model(),
                parent_hint: app.branch_hint.take(),
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
        KeyCode::Tab => app.enter_nav(),
        KeyCode::Esc => app.clear_input(),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
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
    // Re-filter the autocomplete popover after any input edit. Commands that
    // `return` early (newline, autocomplete accept/dismiss) call
    // `refresh_slash_complete` themselves or clear the popover directly.
    app.refresh_slash_complete();
}

/// Ctrl+C: cancel an active run; otherwise clear a non-empty draft, or quit
/// on a double press within [`QUIT_DOUBLE_PRESS`] when the prompt is empty.
/// Mode-independent — works the same in Input, Navigate, and Select.
fn handle_ctrl_c(app: &mut App, current_run: &mut Option<RunHandle>) {
    if let Some(r) = current_run.take() {
        r.handle.abort();
        if let Some(turn) = app.turns.last_mut() {
            turn.blocks.push(Block::Error("cancelled".to_string()));
        }
        app.run_finished();
        app.ctrl_c_at = None;
        return;
    }
    if app.input.is_empty() {
        let now = Instant::now();
        if app.ctrl_c_at.is_some_and(|t| now.duration_since(t) < QUIT_DOUBLE_PRESS) {
            app.should_quit = true;
        } else {
            app.ctrl_c_at = Some(now);
        }
    } else {
        app.clear_input();
        app.ctrl_c_at = None;
    }
}

/// Vim-style word/WORD motion. `big` distinguishes `w`/`b`/`e` (words are
/// runs of word-chars or punctuation) from `W`/`B`/`E` (runs of non-space).
#[derive(Clone, Copy)]
enum WordMotion {
    NextStart { big: bool },
    PrevStart { big: bool },
    NextEnd { big: bool },
}

/// Dispatches the vim line-movement keys (`$`, `^`, `0`, `w`/`b`/`e`/`W`/`B`/`E`).
/// Returns `true` when the key was a motion so the caller can skip its own
/// match. Shared by Navigate and Select.
fn apply_motion(k: &KeyEvent, app: &mut App) -> bool {
    match k.code {
        KeyCode::Char('$') => {
            let (_, cend) = app.cursor_content_range();
            app.nav_set_col(cend.saturating_sub(1));
            true
        }
        KeyCode::Char('0') => {
            let (cstart, _) = app.cursor_content_range();
            app.nav_set_col(cstart);
            true
        }
        KeyCode::Char('^') => {
            app.nav_set_col(app.first_nonblank_col());
            true
        }
        KeyCode::Char('w') => {
            app.nav_word_motion(WordMotion::NextStart { big: false });
            true
        }
        KeyCode::Char('W') => {
            app.nav_word_motion(WordMotion::NextStart { big: true });
            true
        }
        KeyCode::Char('b') => {
            app.nav_word_motion(WordMotion::PrevStart { big: false });
            true
        }
        KeyCode::Char('B') => {
            app.nav_word_motion(WordMotion::PrevStart { big: true });
            true
        }
        KeyCode::Char('e') => {
            app.nav_word_motion(WordMotion::NextEnd { big: false });
            true
        }
        KeyCode::Char('E') => {
            app.nav_word_motion(WordMotion::NextEnd { big: true });
            true
        }
        _ => false,
    }
}

/// Navigate keymap: j/k and arrows move the cursor, g/G jump to top/bottom, v
/// enters Select, i/Tab return to Input. Control combos (other than the
/// globally-handled C-c) are ignored.
fn handle_nav_key(k: &KeyEvent, app: &mut App) {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    if apply_motion(k, app) {
        return;
    }
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => {
            app.sel = None;
            app.nav_move(1);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.sel = None;
            app.nav_move(-1);
        }
        KeyCode::Char('h') | KeyCode::Left => {
            app.sel = None;
            app.nav_col_delta(-1);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.sel = None;
            app.nav_col_delta(1);
        }
        KeyCode::Char('g') => {
            app.sel = None;
            app.nav_top();
        }
        KeyCode::Char('G') => {
            app.sel = None;
            app.nav_bottom();
        }
        KeyCode::Char('v') => app.enter_select(),
        KeyCode::Char('y') => {
            app.yank_line();
            app.enter_input();
        }
        KeyCode::Char('i') | KeyCode::Tab => app.enter_input(),
        KeyCode::Char('[') => app.nav_jump_turn(-1),
        KeyCode::Char(']') => app.nav_jump_turn(1),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        _ => {}
    }
}

/// Select keymap: movement extends the selection, y/Enter yanks it,
/// Tab/Esc drops the selection and returns to Navigate.
fn handle_select_key(k: &KeyEvent, app: &mut App) {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return;
    }
    if apply_motion(k, app) {
        return;
    }
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => app.nav_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.nav_move(-1),
        KeyCode::Char('h') | KeyCode::Left => app.nav_col_delta(-1),
        KeyCode::Char('l') | KeyCode::Right => app.nav_col_delta(1),
        KeyCode::Char('g') => app.nav_top(),
        KeyCode::Char('G') => app.nav_bottom(),
        KeyCode::Char('y') | KeyCode::Enter => {
            app.yank_selection();
            app.enter_input();
        }
        KeyCode::Tab | KeyCode::Esc => {
            app.mode = Mode::Navigate;
            app.sel = None;
        }
        KeyCode::Char('[') => app.nav_jump_turn(-1),
        KeyCode::Char(']') => app.nav_jump_turn(1),
        KeyCode::PageUp => app.page_up(),
        KeyCode::PageDown => app.page_down(),
        _ => {}
    }
}

fn handle_mouse(m: MouseEvent, app: &mut App) {
    let in_log = m.row >= app.log_rect.y
        && m.row < app.log_rect.y + app.log_rect.height
        && m.column >= app.log_rect.x
        && m.column < app.log_rect.x + app.log_rect.width;
    // Mouse drag-selection is an Input-mode convenience; Navigate/Select use
    // the keyboard cursor. The wheel scrolls in every mode.
    let can_select = app.mode == Mode::Input;
    match m.kind {
        MouseEventKind::ScrollUp if in_log => app.scroll_nav(-3),
        MouseEventKind::ScrollDown if in_log => app.scroll_nav(3),
        MouseEventKind::Down(MouseButton::Left) if can_select => {
            app.sel = None;
            if in_log {
                let cell = log_cell(app, m.row, m.column);
                app.sel = Some(Selection { start: cell, end: cell });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if in_log && can_select => {
            let cell = log_cell(app, m.row, m.column);
            if let Some(sel) = app.sel.as_mut() {
                sel.end = cell;
            }
        }
        MouseEventKind::Up(MouseButton::Left) if can_select => app.yank_selection(),
        _ => {}
    }
}

/// Map a screen cell inside the log viewport to (select-line index, char
/// index) in `log_lines`, using display width so wide chars land correctly.
/// The char index is clamped to the line's content range so a press/drag in
/// the gutter or padding snaps to the content edge — the selection never
/// starts or ends in the decorative whitespace.
fn log_cell(app: &App, row: u16, column: u16) -> (usize, usize) {
    let rel_y = row.saturating_sub(app.log_rect.y) as usize;
    let rel_x = column.saturating_sub(app.log_rect.x) as usize;
    let line_idx = app.log_off.saturating_add(rel_y);
    // `log_lines` is the visible window only (window-relative); index by `rel_y`.
    let line = app.log_lines.get(rel_y);
    let col = line.map_or(rel_x, |s| {
        let (cstart, cend) = app.log_content.get(rel_y).copied().unwrap_or((0, s.chars().count()));
        col_to_char_idx(s, rel_x).clamp(cstart, cend)
    });
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

    /// A numbered `read` line whose body is empty must still carry its line
    /// number as decoration with an empty content range, so the Navigate
    /// cursor overlay preserves it instead of treating it as a blank line.
    #[test]
    fn numbered_empty_body_line_keeps_its_number() {
        use crate::tui::view::blocks::render_turn_lines;
        use crate::tui::view::component::Cx;
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::ToolStart {
            id: "e1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolInput {
            id: "e1".to_string(),
            code: "lofi.read('a.txt')".to_string(),
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
            result: "line one\n\nline three".to_string(),
            is_error: false,
        });
        a.apply_event(AgentEvent::ToolEnd {
            id: "e1".to_string(),
            result: "{\"value\":null}".to_string(),
            is_error: false,
            elapsed_ms: 0,
        });
        let turn = &a.turns[0];
        let cx = Cx {
            app: &a,
            theme: a.theme,
            width: 80,
            active_turn: false,
        };
        let rls = render_turn_lines(&cx, turn);
        // The empty-body numbered line keeps its decoration: content range
        // collapses to (cstart, cstart) but the line still has spans, so the
        // Navigate cursor overlay preserves it instead of replacing it.
        let empty = rls
            .iter()
            .find(|rl| rl.content.0 > 0 && rl.content.0 == rl.content.1 && !rl.line.spans.is_empty())
            .expect("empty-body numbered line should keep its decoration");
        let s: String = empty.line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(s.contains(" 2 ") || s.contains(" 2"), "number preserved: {s:?}");
    }

    #[test]
    fn current_line_text_excludes_decoration() {
        let mut a = app();
        push_turn(&mut a);
        a.log_off = 0;
        a.log_lines = vec!["  hello world   ".to_string()];
        a.log_content = vec![(2, 13)]; // "hello world"
        a.nav_cursor = 0;
        assert_eq!(a.current_line_text().as_deref(), Some("hello world"));
    }

    #[test]
    fn vim_motions_move_within_content() {
        let mut a = app();
        push_turn(&mut a);
        a.log_off = 0;
        a.log_lines = vec!["  aa bb cc".to_string()];
        a.log_content = vec![(2, 10)]; // "aa bb cc"
        a.nav_cursor = 0;
        a.nav_col = 2;
        // ^ and 0 land on the first content char.
        assert_eq!(a.first_nonblank_col(), 2);
        // $ lands on the last content char.
        a.nav_set_col(usize::MAX);
        assert_eq!(a.nav_col, 9);
        // w from "aa" -> "bb" start.
        a.nav_col = 2;
        assert_eq!(a.nav_word_target(WordMotion::NextStart { big: false }), 5);
        // e from "aa" -> end of "aa".
        a.nav_col = 2;
        assert_eq!(a.nav_word_target(WordMotion::NextEnd { big: false }), 3);
        // b from "bb" -> "aa" start.
        a.nav_col = 5;
        assert_eq!(a.nav_word_target(WordMotion::PrevStart { big: false }), 2);
    }

    #[test]
    fn vim_word_motion_skips_punctuation() {
        let mut a = app();
        push_turn(&mut a);
        a.log_off = 0;
        a.log_lines = vec!["  a.b c".to_string()];
        a.log_content = vec![(2, 7)]; // "a.b c"
        a.nav_cursor = 0;
        a.nav_col = 2;
        // w from "a" lands on "." (punctuation is its own word).
        assert_eq!(a.nav_word_target(WordMotion::NextStart { big: false }), 3);
        // W from "a" lands on "c" (only whitespace separates, so "a.b" is one WORD).
        assert_eq!(a.nav_word_target(WordMotion::NextStart { big: true }), 6);
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

    /// Build a `Vec<SessionEvent>` from kinds, assigning fresh ids and
    /// chaining each event's `parent_id` to the previous one (root = first).
    /// Mirrors what `store::append_events` does on disk, so
    /// `messages_from_events` / `replay_session_events` see a valid tree.
    fn sev_chain<I: IntoIterator<Item = SessionEventKind>>(kinds: I) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        let mut parent: Option<String> = None;
        for (i, kind) in kinds.into_iter().enumerate() {
            let id = format!("e{i}");
            out.push(SessionEvent {
                id: id.clone(),
                parent_id: parent.clone(),
                kind,
            });
            parent = Some(id);
        }
        out
    }

    /// Wrap a message as a `Message` session-event kind.
    fn msg(m: Message) -> SessionEventKind {
        SessionEventKind::Message(m)
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
            label: "m".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        // TurnEnd accumulates into the footer totals (input + output).
        assert_eq!(a.total_in, 10);
        assert_eq!(a.total_out, 20);
    }

    #[test]
    fn round_usage_updates_totals_per_round() {
        let mut a = app();
        a.apply_event(AgentEvent::TurnStart { prompt: "p".into() });
        // Two rounds within one turn: each carries the turn's cumulative
        // cost and that round's usage.
        a.apply_event(AgentEvent::RoundUsage {
            cost: 0.01,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        assert_eq!(a.total_in, 100);
        assert_eq!(a.total_out, 50);
        assert_eq!(a.turn_cost, 0.01);
        assert!(a.turn_has_round_usage);
        // Footer shows the live running cost (base cost + turn_cost).
        assert!((a.cost + a.turn_cost - 0.01).abs() < 1e-9);

        a.apply_event(AgentEvent::RoundUsage {
            cost: 0.03,
            usage: Usage {
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        // Tokens accumulate per round; turn_cost is replaced with the
        // turn's new cumulative cost.
        assert_eq!(a.total_in, 300);
        assert_eq!(a.total_out, 130);
        assert!((a.turn_cost - 0.03).abs() < 1e-9);
        assert!((a.cost + a.turn_cost - 0.03).abs() < 1e-9);

        // TurnEnd folds turn_cost into cost and does NOT re-add tokens.
        a.apply_event(AgentEvent::TurnEnd {
            label: "m".into(),
            elapsed_ms: 0,
            cost: 0.03,
            usage: Usage {
                input_tokens: 200,
                output_tokens: 80,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        assert!((a.cost - 0.03).abs() < 1e-9);
        assert_eq!(a.turn_cost, 0.0);
        assert!(!a.turn_has_round_usage);
        // Tokens unchanged: TurnEnd skipped re-accumulation.
        assert_eq!(a.total_in, 300);
        assert_eq!(a.total_out, 130);
    }

    #[test]
    fn turn_end_folds_bundled_totals_on_resume_path() {
        // The resume path has no RoundUsage events, so TurnEnd must apply
        // its bundled cost/usage as before.
        let mut a = app();
        a.apply_event(AgentEvent::TurnStart { prompt: "p".into() });
        a.apply_event(AgentEvent::TurnEnd {
            label: "m".into(),
            elapsed_ms: 0,
            cost: 0.05,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        });
        assert!((a.cost - 0.05).abs() < 1e-9);
        assert_eq!(a.total_in, 10);
        assert_eq!(a.total_out, 20);
        assert_eq!(a.turn_cost, 0.0);
        assert!(!a.turn_has_round_usage);
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
    fn slash_complete_filters_and_accepts() {
        let mut a = app();
        // "/" matches all commands.
        a.input = "/".to_string();
        a.refresh_slash_complete();
        let sc = a.slash_complete.as_ref().expect("popover open");
        assert_eq!(sc.candidates.len(), SLASH_COMMANDS.len());
        // "/tr" filters to just /tree.
        a.input = "/tr".to_string();
        a.refresh_slash_complete();
        let sc = a.slash_complete.as_ref().expect("popover open");
        assert_eq!(sc.candidates, vec![7]); // /tree is index 7
        // Typing the full command dismisses (nothing left to complete).
        a.input = "/tree".to_string();
        a.refresh_slash_complete();
        assert!(a.slash_complete.is_none());
        // Non-command input dismisses.
        a.input = "hello".to_string();
        a.refresh_slash_complete();
        assert!(a.slash_complete.is_none());
        // Accept replaces the input with the selected candidate.
        a.input = "/".to_string();
        a.refresh_slash_complete();
        a.slash_complete_down(); // index 1 = /exit
        a.slash_complete_down(); // index 2 = /help
        a.slash_complete_accept();
        assert_eq!(a.input, "/help");
        assert_eq!(a.input_cursor, a.input.len());
        assert!(a.slash_complete.is_none());
    }

    #[test]
    fn slash_complete_enter_accepts_tab_wraps() {
        let mut a = app();
        let mut run = None;
        a.input = "/".to_string();
        a.refresh_slash_complete();
        let len = a.slash_complete.as_ref().unwrap().candidates.len();
        // Tab cycles forward with wrap-around: after `len` presses we're
        // back at the first candidate (/clear, index 0 in SLASH_COMMANDS).
        for _ in 0..len {
            handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
        }
        assert_eq!(a.slash_complete.as_ref().unwrap().selected, 0);
        // Enter accepts the selection (auto-completes), replacing the input.
        handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);
        assert_eq!(a.input, "/clear");
        assert!(a.slash_complete.is_none());
    }

    #[test]
    fn slash_complete_single_item_tab_accepts() {
        // "/tr" matches only /tree, so Tab/Shift+Tab accept it outright
        // instead of cycling (a no-op on a single candidate).
        let mut a = app();
        let mut run = None;
        a.input = "/tr".to_string();
        a.refresh_slash_complete();
        assert_eq!(a.slash_complete.as_ref().unwrap().candidates.len(), 1);
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
        assert_eq!(a.input, "/tree");
        assert!(a.slash_complete.is_none());
        // Same for Shift+Tab on a single candidate.
        a.input = "/tr".to_string();
        a.refresh_slash_complete();
        handle_event(&plain_key(KeyCode::BackTab), &mut a, None, &mut run);
        assert_eq!(a.input, "/tree");
        assert!(a.slash_complete.is_none());
    }

    #[test]
    fn tree_no_session_pushes_error() {
        let mut a = app();
        // No session.path set — ephemeral. /tree rejects with an error turn
        // and leaves no overlay.
        assert!(a.slash_command("/tree"));
        assert!(a.tree_picker.is_none());
        assert!(matches!(a.turns.last().unwrap().blocks[0], Block::Error(_)));
        assert!(a.branch_hint.is_none());
    }

    #[test]
    fn tree_opens_rolls_back_and_prefills_prompt() {
        // Build a two-turn session: user1 → assistant1 → turn_end1 →
        // user2 → assistant2 → turn_end2. The picker should offer three
        // branch points: "after turn 1" (turn_end1), "edit turn 2"
        // (user2, prefilled), and "after turn 2" (turn_end2). Confirming
        // the "edit turn 2" entry rolls the transcript back to turn 1,
        // prefills the input with user2's text, and sets the branch hint
        // to turn_end1's id (so the resend is a sibling of user2).
        use lofi_core::session::store::SessionStore;
        use lofi_types::{ContentBlock, Role};
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("s"));
        let path = store.create(std::path::Path::new("/x"), "m").unwrap();
        let kinds = [
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "first".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "hello".into() }],
            }),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 100,
                cost: 0.0,
                usage: Usage::default(),
            },
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "second".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "world".into() }],
            }),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 100,
                cost: 0.0,
                usage: Usage::default(),
            },
        ];
        let mut batch: Vec<SessionEvent> = kinds
            .into_iter()
            .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
            .collect();
        store::append_events(&path, &mut batch, None).unwrap();
        // Read back the ids so the test can assert against them.
        let (_meta, events, _o, _s) = store::load(&path).unwrap();
        let turn_end1_id = events[2].id.clone();

        let mut a = app();
        a.session.path = Some(path);
        a.session.cwd = std::path::PathBuf::from("/x");
        assert!(a.slash_command("/tree"));
        let picker = a.tree_picker.as_ref().expect("picker opened");
        // Tree: user1, agent1 (turn_end1), user2, agent2 (turn_end2).
        assert_eq!(picker.entries.len(), 4);
        assert_eq!(picker.selected, 3); // defaults to the last entry
        // Find the "edit turn 2" entry (prefill = "second").
        let edit_idx = picker
            .entries
            .iter()
            .position(|e| e.prefill == "second")
            .unwrap();
        a.tree_picker.as_mut().unwrap().selected = edit_idx;
        a.tree_picker_confirm();
        // Confirm rolls back: the visible turns drop to just turn 1
        // (user "first" + assistant "hello" + turn_end), the input is
        // prefilled with "second", and the branch hint is turn_end1's id.
        assert!(a.tree_picker.is_none());
        assert_eq!(a.input, "second");
        assert_eq!(a.branch_hint.as_deref(), Some(turn_end1_id.as_str()));
        // One visible turn (turn 1); turn 2 is rolled back out of view.
        assert_eq!(a.turns.len(), 1);
        assert_eq!(a.history.lock().unwrap().len(), 2); // user1 + assistant1
    }

    #[test]
    fn modal_tab_cycles_with_wraparound() {
        // /tree on a two-turn session yields 4 entries, defaulting to the
        // last (index 3). Tab wraps forward (3 -> 0); Shift+Tab wraps back
        // (0 -> 3); Ctrl+N/Ctrl+P clamp at the edges.
        use lofi_core::session::store::SessionStore;
        use lofi_types::{ContentBlock, Role};
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("s"));
        let path = store.create(std::path::Path::new("/x"), "m").unwrap();
        let kinds = [
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "first".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "hello".into() }],
            }),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 100,
                cost: 0.0,
                usage: Usage::default(),
            },
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "second".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "world".into() }],
            }),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 100,
                cost: 0.0,
                usage: Usage::default(),
            },
        ];
        let mut batch: Vec<SessionEvent> = kinds
            .into_iter()
            .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
            .collect();
        store::append_events(&path, &mut batch, None).unwrap();
        let mut a = app();
        a.session.path = Some(path);
        a.session.cwd = std::path::PathBuf::from("/x");
        assert!(a.slash_command("/tree"));
        let len = a.tree_picker.as_ref().unwrap().entries.len();
        assert_eq!(len, 4);
        assert_eq!(a.tree_picker.as_ref().unwrap().selected, 3);
        let mut run = None;
        // Tab wraps forward: last (3) -> first (0).
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
        assert_eq!(a.tree_picker.as_ref().unwrap().selected, 0);
        // Shift+Tab wraps back: first (0) -> last (3).
        handle_event(&plain_key(KeyCode::BackTab), &mut a, None, &mut run);
        assert_eq!(a.tree_picker.as_ref().unwrap().selected, 3);
        // Ctrl+N moves forward (clamped, no wrap): 0 -> 1.
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
        assert_eq!(a.tree_picker.as_ref().unwrap().selected, 0);
        handle_event(
            &Event::Key(crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('n'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )),
            &mut a,
            None,
            &mut run,
        );
        assert_eq!(a.tree_picker.as_ref().unwrap().selected, 1);
    }

    #[test]
    fn tree_revert_to_root_then_reopens() {
        // Reverting to the first user prompt (root, no parent) sets
        // branch_hint to "" — the active path is empty. Reopening /tree
        // must still show every turn as an unhighlighted branch, not
        // "no branch points in this session yet".
        use lofi_core::session::store::SessionStore;
        use lofi_types::{ContentBlock, Role};
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("s"));
        let path = store.create(std::path::Path::new("/x"), "m").unwrap();
        let kinds = [
            SessionEventKind::Message(Message {
                role: Role::System,
                blocks: vec![ContentBlock::Text { text: "sys".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "first".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "hello".into() }],
            }),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 100,
                cost: 0.0,
                usage: Usage::default(),
            },
            SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "second".into() }],
            }),
            SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "world".into() }],
            }),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 100,
                cost: 0.0,
                usage: Usage::default(),
            },
        ];
        let mut batch: Vec<SessionEvent> = kinds
            .into_iter()
            .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
            .collect();
        store::append_events(&path, &mut batch, None).unwrap();

        let mut a = app();
        a.session.path = Some(path.clone());
        a.session.cwd = std::path::PathBuf::from("/x");
        // First /tree: select the root user prompt (entry 0) and revert.
        // Its branch_point is its parent (the system message), so the
        // active path becomes just the system message — the transcript is
        // empty (no visible turns) but branch_hint is the system id.
        assert!(a.slash_command("/tree"));
        a.tree_picker.as_mut().unwrap().selected = 0;
        a.tree_picker_confirm();
        assert!(a.branch_hint.is_some());
        assert_eq!(a.turns.len(), 0); // rolled back to before any user turn
        assert_eq!(a.input, "first");
        // Reopen /tree: all four nodes must appear, none active.
        assert!(a.slash_command("/tree"));
        let picker = a.tree_picker.as_ref().expect("picker reopened");
        assert_eq!(picker.entries.len(), 4);
        assert!(picker.entries.iter().all(|e| !e.is_active));
    }

    #[test]
    fn verbose_toggles() {
        let mut a = app();
        assert!(!a.verbose);
        let before = a.turns.len();
        a.toggle_verbose();
        assert!(a.verbose);
        // Verbose state surfaces on the rule line, not as a chat turn.
        assert_eq!(a.turns.len(), before);
        a.toggle_verbose();
        assert!(!a.verbose);
        assert_eq!(a.turns.len(), before);
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
            label: "m".into(),
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
        assert!(r.contains("context 40k/200k"), "footer: {r}");
    }

    #[test]
    fn thinking_timing_is_restored_from_transcript() {
        let assistant_with_thinking = Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Thinking {
                    text: "hm".to_string(),
                    signature: None,
                },
                ContentBlock::Text {
                    text: "ok".to_string(),
                },
            ],
        };
        let events = sev_chain([
            msg(user("hi")),
            msg(assistant_with_thinking),
            SessionEventKind::ThinkingTiming { elapsed_ms: 1234 },
        ]);
        let turns = turns_from_session_events(&events);
        assert_eq!(turns.len(), 1);
        let thinking = turns[0]
            .blocks
            .iter()
            .find_map(|b| match b {
                Block::Thinking(t) => Some(t),
                _ => None,
            })
            .expect("thinking block");
        assert_eq!(thinking.elapsed, Some(Duration::from_millis(1234)));
    }

    #[test]
    fn turns_from_events_round_trip() {
        let events = sev_chain([
            msg(user("hello")),
            msg(assistant("hi there")),
            msg(user("again")),
            msg(assistant("yep")),
        ]);
        let turns = turns_from_session_events(&events);
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
        let events: Vec<SessionEvent> = sev_chain(messages.into_iter().map(msg));
        let turns = turns_from_session_events(&events);
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
        let events = sev_chain([
            msg(user("run it")),
            msg(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"code": "return 1"}),
                }],
            }),
            msg(Message {
                role: Role::User,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".to_string(),
                    content: "1".to_string(),
                    is_error: false,
                }],
            }),
            msg(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text: "done".to_string() }],
            }),
            SessionEventKind::ToolTiming { tool_call_id: "t1".into(), elapsed_ms: 7 },
            SessionEventKind::TurnEnd {
                label: "proxy/gemini-3-flash · medium".into(),
                elapsed_ms: 2000,
                cost: 0.0,
                usage: Usage::default(),
            },
        ]);
        let turns = turns_from_session_events(&events);
        assert_eq!(turns.len(), 1);
        let blocks = &turns[0].blocks;
        // Tool elapsed is restored from the ToolTiming event.
        let Block::Tool(t) = &blocks[0] else { unreachable!() };
        assert_eq!(t.elapsed, Some(Duration::from_millis(7)));
        // The trailing block is the restored turn-end marker.
        match blocks.last() {
            Some(Block::TurnEnd { label, elapsed }) => {
                assert_eq!(label, "proxy/gemini-3-flash · medium");
                assert_eq!(*elapsed, Duration::from_secs(2));
            }
            other => panic!("expected TurnEnd, got {other:?}"),
        }
    }

#[test]
    fn messages_from_events_excludes_failed_turn_branch() {
        // Build a tree: root chain [user1, assistant1, TurnEnd1], then a
        // failed turn chained linearly off TurnEnd1: [user2, assistant2,
        // TurnFailed]. The TurnFailed marker is the active leaf. The active
        // path INCLUDES the failed turn's messages (so the UI can render
        // them), but `messages_from_events` must EXCLUDE them from the
        // agent's history via the TurnFailed boundary — the model resumes
        // from the checkpoint (TurnEnd1), not the failed partial content.
        use lofi_core::session::store::{active_path_from_leaf, last_event_id};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(&path, "{\"type\":\"meta\",\"version\":2,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n").unwrap();

        // First (successful) turn: two messages + a TurnEnd, chained from
        // the root so append_events assigns linear ids.
        let mut t1_events: Vec<SessionEvent> = [
            SessionEventKind::Message(user("hi")),
            SessionEventKind::Message(assistant("hello")),
            SessionEventKind::TurnEnd {
                label: "m".into(),
                elapsed_ms: 10,
                cost: 0.0,
                usage: Usage::default(),
            },
        ]
        .into_iter()
        .map(|kind| SessionEvent { id: String::new(), parent_id: None, kind })
        .collect();
        store::append_events(&path, &mut t1_events, None).unwrap();
        let checkpoint = last_event_id(&path).unwrap().unwrap();

        // Second (failed) turn: messages + TurnFailed marker, all chained
        // linearly off the checkpoint (parent_hint = checkpoint), mirroring
        // the recorder's flush(Failed) which does NOT branch the marker.
        let mut t2_events: Vec<SessionEvent> = [
            SessionEventKind::Message(user("oops")),
            SessionEventKind::Message(assistant("partial")),
            SessionEventKind::TurnFailed {
                label: "m".into(),
                elapsed_ms: 5,
                error: "boom".into(),
                cost: 0.01,
                usage: Usage::default(),
            },
        ]
        .into_iter()
        .map(|kind| SessionEvent { id: String::new(), parent_id: None, kind })
        .collect();
        store::append_events(&path, &mut t2_events, Some(&checkpoint)).unwrap();

        let (_meta, events, _, _) = store::load(&path).unwrap();
        // The active leaf is the TurnFailed marker; the active path includes
        // the failed turn's messages (they're ancestors of TurnFailed).
        let path_idx = active_path_from_leaf(&events);
        assert_eq!(path_idx.len(), 6, "active path includes failed turn's msgs");
        assert!(matches!(&events[path_idx[0]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "hi")));
        assert!(matches!(&events[path_idx[1]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant));
        assert!(matches!(&events[path_idx[2]].kind, SessionEventKind::TurnEnd { .. }));
        assert!(matches!(&events[path_idx[3]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "oops")));
        assert!(matches!(&events[path_idx[4]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant));
        assert!(matches!(&events[path_idx[5]].kind, SessionEventKind::TurnFailed { .. }));

        // messages_from_events yields only the checkpoint's messages,
        // excluding the failed turn's messages via the TurnFailed boundary.
        let msgs = messages_from_events(&events);
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[1].role, Role::Assistant);
    }    #[test]
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
            "proxy/deepseek-v4-flash".to_string(),
            ThinkingLevel::Off,
            200_000,
        );
        push_turn(&mut a);
        a.apply_event(AgentEvent::TurnEnd {
            label: "m".into(),
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
        assert!(footer.contains("context 2M/200k"), "footer: {footer}");
        let cost: String = a
            .render_footer_cost()
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert!(cost.contains("$18.00"), "cost: {cost}");
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

    #[test]
    fn selection_text_is_content_aware() {
        let mut a = app();
        // Simulated rendered log lines paired with their content ranges
        // (as components now report them): the leading gutter/rails and the
        // trailing padding tail fall outside the content range, while the
        // content's own leading spaces (indentation) are inside it.
        a.log_off = 0;
        a.log_lines = vec![
            // gutter "  " + content "hello world" + padding "   "
            "  hello world   ".to_string(),
            // gutter "  " + rails "│ │ " + content "lofi-core…Agent {" + padding
            "  │ │ lofi-core/src/agent.rs:233:pub struct Agent {     ".to_string(),
            // gutter "  " + content "    let x = 1;" (indentation preserved!)
            "      let x = 1;".to_string(),
        ];
        a.log_content = vec![
            (2, 13),  // "hello world"
            (6, 51),  // "lofi-core/src/agent.rs:233:pub struct Agent {"
            (2, 16),  // "    let x = 1;"
        ];
        a.sel = Some(Selection { start: (0, 0), end: (2, 40) });
        assert_eq!(
            a.selection_text().as_deref(),
            Some("hello world\nlofi-core/src/agent.rs:233:pub struct Agent {\n    let x = 1;")
        );
    }

    fn ctrl_key(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new_with_kind(
            code,
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        ))
    }

    #[test]
    fn ctrl_k_at_end_of_buffer_does_nothing() {
        // Regression: C-k at the end of a buffer with no trailing newline used
        // to slice one past the end and panic (observed as the app "quitting").
        let mut a = app();
        a.set_input("hello".to_string());
        a.move_line_end();
        a.kill_line_end(false);
        assert_eq!(a.input, "hello");
        assert!(a.kill_ring.is_empty());
    }

    #[test]
    fn ctrl_k_kills_to_end_of_line_not_newline() {
        let mut a = app();
        a.set_input("hello world\nfoo".to_string());
        a.input_cursor = 0;
        a.kill_line_end(false);
        assert_eq!(a.input, "\nfoo");
        assert_eq!(a.kill_ring, "hello world");
    }

    #[test]
    fn ctrl_k_kills_trailing_newline_on_empty_remainder() {
        let mut a = app();
        a.set_input("foo\nbar".to_string());
        a.input_cursor = 0;
        a.move_line_end();
        a.kill_line_end(false);
        assert_eq!(a.input, "foobar");
    }

    #[test]
    fn ctrl_c_clears_nonempty_prompt() {
        let mut a = app();
        a.set_input("a draft".to_string());
        let mut run = None;
        handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
        assert_eq!(a.input, "");
        assert!(!a.should_quit);
    }

    #[test]
    fn ctrl_c_double_press_on_empty_quits() {
        let mut a = app();
        let mut run = None;
        handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
        assert!(!a.should_quit, "first C-c must not quit");
        handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
        assert!(a.should_quit, "second C-c must quit");
    }

    #[test]
    fn ctrl_c_single_press_on_empty_does_not_quit() {
        let mut a = app();
        let mut run = None;
        handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
        assert!(!a.should_quit);
    }

    #[test]
    fn scroll_up_enters_nav_and_clamps_cursor_to_bottom_edge() {
        let mut a = app();
        a.mode = Mode::Input;
        a.log_total = 20;
        a.log_view_h = 5;
        a.last_base = 15; // base = total - height
        a.pinned = true; // sitting at the bottom
        a.nav_cursor = 19; // bottom line
        a.scroll_nav(-3);
        assert_eq!(a.mode, Mode::Navigate);
        // Viewport moved up to [12, 16]; cursor fell below -> clamp to 16.
        assert_eq!(a.view_off(), 12);
        assert_eq!(a.nav_cursor, 16);
    }

    #[test]
    fn scroll_down_clamps_cursor_to_top_edge() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.log_total = 20;
        a.log_view_h = 5;
        a.last_base = 15;
        a.pinned = false;
        a.top_line = 10;
        a.nav_cursor = 10; // top of the viewport
        a.scroll_nav(3);
        // Viewport moved down to [13, 17]; cursor fell above -> clamp to 13.
        assert_eq!(a.view_off(), 13);
        assert_eq!(a.nav_cursor, 13);
    }

    #[test]
    fn scroll_keeps_cursor_when_still_visible() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.log_total = 20;
        a.log_view_h = 5;
        a.last_base = 15;
        a.pinned = false;
        a.top_line = 10;
        a.nav_cursor = 12; // middle of [10, 14]
        a.scroll_nav(1);
        assert_eq!(a.view_off(), 11);
        assert_eq!(a.nav_cursor, 12); // still inside [11, 15]
    }

    #[test]
    fn scroll_at_boundary_leaves_mode_untouched() {
        let mut a = app();
        a.mode = Mode::Input;
        a.log_total = 20;
        a.log_view_h = 5;
        a.last_base = 15;
        a.pinned = true; // already at the bottom
        a.scroll_nav(3); // scrolling down does nothing
        assert_eq!(a.mode, Mode::Input);
    }

    #[test]
    fn ctrl_c_on_empty_shows_quit_badge() {
        let mut a = app();
        let mut run = None;
        handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
        assert_eq!(a.quit_badge(), Some("Press Ctrl-C again to quit"));
        // A non-empty draft clears the quit window, dropping the badge.
        a.set_input("draft".to_string());
        handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
        assert_eq!(a.quit_badge(), None);
    }

    #[test]
    fn ctrl_d_deletes_char_or_quits_on_empty() {
        let mut a = app();
        a.set_input("ab".to_string());
        a.input_cursor = 0;
        let mut run = None;
        handle_event(&ctrl_key(KeyCode::Char('d')), &mut a, None, &mut run);
        assert_eq!(a.input, "b");
        assert!(!a.should_quit);
        // Empty prompt -> EOF -> quit.
        let mut b = app();
        handle_event(&ctrl_key(KeyCode::Char('d')), &mut b, None, &mut run);
        assert!(b.should_quit);
}

    fn plain_key(code: KeyCode) -> Event {
        Event::Key(crossterm::event::KeyEvent::new_with_kind(
            code,
            KeyModifiers::empty(),
            KeyEventKind::Press,
        ))
    }

    #[test]
    fn tab_enters_navigate_and_esc_clears() {
        let mut a = app();
        let mut run = None;
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
        assert_eq!(a.mode, Mode::Navigate);
        // Esc on a non-empty prompt just clears it (no mode switch).
        let mut b = app();
        b.set_input("draft".to_string());
        handle_event(&plain_key(KeyCode::Esc), &mut b, None, &mut run);
        assert_eq!(b.mode, Mode::Input);
        assert_eq!(b.input, "");
    }

    #[test]
    fn navigate_jk_moves_cursor_and_clamps() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.log_total = 10;
        a.log_view_h = 4;
        a.nav_cursor = 9;
        a.nav_move(-3);
        assert_eq!(a.nav_cursor, 6);
        // Clamp at the top.
        a.nav_move(-100);
        assert_eq!(a.nav_cursor, 0);
        // Clamp at the bottom.
        a.nav_move(100);
        assert_eq!(a.nav_cursor, 9);
    }

    #[test]
    fn select_sel_is_charwise_inclusive() {
        let mut a = app();
        a.mode = Mode::Select;
        a.select_anchor = (2, 3);
        a.nav_cursor = 5;
        a.nav_col = 6;
        let sel = a.select_sel();
        // Max end is exclusive (+1) so the cursor char is included.
        assert_eq!(sel.start, (2, 3));
        assert_eq!(sel.end, (5, 7));
        // Cursor left of anchor: same span, min becomes start.
        a.nav_cursor = 1;
        a.nav_col = 1;
        let sel = a.select_sel();
        assert_eq!(sel.start, (1, 1));
        assert_eq!(sel.end, (2, 4));
    }

    #[test]
    fn v_enters_select_and_extends_selection() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.log_total = 10;
        a.log_view_h = 4;
        a.nav_cursor = 3;
        let mut run = None;
        handle_event(&plain_key(KeyCode::Char('v')), &mut a, None, &mut run);
        assert_eq!(a.mode, Mode::Select);
        // Empty selection at the cursor (anchor == cursor); +1 makes the end
        // exclusive, so it spans one char once clamped to the content range.
        assert_eq!(a.sel.as_ref().unwrap().start, (3, 0));
        assert_eq!(a.sel.as_ref().unwrap().end, (3, 1));
        // Move down: selection extends to the next line, column preserved.
        handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 4);
        assert_eq!(a.sel.as_ref().unwrap().start, (3, 0));
        assert_eq!(a.sel.as_ref().unwrap().end, (4, 1));
        // Tab drops selection, back to Navigate.
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
        assert_eq!(a.mode, Mode::Navigate);
        assert!(a.sel.is_none());
    }

    #[test]
    fn esc_discards_select_back_to_nav() {
        // From Select, Esc returns to Navigate and clears the selection —
        // same as Tab, but more conventional for drop-without-yank.
        let mut a = app();
        a.mode = Mode::Navigate;
        a.log_total = 10;
        a.log_view_h = 4;
        a.nav_cursor = 3;
        let mut run = None;
        handle_event(&plain_key(KeyCode::Char('v')), &mut a, None, &mut run);
        assert_eq!(a.mode, Mode::Select);
        handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
        assert!(a.sel.is_some());
        handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
        assert_eq!(a.mode, Mode::Navigate);
        assert!(a.sel.is_none());
    }

    #[test]
    fn turn_start_line_accounts_for_blanks() {
        let mut a = app();
        a.frozen_heights = vec![3, 2];
        a.last_turn_height = 4;
        a.turns.clear();
        for _ in 0..3 {
            push_turn(&mut a);
        }
        assert_eq!(a.turn_start_line(0), 0);
        // turn0 (3) + blank (1) = 4.
        assert_eq!(a.turn_start_line(1), 4);
        // + turn1 (2) + blank (1) = 7.
        assert_eq!(a.turn_start_line(2), 7);
    }

    #[test]
    fn bracket_jumps_between_turns() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.frozen_heights = vec![3, 2];
        a.last_turn_height = 4;
        a.log_total = 3 + 1 + 2 + 1 + 4;
        a.log_view_h = 10;
        a.turns.clear();
        for _ in 0..3 {
            push_turn(&mut a);
        }
        let mut run = None;
        a.nav_cursor = 1;
        handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 4);
        handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 7);
        // At the last turn, `]` stays at its start.
        handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 7);
        handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 4);
        handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 0);
        // At the first turn, `[` stays at 0.
        handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 0);
    }

    #[test]
    fn page_keys_move_cursor_in_nav() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.log_total = 40;
        a.log_view_h = 10;
        a.nav_cursor = 20;
        let mut run = None;
        handle_event(&plain_key(KeyCode::PageDown), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 30);
        handle_event(&plain_key(KeyCode::PageUp), &mut a, None, &mut run);
        assert_eq!(a.nav_cursor, 20);
    }

    #[test]
    fn page_down_in_input_pins_at_bottom() {
        let mut a = app();
        a.mode = Mode::Input;
        a.log_total = 40;
        a.log_view_h = 10;
        a.last_base = 30;
        a.top_line = 0;
        let mut run = None;
        handle_event(&plain_key(KeyCode::PageDown), &mut a, None, &mut run);
        assert_eq!(a.top_line, 10);
        assert!(!a.pinned);
        for _ in 0..3 {
            handle_event(&plain_key(KeyCode::PageDown), &mut a, None, &mut run);
        }
        assert!(a.pinned);
        handle_event(&plain_key(KeyCode::PageUp), &mut a, None, &mut run);
        assert!(!a.pinned);
    }

    #[test]
    fn ctrl_c_in_nav_returns_to_input_pinned() {
        let mut a = app();
        a.mode = Mode::Navigate;
        a.last_base = 42;
        a.top_line = 5;
        a.pinned = false;
        let mut run = None;
        handle_event(
            &Event::Key(crossterm::event::KeyEvent::new_with_kind(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                KeyEventKind::Press,
            )),
            &mut a,
            None,
            &mut run,
        );
        assert_eq!(a.mode, Mode::Input);
        assert!(a.pinned);
        assert_eq!(a.top_line, 42);
    }

    #[test]
    fn insert_str_normalizes_line_endings() {
        let mut a = app();
        a.insert_str("a\r\nb\rc");
        assert_eq!(a.input, "a\nb\nc");
    }
}