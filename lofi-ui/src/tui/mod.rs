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
mod modals;
mod replay;
mod resume;
mod run;
mod text;
mod transcript;
use transcript::{Transcript, TranscriptLayout};
mod tree;
mod tty_events;
mod types;
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
#[allow(clippy::wildcard_imports)]
pub(crate) use modals::*;
#[allow(clippy::wildcard_imports)]
pub(crate) use run::*;
use std::time::Instant;
pub(crate) use theme::Theme;
use tokio::sync::mpsc::Receiver;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::MissedTickBehavior;
#[allow(clippy::wildcard_imports)]
pub(crate) use types::*;

use crate::tui::view::HStack;
use lofi_core::backend::Backend;
use lofi_core::{Agent, AgentEvent, AgentLifecycle, HardCompactOutcome};
use lofi_error::{Error, Result};

/// Native and external model choices share one picker. Native startup keeps
/// its resolved registry; external startup resolves credentials only when
/// a native model is selected.
pub(crate) struct ModelSwitcher {
    registry: lofi_core::ModelRegistry,
    config: lofi_types::Config,
    root: PathBuf,
    env: Vec<(String, String)>,
    choices: Vec<lofi_types::ModelChoice>,
    unresolved_config: Option<lofi_types::Config>,
    executor: Option<lofi_core::agent::ToolExecutor>,
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
            unresolved_config: None,
            executor: None,
        }
    }

    pub(crate) fn from_external(
        config: lofi_types::Config,
        root: PathBuf,
        env: Vec<(String, String)>,
    ) -> Result<Self> {
        let mut choices_config = config.clone();
        let _guard = lofi_core::config_loader::apply_env_specs(&env);
        for provider in choices_config.providers.values_mut() {
            if provider.api_key.is_none() {
                provider.api_key = provider
                    .env_name
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok());
            }
        }
        let registry = lofi_core::ModelRegistry::load(&choices_config)?;
        let mut switcher = Self::new(registry, config.clone(), root, env);
        switcher.unresolved_config = Some(config);
        Ok(switcher)
    }

    pub(crate) fn choices(&self) -> &[lofi_types::ModelChoice] {
        &self.choices
    }

    pub(crate) fn with_tool_executor(mut self, executor: lofi_core::agent::ToolExecutor) -> Self {
        self.executor = Some(executor);
        self
    }

    pub(crate) fn external(
        &self,
        existing: Option<&Backend>,
        query: &str,
    ) -> Result<Option<Backend>> {
        let Some((name, _)) = query.split_once('/') else {
            return Ok(None);
        };
        let Some(config) = self.config.backends.get(name) else {
            return Ok(None);
        };
        let mut backend = Backend::external(
            name.into(),
            config.clone(),
            self.root.clone(),
            existing
                .map(Backend::tool_executor)
                .or_else(|| self.executor.clone())
                .ok_or_else(|| Error::State("missing tool executor".into()))?,
        );
        backend.select_model(query)?;
        Ok(Some(backend))
    }

    pub(crate) async fn rebuild_backend(
        &self,
        existing: Option<&Backend>,
        query: &str,
    ) -> Result<(Backend, Option<lofi_types::Model>, ThinkingLevel)> {
        if let Some(backend) = self.external(existing, query)? {
            return Ok((backend, None, ThinkingLevel::Off));
        }
        let (agent, model, level) = if let Some(config) = &self.unresolved_config {
            let provider = query.split_once('/').map_or("", |(name, _)| name);
            let mut config = config.clone();
            config.providers.retain(|name, _| name == provider);
            let guard = lofi_core::config_loader::apply_env_specs(&self.env);
            lofi_core::config_loader::resolve_config(&mut config).await?;
            drop(guard);
            let registry = match lofi_core::ModelRegistry::load_async(&config).await {
                Ok(registry) => registry,
                Err(_) => lofi_core::ModelRegistry::load(&config)?,
            };
            lofi_core::rebuild_agent(
                existing.and_then(Backend::native),
                &registry,
                &config,
                Some(query),
                &self.root,
                &self.env,
            )?
        } else {
            self.rebuild(existing.and_then(Backend::native), query)?
        };
        let agent = if let Some(existing) = existing {
            agent
                .with_system_prompt(existing.system_prompt().into())
                .with_tool_executor(existing.tool_executor())
        } else {
            agent
        };
        Ok((agent.into(), Some(model), level))
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

#[allow(clippy::struct_excessive_bools)]
pub(crate) struct App {
    transcript: Transcript,
    layout: TranscriptLayout,
    input: String,
    input_cursor: usize,
    lifecycle: AgentLifecycle,
    external_backend: bool,
    /// The system prompt the agent was built with; pinned onto the durable
    /// transcript at each context boundary so a resumed or compacted history
    /// replays the exact governing prompt rather than what today's config
    /// would produce.
    system_prompt: String,
    history_nav: Vec<String>,
    history_idx: Option<usize>,
    input_stash: String,
    model_label: String,
    selected_external_model_label: Option<String>,
    thinking_label: Option<String>,
    thinking: ThinkingLevel,
    service_label: Option<String>,
    service_tier: ServiceTier,
    status_usage: Option<Usage>,
    backend_context: Option<lofi_types::ContextUsage>,
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
    run_selected_model_label: Option<String>,
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
    /// Ids of terminal jobs whose completion notice the run loop has queued
    /// or spawned. The registry flips a job to terminal before its notice
    /// reaches the select loop; the tail keeps rendering the running line
    /// until the notice lands so line and notice swap in one frame.
    notified_jobs: std::collections::HashSet<u64>,
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
        self.transcript.clear();
        self.expanded_details.clear();
        self.detail_focus = None;
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
