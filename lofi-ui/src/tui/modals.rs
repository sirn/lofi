#[allow(clippy::wildcard_imports)]
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct PickerEntry {
    pub(crate) file: store::SessionFile,
    pub(crate) preview: Option<String>,
    pub(crate) details: Option<SessionEntry>,
}

#[derive(Debug, Clone)]
pub(crate) struct PickerState {
    pub(crate) entries: Vec<PickerEntry>,
    pub(crate) selected: usize,
    pub(crate) generation: u64,
}

/// `scroll` is the top visible wrapped-line index; `total` and `view_h`
/// are filled by the renderer each frame so the key handler can clamp and
/// page without knowing the terminal size itself.
#[derive(Debug, Clone)]
pub(crate) struct InfoModal {
    pub(crate) title: String,
    pub(crate) lines: Vec<InfoLine>,
    pub(crate) scroll: usize,
    pub(crate) total: usize,
    pub(crate) view_h: usize,
}

#[derive(Debug, Clone)]
pub(crate) enum InfoLine {
    Section(String),
    Text(Line<'static>),
}

impl InfoModal {
    pub(crate) fn max_scroll(&self) -> usize {
        self.total.saturating_sub(self.view_h)
    }

    pub(crate) fn scroll_down(&mut self) {
        self.scroll = (self.scroll + 1).min(self.max_scroll());
    }

    pub(crate) fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    pub(crate) fn scroll_page_down(&mut self) {
        self.scroll = (self.scroll + self.view_h.max(1)).min(self.max_scroll());
    }

    pub(crate) fn scroll_page_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(self.view_h.max(1));
    }
}

pub(crate) fn info_section(label: &str) -> InfoLine {
    InfoLine::Section(label.to_string())
}

