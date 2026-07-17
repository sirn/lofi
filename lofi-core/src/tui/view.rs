//! Plaintext view layout: message log, input box, status bar.
//!
//! Three vertical chunks — a scrollable message log, a bordered input box
//! with a visible cursor, and a one-line status bar. Rendering is plaintext
//! for v1; the layout is split by [`ratatui::layout::Layout`] and each region
//! is a single [`ratatui::widgets::Paragraph`]. The caller (`tui::run_loop`)
//! invokes [`render`] once per wake and lets ratatui diff the frame.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;

use crate::tui::App;

/// Render the whole app into one frame.
pub(crate) fn render(f: &mut Frame, app: &App) {
    let area = f.area();
    let chunks = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .split(area);
    render_log(f, chunks[0], app);
    render_input(f, chunks[1], app);
    render_status(f, chunks[2], app);
}

fn render_log(f: &mut Frame, area: Rect, app: &App) {
    let text = app.render_log();
    let offset = app.scroll_offset(area.height as usize);
    let para = Paragraph::new(text)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    f.render_widget(para, area);
}

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::bordered().title("Input");
    let para = Paragraph::new(app.input.as_str()).block(block);
    f.render_widget(para, area);
    // Place the visible cursor inside the input box. The box has a 1-cell
    // border, so the cursor sits at `x+1 + char_col`, `y+1`.
    let col = u16::try_from(app.cursor_col()).unwrap_or(u16::MAX);
    let x = area.x.saturating_add(1).saturating_add(col);
    let y = area.y.saturating_add(1);
    f.set_cursor_position((x, y));
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let para = Paragraph::new(app.render_status());
    f.render_widget(para, area);
}
