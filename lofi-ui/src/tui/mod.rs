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

// Glob re-export so `view`, `tests`, and this module call the moved
// helpers by bare name; the submodules are cohesive slices of `tui`.
#[allow(clippy::wildcard_imports)]
use {input::*, replay::*, resume::*, text::*, tree::*};

use std::collections::{HashMap, VecDeque};
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
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

use base64::Engine;

pub(crate) mod theme;
use std::time::Instant;
pub(crate) use theme::Theme;
use tokio::sync::mpsc::Receiver;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::MissedTickBehavior;

use crate::tui::view::HStack;
use lofi_core::{compact, compacted_history, Agent, AgentEvent, CompactOptions, SessionCommit};
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

    /// The selectable models, in registry order.
    pub(crate) fn choices(&self) -> &[lofi_types::ModelChoice] {
        &self.choices
    }

    /// Rebuild the agent for `provider/model[:level]`, reusing `existing`'s
    /// per-session tmp dir when given.
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

/// Braille spinner frames, advanced on each tick while a run is active.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const TICK_MS: u64 = 60;
/// How long the "Copied to clipboard" badge stays on the footer rule.
const YANK_NOTIFY: Duration = Duration::from_secs(2);
/// How long a slash-command notification stays on the rule line.
const NOTIFY_TTL: Duration = Duration::from_secs(5);
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
    TurnEnd {
        label: String,
        elapsed: Duration,
    },
    /// Turn-failed rule: `<label> failed in Ns · <error>` in the error
    /// tint, appended when a run ends in a non-retryable error or is
    /// cancelled. The turn's partial messages precede it; the marker is the
    /// leaf of the failed branch.
    TurnFailed {
        label: String,
        elapsed: Duration,
        error: String,
    },
    /// An offline compaction marker: `◇ Compacted N messages · kept M` in the
    /// muted tint, appended to the current turn when `/compact` (or the
    /// auto-trigger) folds the older history into a summary. The summary
    /// text is carried along so `/verbose` can expand it inline; the default
    /// (collapsed) view shows only the one-line marker. The summary is also
    /// injected into the agent's history, not just the visible transcript.
    Compaction {
        summarized: usize,
        kept: usize,
        summary: String,
    },
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
    /// Lightweight tree/offset index loaded on resume. Message and tool bodies
    /// remain on disk and are parsed only for the active turn/history.
    index: Vec<store::EventIndex>,
    file_size: u64,
    cwd: PathBuf,
}

impl SessionConfig {
    /// No persistence (--no-session).
    pub(crate) fn ephemeral(cwd: PathBuf) -> Self {
        Self {
            store: None,
            path: None,
            index: Vec::new(),
            file_size: 0,
            cwd,
        }
    }

    /// A new session, created lazily on the first prompt.
    pub(crate) fn fresh(store: SessionStore, cwd: PathBuf) -> Self {
        Self {
            store: Some(store),
            path: None,
            index: Vec::new(),
            file_size: 0,
            cwd,
        }
    }

    /// Resume an existing transcript file.
    pub(crate) fn resumed(
        store: SessionStore,
        path: PathBuf,
        index: Vec<store::EventIndex>,
        file_size: u64,
        cwd: PathBuf,
    ) -> Self {
        Self {
            store: Some(store),
            path: Some(path),
            index,
            file_size,
            cwd,
        }
    }

    /// The raw model+thinking of the last completed turn on the active path,
    /// for restoring the model on resume (see [`store::last_run_model`]).
    /// `None` for fresh/ephemeral sessions or sessions with no completed turn.
    #[must_use]
    pub(crate) fn last_run_model(&self) -> Option<RunModel> {
        self.path
            .as_deref()
            .and_then(|p| last_run_model_from_index(p, &self.index))
    }
}

/// State for the '/resume' session-picker overlay.
#[derive(Debug, Clone)]
struct PickerState {
    entries: Vec<SessionEntry>,
    selected: usize,
}

/// A read-only, scrollable information modal (e.g. `/help`, `/session`
/// output): a centered box showing `title` over `lines`. Navigation uses
/// the modal keys — `↑/↓` or `j`/`k` (and `Ctrl+N`/`Ctrl+P`, `PgUp`/
/// `PgDn`) scroll; `y` copies the body; `Esc`/`q`/`Enter` dismiss.
///
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

/// Section header for an info modal: bold, accent-colored.
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

/// A plain muted note line (no key column, flush left).
fn info_note(t: Theme, text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::new().fg(t.muted)))
}

/// Format a byte count as a human-readable string (e.g. `1.2 KB`, `3.4 MB`).
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

/// Severity of a transient rule-line notification (see [`App::notify`]).
/// Maps to a background color: Info → muted, Warn → warn, Error → error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NotifyKind {
    Info,
    Warn,
    Error,
}

