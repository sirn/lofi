//! Terminal UI: ratatui + crossterm event loop.
//!
//! Flicker-free rendering relies on ratatui's double-buffered diff: we never
//! call `terminal.clear()` between frames, only `terminal.draw(|f| ...)` per
//! wake, and ratatui writes just the changed cells.
//!
//! ## The `!Send` agent future
//!
//! [`crate::agent::Agent::run`] is **not** `Send`: the code-mode sandbox holds
//! an `rquickjs` `AsyncContext` which is `!Send`/`!Sync` (see `crate::code`).
//! That means the agent future cannot be `tokio::spawn`'d on the multi-thread
//! runtime. The whole interactive loop therefore runs inside a
//! [`tokio::task::LocalSet`] on the current worker thread: the agent is driven
//! by [`tokio::task::spawn_local`], and the TUI event loop (`tokio::select!`
//! over crossterm input, the `AgentEvent` receiver, and a 60 ms spinner tick)
//! runs alongside it on the same thread. This is the standard pattern for
//! `!Send` futures in tokio.

pub mod view;

use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use lofi_types::Usage;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc::Receiver;
use tokio::task::{JoinHandle, LocalSet};
use tokio::time::MissedTickBehavior;

use crate::agent::{Agent, AgentEvent};
use crate::error::{Error, Result};

/// Braille spinner frames, advanced on each tick while a run is active.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spinner tick interval (ms). 60 ms is fast enough to look alive without
/// burning a core.
const TICK_MS: u64 = 60;

/// A role tag for a line in the rendered message log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    You,
    Assistant,
    Tool,
    Error,
}

/// One accumulated message in the log: a role tag plus the text rendered so
/// far. Assistant text deltas are appended to the current `Assistant` message;
/// tool start/end become `Tool` messages.
#[derive(Debug, Clone)]
struct RenderedMessage {
    role: Role,
    text: String,
}

impl RenderedMessage {
    fn new(role: Role, text: String) -> Self {
        Self { role, text }
    }
}

/// The TUI's mutable state.
///
/// Kept intentionally small: the agent run handle lives in the event loop
/// (it borrows the `mpsc::Receiver`, which is awkward to poll from inside a
/// `select!` while also storing it here), and `App` holds only what the
/// renderer needs.
pub(crate) struct App {
    messages: Vec<RenderedMessage>,
    input: String,
    input_cursor: usize,
    status_model: String,
    status_usage: Option<Usage>,
    spinner_idx: usize,
    /// Lines scrolled back from the bottom; `0` means pinned to the latest
    /// output. [`App::scroll_offset`] converts this to a `Paragraph` scroll.
    log_scroll: usize,
    run_active: bool,
    should_quit: bool,
}

impl App {
    fn new(status_model: String) -> Self {
        Self {
            messages: Vec::new(),
            input: String::new(),
            input_cursor: 0,
            status_model,
            status_usage: None,
            spinner_idx: 0,
            log_scroll: 0,
            run_active: false,
            should_quit: false,
        }
    }

