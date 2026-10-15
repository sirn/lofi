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

/// `/subagent` modal. Lists the subagents spawned by the live session:
/// store rows (durable, survive restarts) merged with the registry's live
/// overlay (in-flight runs). `Enter` opens a read-only transcript view.
pub(crate) struct SubagentsModalState {
    pub(crate) selected: usize,
    /// Rows loaded at open and on `r`; store order (newest first), then live
    /// rows not yet on disk.
    pub(crate) rows: Vec<SubagentRow>,
    /// Set while viewing one transcript; `Esc` returns to the list.
    pub(crate) viewing: Option<SubagentTranscriptView>,
}

/// One `/subagent` list row. `file` is present for settled runs with a
/// transcript on disk; live-only rows carry the registry's file name.
pub(crate) struct SubagentRow {
    pub(crate) id: String,
    pub(crate) running: bool,
    pub(crate) preview: String,
    /// Transcript file name (`<millis>_<rand>.jsonl`), when one exists.
    pub(crate) file: Option<String>,
}

/// Read-only drill-in of one subagent transcript, rendered as flat lines.
pub(crate) struct SubagentTranscriptView {
    pub(crate) id: String,
    pub(crate) lines: Vec<String>,
    pub(crate) scroll: usize,
    /// Body height from the last render; the key handler clamps scroll
    /// against it so there is no dead zone past the top.
    pub(crate) view_h: usize,
}

/// State for the `/job` modal. The list is rebuilt from a fresh
/// [`lofi_core::JobRegistry::snapshot`] each render, so only `selected` and
/// the drill-in output view are kept here. Plain-job logs page bounded
/// windows from disk, so an unbounded log never inflates memory.
pub(crate) struct JobsModalState {
    pub(crate) selected: usize,
    /// Set while drilling into one job's output; `Esc` returns to the list.
    pub(crate) viewing: Option<JobOutputView>,
    /// Armed when the user picks kill: the next `y` confirms, anything else
    /// cancels. Holds the target job id so the confirm survives a re-render.
    pub(crate) confirm_kill: Option<u64>,
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