/// A transient slash-command notification shown on the rule line's left
/// edge. Auto-expires after [`NOTIFY_TTL`].
#[derive(Debug, Clone)]
struct Notify {
    msg: String,
    kind: NotifyKind,
    at: Instant,
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

/// State for the `/model` picker overlay. Owns a snapshot of the available
/// models ([`App::model_choices`] at open time) so navigation shares the
/// [`Modal`] dispatch with a correct `len`.
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

/// State for the `/thinking` picker overlay. Owns the levels offered for the
/// current model (`off` plus its declared `thinking_levels`, deduped) so
/// navigation shares the [`Modal`] dispatch with a correct `len`.
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

/// One row in the '/tree' picker. `branch_point` is the event id the next
/// run chains off (becomes the new turn's parent); `label` is the node text
/// (`user: ...` or `agent: ...`); `prefix` is the ASCII tree art (`|- `,
/// `` `- ``, `|  `, `   `); `prefill` is loaded into the input box on confirm
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

/// Viewport-local cache of rendered frozen turns, keyed by turn index.
/// Only turns intersecting the viewport (plus a one-turn margin) are retained;
/// the rest are re-rendered on demand. Heights for *all* frozen turns live in
/// `frozen_heights` (tiny), so the viewport can be located without retaining
/// the heavy styled-line representation of the whole session.
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