    /// Fold an [`AgentEvent`] into the rendered log / status.
    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::Text(delta) => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Assistant {
                        last.text.push_str(&delta);
                        return;
                    }
                }
                self.messages
                    .push(RenderedMessage::new(Role::Assistant, delta));
            }
            AgentEvent::ToolStart { id, name } => {
                self.messages
                    .push(RenderedMessage::new(Role::Tool, format!("▶ {name} ({id})")));
            }
            AgentEvent::ToolEnd { id: _, result } => {
                if let Some(last) = self.messages.last_mut() {
                    if last.role == Role::Tool {
                        last.text.push_str("\n↳ ");
                        last.text.push_str(&result);
                        return;
                    }
                }
                self.messages
                    .push(RenderedMessage::new(Role::Tool, format!("↳ {result}")));
            }
            AgentEvent::Done(usage) => {
                self.status_usage = Some(usage);
            }
            AgentEvent::Error(msg) => {
                self.messages.push(RenderedMessage::new(Role::Error, msg));
            }
        }
    }

    /// Mark a run as finished (channel closed or cancelled).
    fn run_finished(&mut self) {
        self.run_active = false;
        self.spinner_idx = 0;
    }

    /// Insert a char at the byte cursor, keeping the cursor on a char
    /// boundary.
    fn insert_char(&mut self, c: char) {
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
    }

    /// Delete the char before the cursor.
    fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let i = match self.input[..self.input_cursor].char_indices().last() {
            Some((i, _)) => i,
            None => 0,
        };
        self.input.replace_range(i..self.input_cursor, "");
        self.input_cursor = i;
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

    /// Display column of the cursor (in chars, not bytes).
    fn cursor_col(&self) -> usize {
        self.input[..self.input_cursor].chars().count()
    }

    fn scroll_up(&mut self) {
        self.log_scroll = self.log_scroll.saturating_add(1);
        self.clamp_scroll();
    }

    fn scroll_down(&mut self) {
        self.log_scroll = self.log_scroll.saturating_sub(1);
    }

    fn clamp_scroll(&mut self) {
        let max = self.total_log_lines();
        if self.log_scroll > max {
            self.log_scroll = max;
        }
    }

    /// Approximate total rendered line count of the log, used to clamp scroll
    /// and to compute the `Paragraph` scroll offset. Wrapping may make the
    /// real on-screen count larger; this is a v1 approximation.
    fn total_log_lines(&self) -> usize {
        self.render_log().lines().count().max(1)
    }

    /// The scroll offset to hand to `Paragraph::scroll` — `total - log_scroll`
    /// so `log_scroll == 0` pins to the bottom.
    fn scroll_offset(&self) -> u16 {
        let total = self.total_log_lines();
        let off = total.saturating_sub(self.log_scroll);
        u16::try_from(off).unwrap_or(u16::MAX)
    }

    /// Render the full log as a single string with role prefixes.
    fn render_log(&self) -> String {
        let mut out = String::new();
        for m in &self.messages {
            let prefix = match m.role {
                Role::You => "You: ",
                Role::Assistant => "Assistant: ",
                Role::Tool => "[tool] ",
                Role::Error => "! ",
            };
            out.push_str(prefix);
            out.push_str(&m.text);
            out.push('\n');
        }
        out
    }

    /// One-line status: model, spinner (when active), and token usage.
    fn render_status(&self) -> String {
        let spinner = if self.run_active {
            SPINNER[self.spinner_idx % SPINNER.len()]
        } else {
            " "
        };
        let usage = match self.status_usage {
            Some(u) => format!(" in:{} out:{}", u.input_tokens, u.output_tokens),
            None => String::new(),
        };
        format!(" {} {}{}", self.status_model, spinner, usage)
    }
}

/// An in-flight agent run: the spawned task handle and the event receiver.
struct RunHandle {
    handle: JoinHandle<()>,
    rx: Receiver<AgentEvent>,
}