/// `key  value` row: key bold in `fg`, value muted, key padded to a fixed
/// column so the values line up. Longer keys just overflow the column.
pub(crate) fn info_kv(t: Theme, key: &str, value: &str) -> InfoLine {
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

pub(crate) fn info_note(t: Theme, text: &str) -> InfoLine {
    InfoLine::Text(Line::from(Span::styled(
        text.to_string(),
        Style::new().fg(t.muted),
    )))
}

pub(crate) fn format_bytes(n: u64) -> String {
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
pub(crate) struct Notify {
    pub(crate) msg: String,
    pub(crate) kind: NotifyKind,
    pub(crate) at: Instant,
}

/// `j`/`k`/`q` remain printable because this popover overlays text input.
#[derive(Debug, Clone)]
pub(crate) struct SlashComplete {
    pub(crate) candidates: Vec<usize>,
    pub(crate) selected: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct TreePickerState {
    pub(crate) entries: Vec<TreeEntry>,
    pub(crate) selected: usize,
    pub(crate) generation: u64,
    pub(crate) loading: bool,
}

/// Confirmation stays outside this trait because borrowing a modal and
/// `&mut App` together would conflict.
pub(crate) trait Modal {
    fn len(&self) -> usize;
    fn selected(&self) -> usize;
    fn set_selected(&mut self, n: usize);
}

pub(crate) trait Popover {
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

pub(crate) struct ModelPickerState {
    pub(crate) choices: Vec<lofi_types::ModelChoice>,
    pub(crate) selected: usize,
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

pub(crate) struct ThinkingPickerState {
    pub(crate) levels: Vec<ThinkingLevel>,
    pub(crate) selected: usize,
}

pub(crate) struct ServicePickerState {
    pub(crate) tiers: Vec<ServiceTier>,
    pub(crate) selected: usize,
}

pub(crate) struct ThemePickerState {
    pub(crate) modes: [lofi_types::ThemeMode; 3],
    pub(crate) selected: usize,
}

/// State for the `/policy` modal: the session-scoped bash approval modes
/// the user can pick. `AskAuto` is offered only when auto mode is
/// configured.
pub(crate) struct PolicyPickerState {
    pub(crate) modes: Vec<lofi_types::BashApprovalMode>,
    pub(crate) selected: usize,
}

impl ThemePickerState {
    pub(crate) const MODES: [lofi_types::ThemeMode; 3] = [
        lofi_types::ThemeMode::Auto,
        lofi_types::ThemeMode::Light,
        lofi_types::ThemeMode::Dark,
    ];
}

/// `/paths` modal. Lists every allowed path with its access and source.
/// Only state rows are editable: `space` toggles read/write, `d` forgets
/// the grant. Config rows say to edit `config.toml`; builtin rows are
/// fixed by the session.
pub(crate) struct PathsPickerState {
    pub(crate) rows: Vec<lofi_core::paths::PathRow>,
    pub(crate) selected: usize,
}

impl Modal for PathsPickerState {
    fn len(&self) -> usize {
        self.rows.len()
    }

    fn selected(&self) -> usize {
        self.selected
    }

    fn set_selected(&mut self, n: usize) {
        self.selected = n;
    }
}

/// The subagents tab. Lists the subagents spawned by the live session:
/// store rows (durable, survive restarts) merged with the registry's live
/// overlay (in-flight runs). `Enter` opens a read-only transcript view.
pub(crate) struct SubagentsModalState {
    pub(crate) selected: usize,
    /// Rows loaded at open and on `r`; store order (newest first), then live
    /// rows not yet on disk.
    pub(crate) rows: Vec<SubagentRow>,
}

/// One `/subagent` list row. `file` is present for settled runs with a
/// transcript on disk; live-only rows carry the registry's file name.
pub(crate) struct SubagentRow {
    pub(crate) id: String,
    pub(crate) running: bool,
    /// The run's one-line label: its task while live, its last answer once a
    /// store row stands in for it.
    pub(crate) preview: String,
    /// Transcript file name (`<millis>_<rand>.jsonl`), when one exists.
    pub(crate) file: Option<String>,
    /// The run's model: the registry knows the one it started, and a row read
    /// from the store takes it from the transcript header.
    pub(crate) model: Option<String>,
    /// The run's spend: live from the registry, terminal from the settle
    /// marker a row read from the store keeps.
    pub(crate) cost: Option<f64>,
    /// Elapsed time of a live run, 0 for a settled one.
    pub(crate) elapsed_ms: u64,
}

impl SubagentRow {
    /// A row for a run the registry knows and the store has not listed yet.
    pub(crate) fn from_live(run: &lofi_core::SubagentLive) -> Self {
        let mut row = Self {
            id: run.id.clone(),
            running: run.running,
            preview: String::new(),
            file: run.session.clone(),
            model: None,
            cost: None,
            elapsed_ms: 0,
        };
        row.fill_live(run);
        row
    }

    /// Whether this row is `run`: by id, or by transcript file when the row
    /// has one. A run spawned moments ago has no file yet, so the id is the
    /// only key that finds it.
    pub(crate) fn is_run(&self, run: &lofi_core::SubagentLive) -> bool {
        self.id == run.id
            || self
                .file
                .as_deref()
                .zip(run.session.as_deref())
                .is_some_and(|(file, session)| file == session)
    }

    /// Overlay what only the live registry knows onto this row.
    pub(crate) fn fill_live(&mut self, run: &lofi_core::SubagentLive) {
        self.id.clone_from(&run.id);
        self.running = run.running;
        self.model.clone_from(&run.model);
        self.cost = run.cost;
        self.elapsed_ms = run.elapsed_ms;
        // The task names what a run in flight is doing; a store preview is the
        // answer so far, which says nothing about work still going on.
        if run.running && !run.task.is_empty() {
            self.preview.clone_from(&run.task);
        }
    }
}

impl SubagentsModalState {
    /// Overlay the registry's live fields onto the loaded rows, matching
    /// each run by id, or by transcript file when the row has one. A row
    /// no live run covers keeps the cost the last listing gave it.
    pub(crate) fn overlay_live(&mut self, runs: &[lofi_core::SubagentLive]) {
        for row in &mut self.rows {
            if let Some(run) = runs.iter().find(|run| row.is_run(run)) {
                row.fill_live(run);
            } else {
                // The row claims a live run the registry no longer has, and
                // only a settle ends one: stop showing it as running.
                row.running = false;
                row.elapsed_ms = 0;
            }
        }
        // A run that started after the listing was read has no row yet. The
        // dialog adds it here rather than re-reading the store each frame.
        for run in runs {
            if !self.rows.iter().any(|row| row.is_run(run)) {
                self.rows.push(SubagentRow::from_live(run));
            }
        }
    }
}

/// The timers tab. Rows are the pending wake-up timers, soonest first.
pub(crate) struct TimersModalState {
    pub(crate) selected: usize,
    pub(crate) rows: Vec<lofi_core::TimerInfo>,
}

/// The sections of the background dialog, in tab order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackgroundTab {
    Subagents,
    Jobs,
    Timers,
}

impl BackgroundTab {
    pub(crate) const ALL: [BackgroundTab; 3] = [Self::Subagents, Self::Jobs, Self::Timers];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Subagents => "subagents",
            Self::Jobs => "jobs",
            Self::Timers => "timers",
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Subagents => "Subagents",
            Self::Jobs => "Jobs",
            Self::Timers => "Timers",
        }
    }

    /// A digit jumps to its tab, so a digit never reaches the row list. `h`
    /// and `l` step the tabs; the list keeps `j` and `k`.
    pub(crate) fn from_key(key: char) -> Option<Self> {
        match key {
            '1' => Some(Self::Subagents),
            '2' => Some(Self::Jobs),
            '3' => Some(Self::Timers),
            _ => None,
        }
    }
}

/// One background dialog for every kind of running work: a tab per kind,
/// the drill-in of the active tab, and the armed kill.
pub(crate) struct BackgroundModalState {
    pub(crate) tab: BackgroundTab,
    pub(crate) jobs: JobsModalState,
    pub(crate) subagents: SubagentsModalState,
    pub(crate) timers: TimersModalState,
    /// Set while drilling into one row; `Esc` returns to the list.
    pub(crate) view: Option<BackgroundView>,
    /// Armed when the user picks a kill: the next `y` confirms, anything
    /// else cancels. The target survives a re-render.
    pub(crate) confirm: Option<BackgroundKill>,
}

pub(crate) enum BackgroundView {
    Job(JobOutputView),
    Subagent(SubagentTranscriptView),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BackgroundKill {
    Job(u64),
    Subagent(String),
    Timer(u64),
    /// Every row of the tab, so a runaway fan-out is one keystroke.
    All(BackgroundTab),
}

impl BackgroundModalState {
    /// Rows of the active tab. Job rows come from the registry snapshot plus
    /// the resumed ids, which the caller reads fresh each frame.
    pub(crate) fn len(&self, jobs: &[JobRow]) -> usize {
        match self.tab {
            BackgroundTab::Subagents => self.subagents.rows.len(),
            BackgroundTab::Jobs => jobs.len(),
            BackgroundTab::Timers => self.timers.rows.len(),
        }
    }

    pub(crate) fn selected(&self) -> usize {
        match self.tab {
            BackgroundTab::Subagents => self.subagents.selected,
            BackgroundTab::Jobs => self.jobs.selected,
            BackgroundTab::Timers => self.timers.selected,
        }
    }

    pub(crate) fn select(&mut self, row: usize) {
        match self.tab {
            BackgroundTab::Subagents => self.subagents.selected = row,
            BackgroundTab::Jobs => self.jobs.selected = row,
            BackgroundTab::Timers => self.timers.selected = row,
        }
    }

    /// Switch tabs, keeping each tab's own selection. A drill-in belongs to
    /// the tab it was opened from, so a switch closes it.
    pub(crate) fn show(&mut self, tab: BackgroundTab) {
        if self.tab != tab {
            self.tab = tab;
            self.view = None;
        }
    }
}

/// Read-only drill-in of one subagent transcript. Turns are built with the
/// same replay the main transcript uses and rendered per frame, so the view
/// matches the main transcript while retained styled lines stay bounded by
/// the viewport. Tool expansion is not wired up here.
pub(crate) struct SubagentTranscriptView {
    pub(crate) id: String,
    /// Transcript file name, kept so a live run's view can reload.
    pub(crate) file: Option<String>,
    /// Resolved transcript path, cached so a live reload does not list the
    /// sessions directory on every tick.
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) turns: Vec<Turn>,
    pub(crate) load_error: Option<String>,
    /// Per-turn rendered heights; `None` marks a turn that changed since
    /// the last render and still needs a measure pass.
    pub(crate) heights: Vec<Option<usize>>,
    /// Change detection per turn. A reload serializes the fresh turns once
    /// and compares against these; the old turns are never re-serialized.
    pub(crate) signatures: Vec<u64>,
    /// Complete heights from the last render before a reload; the renderer
    /// compares them against the new layout to keep a scrolled-up view on
    /// the same content. Empty when no complete measurement exists.
    pub(crate) prior_heights: Vec<usize>,
    /// Parsed byte watermark at the last load; a file length equal to it
    /// skips the reload. A same-length rewrite would slip through, but the
    /// transcript is append-only in practice.
    pub(crate) parsed_len: Option<u64>,
    pub(crate) width: usize,
    pub(crate) total: usize,
    pub(crate) scroll: usize,
    /// Last reload of a live run; throttles the live refresh.
    pub(crate) last_reload: std::time::Instant,
    /// The run was live at the last load: when it settles, one final
    /// reload picks up the closing events.
    pub(crate) live: bool,
    /// Set by the refresh after a successful reload; the renderer then
    /// compensates the bottom-relative scroll for the appended height.
    pub(crate) reloaded: bool,
    /// Body height from the last render; the key handler clamps scroll
    /// against it so there is no dead zone past the top.
    pub(crate) view_h: usize,
}

/// The jobs tab. The list is rebuilt from a fresh
/// [`lofi_core::JobRegistry::snapshot`] each render, so only `selected` is
/// kept here. Plain-job logs page bounded windows from disk, so an
/// unbounded log never inflates memory.
pub(crate) struct JobsModalState {
    pub(crate) selected: usize,
}

/// One jobs-tab row: an entry of the live registry, or a job a resumed
/// transcript still lists as started. Only the registry knows a log or a
/// screen; a stale row knows what it ran only when the record kept that.
pub(crate) enum JobRow {
    Live(lofi_core::JobInfo),
    Stale(lofi_core::session::replay::OutstandingJob),
}

impl JobRow {
    pub(crate) fn id(&self) -> u64 {
        match self {
            Self::Live(job) => job.id,
            Self::Stale(job) => job.id,
        }
    }

    pub(crate) fn running(&self) -> bool {
        matches!(self, Self::Live(job) if job.running)
    }
}

/// Drill-in output view for one job. PTY jobs show their parsed terminal
/// screen. Plain jobs retain a bounded, scrollable log tail.
pub(crate) struct JobOutputView {
    pub(crate) id: u64,
    pub(crate) content: JobViewContent,
}

pub(crate) enum JobViewContent {
    Terminal(lofi_core::JobScreen),
    Log {
        lines: std::collections::VecDeque<String>,
        cursor: u64,
        total: u64,
        scroll: usize,
    },
}

impl JobViewContent {
    pub(crate) fn empty_log() -> Self {
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

pub(crate) fn policy_mode_label(mode: lofi_types::BashApprovalMode) -> &'static str {
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