    /// Drop rendered turns outside the viewport-local working set. A one-turn
    /// margin on either side avoids re-rendering immediately on a small scroll.
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

/// The TUI's mutable state.
/// Mouse selection in the log, in select-line + char-index space (absolute
/// indices into `log_vis`).
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
    /// ":medium"-style suffix, or None when thinking is off.
    thinking_label: Option<String>,
    /// Current thinking level; the source of `thinking_label` and the
    /// pre-selection for the `/thinking` picker.
    thinking: ThinkingLevel,
    /// Most recent turn's usage, for the context-window gauge (input
    /// + output + cache read/write of the latest round = current fill).
    status_usage: Option<Usage>,
    total_in: u64,
    total_out: u64,
    /// Cumulative prompt-cache read tokens across the session.
    total_cache_read: u64,
    /// Cumulative prompt-cache write tokens across the session.
    total_cache_write: u64,
    /// Queued prompts waiting for the current run to finish. FIFO when
    /// auto-popping at turn end; LIFO when restoring via Alt+Up.
    prompt_queue: Vec<String>,
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
    /// Compaction configuration (from `[compaction]`): the reserve hard cap
    /// plus the speculative `[compaction.auto]` soft caps.
    compaction: lofi_types::CompactionConfig,
    /// Last observed context input-token count, for the auto-compaction
    /// hysteresis: the trigger fires only on the upward crossing of the
    /// threshold, not on every above-threshold turn. `None` until the
    /// first round reports usage, and reset to `None` after a compaction
    /// or a session rollback so the baseline re-evaluates cleanly.
    prev_ctx_tokens: Option<u64>,
    /// Message count at the time of the last compaction, used as a soft
    /// cooldown for auto-compaction so a tail too large to compact further
    /// is not re-compacted every turn (which would waste tokens for no
    /// benefit). Reset on rollback/resume.
    last_compact_msg_count: usize,
    /// Whether the session has been compacted at least once. Drives a `c`
    /// prefix on the context gauge so the user can tell the history is folded.
    compacted: bool,
    /// Set by `ContextPressure` when the engine force-stopped the run at the
    /// hard context cap. The run loop reads (and clears) it on channel close
    /// to drive the force-compact + silent continue, instead of the soft
    /// `agent_settled` path.
    context_pressure: bool,
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
    /// Debug event to sample after the next completed frame. `/verbose`
    /// schedules this so diagnostics capture the render/materialization cost,
    /// not merely the cheap boolean toggle that precedes it.
    debug_after_draw: Option<&'static str>,
    /// Opt-in process/component diagnostics writer enabled by `/debug`.
    debug: Option<debug_stats::DebugState>,
    should_quit: bool,
    session: SessionState,
    picker: Option<PickerState>,
    /// '/tree' overlay state, when open. See [`TreePickerState`].
    tree_picker: Option<TreePickerState>,
    /// `/model` overlay state, when open. See [`ModelPickerState`].
    model_picker: Option<ModelPickerState>,
    /// `/thinking` overlay state, when open. See [`ThinkingPickerState`].
    thinking_picker: Option<ThinkingPickerState>,
    /// The available models for `/model`, snapshot at startup from the
    /// retained registry. Empty when no provider has credentials.
    model_choices: Vec<lofi_types::ModelChoice>,
    /// A `/model` confirmation hands a `provider/model` query here; the run
    /// loop rebuilds the agent from the retained registry and clears it.
    pending_model_switch: Option<String>,
    /// Read-only information modal (e.g. `/session` output), when open.
    info: Option<InfoModal>,
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
    log_vis: Vec<view::VisLine>,
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
    /// Queue of pending shell-policy confirmation requests.
    /// The first item is shown as a centered modal; when the user
    /// responds, it is popped and the next one (if any) appears.
    pending_confirms: Vec<lofi_core::ConfirmRequest>,
    /// Selected action in the permission dialog: 0 = Allow, 1 = Deny.
    /// Navigation changes this; only Enter or an explicit action key resolves
    /// the request, so stray key presses can never reject a command.
    confirm_selected: usize,
    /// First wrapped command row visible in the permission dialog. Reset for
    /// each queued request and clamped by the renderer to its viewport.
    confirm_scroll: usize,
    /// Wrapped command row count and viewport height from the last render.
    confirm_total: usize,
    confirm_view_h: usize,
    /// When the yank-to-clipboard badge was last triggered; shown on the
    /// footer rule's left for a short window after a yank.
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
    /// restricted to turns intersecting the viewport plus a one-turn margin.
    /// Turns outside the cache are re-rendered on demand. The last turn is
    /// rebuilt fresh each frame; earlier turns are immutable once a new turn
    /// is pushed, so their height is stable and only their (heavy) styled
    /// lines are evictable.
    frozen_render: FrozenCache,
    /// Line count per frozen turn (all of them), so the viewport can be
    /// located and `total` computed without fetching rendered lines. Synced
    /// to `turns.len()-1` for the active verbose mode.
    frozen_heights: Vec<usize>,
    /// Height index for the inactive verbose mode. `/verbose` swaps this
    /// with `frozen_heights`, avoiding a full transcript reparse when
    /// collapsing or revisiting a mode that has already been measured.
    frozen_heights_other_mode: Vec<usize>,
    /// Bumped whenever `turns` is replaced wholesale (resume, `/new`,
    /// `/clear`); a mismatch with `frozen_epoch` discards the cache.
    render_epoch: u64,
    /// Epoch captured when `frozen_render` was last built.
    frozen_epoch: u64,
    /// Viewport width the frozen cache was last built at. A resize changes
    /// the wrap width, so a mismatch discards the cache just like an epoch
    /// bump — otherwise background-padded lines keep the old (narrower)
    /// width after the terminal grows.
    frozen_width: usize,
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
        path,
        index,
        file_size,
        cwd,
    } = session;
    let model_choices = switcher
        .as_ref()
        .map_or(Vec::new(), |s| s.choices().to_vec());
    let edit = compaction.edit.clone();
    let mut app = App::new(model_label, thinking, ctx_limit, compaction);
    app.model_choices = model_choices;
    app.session = SessionState { store, path, cwd };
    if let Some(path) = app.session.path.clone() {
        let messages = history_from_index(&path, &index, &edit)?;
        if let Ok(mut history) = app.history.lock() {
            *history = messages;
        }
        replay_indexed_session(&mut app, &path, &index, file_size)?;
        restore_compaction_from_index(&mut app, &path, &index);
    }
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

    // LOFI_DEBUG opts into the same diagnostics as /debug, but from process
    // startup so resume/replay and subsequent activity are logged without an
    // interactive command. Enable after session restoration so the first
    // sample describes the fully initialized application.
    app.enable_debug_from_env();

    // Create the confirmation channel for shell-policy `ask` decisions.
    // The agent sends ConfirmRequests; the TUI shows a yes/no prompt and
    // responds through the embedded oneshot.
    let (confirm_tx, mut confirm_rx) =
        tokio::sync::mpsc::unbounded_channel::<lofi_core::ConfirmRequest>();
    if let Some(a) = agent.take() {
        agent = Some(a.with_confirm_tx(confirm_tx));
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
                        app.apply_event(e);
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
                            r.handle.abort();
                            app.run_finished();
                            app.debug_sample("agent_settled");
                            if app.context_pressure {
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
                                // Auto-pop the next queued prompt (FIFO).
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
                match maybe_ev {
                    Some(Ok(ev)) => {
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
                // Slash-command notifications expire on their own.
                if let Some(n) = app.notify.as_ref() {
                    if n.at.elapsed() >= NOTIFY_TTL {
                        app.notify = None;
                    }
                    dirty = true;
                }
            }
            // Shell-policy confirmation request from the agent.
            req = confirm_rx.recv() => {
                if let Some(req) = req {
                    if app.pending_confirms.is_empty() {
                        app.confirm_selected = 0;
                        app.confirm_scroll = 0;
                        app.confirm_total = 0;
                        app.confirm_view_h = 0;
                    }
                    app.pending_confirms.push(req);
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