/// Owns the terminal and restores it on drop — even on panic. The `Drop`
/// impl is the only teardown path; we never call `disable_raw_mode` /
/// `LeaveAlternateScreen` inline.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Diff-render one frame. Never clears between frames.
    fn draw(&mut self, app: &App) -> Result<()> {
        self.terminal
            .draw(|f| view::render(f, app))
            .map_err(Error::Io)?;
        Ok(())
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort restore; failures here cannot be surfaced usefully.
        let _ = self.terminal.show_cursor();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

/// Enter the interactive TUI for `agent`, labeling the status bar with
/// `model_label` (typically `provider/id`).
///
/// Runs the whole loop on a [`LocalSet`] so the `!Send` agent future can be
/// `spawn_local`'d. Returns after the user quits (Ctrl+D / `q`); the
/// [`TerminalGuard`] restores the terminal on the way out.
pub(crate) async fn run(agent: Agent, model_label: String) -> Result<()> {
    enable_raw_mode().map_err(Error::Io)?;
    let setup = (|| -> std::io::Result<_> {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let backend = CrosstermBackend::new(stdout);
        Terminal::new(backend)
    })();
    let terminal = match setup {
        Ok(t) => t,
        Err(e) => {
            // Don't leave raw mode on if alternate-screen setup failed.
            let _ = disable_raw_mode();
            return Err(Error::Io(e));
        }
    };

    let mut guard = TerminalGuard { terminal };
    let local = LocalSet::new();
    let result = local
        .run_until(async move { run_loop(&mut guard, &agent, model_label).await })
        .await;
    result
}

/// The select loop: crossterm input, agent events, and a spinner tick.
async fn run_loop(guard: &mut TerminalGuard, agent: &Agent, model_label: String) -> Result<()> {
    let mut app = App::new(model_label);
    let mut current_run: Option<RunHandle> = None;
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        guard.draw(&app)?;

        // The receiver is polled via a borrowing async block: when no run is
        // active it pending()s forever so the branch never fires. After a
        // branch resolves the borrow is released, so the arm body can mutate
        // `current_run`.
        tokio::select! {
            ev = async {
                match &mut current_run {
                    Some(r) => r.rx.recv().await,
                    None => None,
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
            }
            maybe_ev = events.next() => {
                if let Some(Ok(ev)) = maybe_ev {
                    handle_event(&ev, &mut app, agent, &mut current_run);
                }
            }
            _ = tick.tick() => {
                if app.run_active {
                    app.spinner_idx = app.spinner_idx.wrapping_add(1);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Cancel any in-flight run on quit so the LocalSet doesn't deadlock
    // waiting on a dropped receiver.
    if let Some(r) = current_run.take() {
        r.handle.abort();
    }
    Ok(())
}

/// Translate a crossterm terminal event into app state changes.
fn handle_event(ev: &Event, app: &mut App, agent: &Agent, current_run: &mut Option<RunHandle>) {
    let Event::Key(k) = ev else {
        return;
    };
    if !matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return;
    }
    match k.code {
        KeyCode::Enter if current_run.is_none() && !app.input.is_empty() => {
            let prompt = std::mem::take(&mut app.input);
            app.input_cursor = 0;
            app.messages
                .push(RenderedMessage::new(Role::You, prompt.clone()));
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let agent_clone = agent.clone();
            let handle = tokio::task::spawn_local(async move {
                let _ = agent_clone.run(prompt, tx).await;
            });
            *current_run = Some(RunHandle { handle, rx });
            app.run_active = true;
            app.log_scroll = 0;
        }
        KeyCode::Backspace => app.backspace(),
        KeyCode::Left => app.move_left(),
        KeyCode::Right => app.move_right(),
        KeyCode::Up => app.scroll_up(),
        KeyCode::Down => app.scroll_down(),
        KeyCode::Esc => {
            app.input.clear();
            app.input_cursor = 0;
        }
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            if let Some(r) = current_run.take() {
                r.handle.abort();
                app.run_finished();
                app.messages
                    .push(RenderedMessage::new(Role::Error, "cancelled".to_string()));
            }
        }
        KeyCode::Char('d') if k.modifiers.contains(KeyModifiers::CONTROL) => {
            app.should_quit = true;
        }
        // `q` quits only on an empty input with no run active, so it never
        // swallows a typed prompt.
        KeyCode::Char('q')
            if k.modifiers == KeyModifiers::NONE
                && app.input.is_empty()
                && current_run.is_none() =>
        {
            app.should_quit = true;
        }
        KeyCode::Char(c) => app.insert_char(c),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::Usage;

    fn app() -> App {
        App::new("openai/gpt-4o".to_string())
    }

    #[test]
    fn text_deltas_accumulate_into_one_assistant_message() {
        let mut a = app();
        a.apply_event(AgentEvent::Text("hel".to_string()));
        a.apply_event(AgentEvent::Text("lo".to_string()));
        assert_eq!(a.messages.len(), 1);
        assert_eq!(a.messages[0].role, Role::Assistant);
        assert_eq!(a.messages[0].text, "hello");
    }

    #[test]
    fn tool_start_then_text_starts_new_assistant_message() {
        let mut a = app();
        a.apply_event(AgentEvent::Text("first".to_string()));
        a.apply_event(AgentEvent::ToolStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::Text("second".to_string()));
        assert_eq!(a.messages.len(), 3);
        assert_eq!(a.messages[0].role, Role::Assistant);
        assert_eq!(a.messages[1].role, Role::Tool);
        assert_eq!(a.messages[2].role, Role::Assistant);
        assert_eq!(a.messages[2].text, "second");
    }

    #[test]
    fn tool_end_appends_to_tool_message() {
        let mut a = app();
        a.apply_event(AgentEvent::ToolStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        a.apply_event(AgentEvent::ToolEnd {
            id: "t1".to_string(),
            result: "{ ok: true }".to_string(),
        });
        assert_eq!(a.messages.len(), 1);
        assert!(a.messages[0].text.contains("▶ exec"));
        assert!(a.messages[0].text.contains("↳ { ok: true }"));
    }

    #[test]
    fn done_updates_usage() {
        let mut a = app();
        a.apply_event(AgentEvent::Done(Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        }));
        assert_eq!(a.status_usage.unwrap().output_tokens, 20);
    }

    #[test]
    fn spinner_frame_index_wraps() {
        let mut a = app();
        a.run_active = true;
        for _ in 0..25 {
            a.spinner_idx = a.spinner_idx.wrapping_add(1);
        }
        assert_eq!(SPINNER[a.spinner_idx % SPINNER.len()], SPINNER[5]);
    }

    #[test]
    fn insert_backspace_cursor_boundaries() {
        let mut a = app();
        a.insert_char('h');
        a.insert_char('i');
        assert_eq!(a.input, "hi");
        assert_eq!(a.input_cursor, 2);
        a.move_left();
        assert_eq!(a.input_cursor, 1);
        a.insert_char('X');
        assert_eq!(a.input, "hXi");
        a.backspace();
        assert_eq!(a.input, "hi");
        a.move_right();
        a.backspace();
        assert_eq!(a.input, "h");
    }

    #[test]
    fn scroll_clamps_to_log_lines() {
        let mut a = app();
        a.apply_event(AgentEvent::Text("line1\nline2\nline3".to_string()));
        let max = a.total_log_lines();
        for _ in 0..max + 10 {
            a.scroll_up();
        }
        assert_eq!(a.log_scroll, max);
        for _ in 0..max + 10 {
            a.scroll_down();
        }
        assert_eq!(a.log_scroll, 0);
        assert_eq!(a.scroll_offset(), max as u16);
    }

    #[test]
    fn scroll_offset_pins_bottom_at_zero() {
        let mut a = app();
        a.apply_event(AgentEvent::Text("a\nb\nc".to_string()));
        let total = a.total_log_lines();
        assert_eq!(a.scroll_offset(), total as u16);
        a.scroll_up();
        assert_eq!(a.scroll_offset(), (total - 1) as u16);
    }

    #[test]
    fn render_log_has_role_prefixes() {
        let mut a = app();
        a.apply_event(AgentEvent::Text("hi".to_string()));
        a.messages
            .push(RenderedMessage::new(Role::You, "hello".to_string()));
        let log = a.render_log();
        assert!(log.contains("Assistant: hi"));
        assert!(log.contains("You: hello"));
    }

    #[test]
    fn render_status_shows_spinner_only_when_active() {
        let mut a = app();
        let idle = a.render_status();
        assert!(!idle.contains('⠋'));
        a.run_active = true;
        let active = a.render_status();
        assert!(active.contains('⠋'));
        assert!(active.contains("openai/gpt-4o"));
    }
}
