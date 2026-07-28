//! Screen layout and chrome.
//!
//! The turn log is built from composable components in [`blocks`] over the
//! primitives in [`prim`]; this file owns the frame-level chrome — header,
//! log viewport, working indicator, prompt, status footer, and the resume
//! picker overlay.

// UI rendering uses short, conventional names (`t` for theme, `bg`, `fg`,
// `x`/`y` for cursor coords) and a few long render fns; these pedantic lints
// are noise here and apply to the submodules too.
#![allow(
    clippy::many_single_char_names,
    clippy::needless_lifetimes,
    clippy::similar_names,
    clippy::too_many_lines,
    clippy::too_many_arguments
)]

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::tui::theme::active_indicator;
use crate::tui::SLASH_COMMANDS;
use crate::tui::{App, Mode, NotifyKind, SPINNER};

pub(crate) mod blocks;
pub(crate) mod component;
mod modals;
mod prim;
use modals::{
    render_confirm_modal, render_info_modal, render_model_picker, render_picker,
    render_slash_complete, render_thinking_picker, render_tree_picker,
};

#[allow(unused_imports)]
pub(crate) use prim::RawLine;
pub(crate) use prim::RenderLine;
pub(crate) use prim::VisLine;

pub(crate) use prim::HStack;

/// Frame-level vertical stack for fixed chrome. Spacing is explicit via
/// [`VStack::spacer`], so adjacent components are joined unless the caller
/// deliberately inserts blank space between them.
enum VRegion {
    /// Fixed-height region; a height of 0 means the region is absent.
    Fixed(u16),
    /// Flexible region that fills the space left by the fixed regions.
    Fill,
    /// Explicit blank space between components.
    Spacer(u16),
}

struct VStack {
    regions: Vec<VRegion>,
}

impl VStack {
    fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }
    fn fixed(&mut self, h: u16) {
        self.regions.push(VRegion::Fixed(h));
    }
    fn fill(&mut self) {
        self.regions.push(VRegion::Fill);
    }
    fn spacer(&mut self, h: u16) {
        self.regions.push(VRegion::Spacer(h));
    }

    /// Split `area` into one rect per region. Spacer regions are cleared and
    /// return `None`; zero-height fixed regions likewise remain absent.
    fn split(&self, f: &mut Frame, area: Rect) -> Vec<Option<Rect>> {
        let constraints = self.regions.iter().map(|region| match region {
            VRegion::Fixed(h) | VRegion::Spacer(h) => Constraint::Length(*h),
            VRegion::Fill => Constraint::Min(0),
        });
        let chunks = Layout::vertical(constraints).split(area);
        self.regions
            .iter()
            .zip(chunks.iter())
            .map(|(region, &rect)| match region {
                VRegion::Spacer(_) => {
                    f.render_widget(Clear, rect);
                    None
                }
                VRegion::Fixed(0) => None,
                VRegion::Fixed(_) | VRegion::Fill => Some(rect),
            })
            .collect()
    }
}

pub(crate) fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let input_lines = app.input_lines(area.width as usize).max(1);
    let input_h = u16::try_from(input_lines).unwrap_or(u16::MAX);
    // Keep the prompt cursor on screen within its capped height.
    app.sync_input_scroll(area.width.saturating_sub(3) as usize, input_lines);

    // The footer: a mode-badge line on the default background, then the
    // panel — a leading blank, the prompt, a blank, and the usage line —
    // on panel_bg with the `▌` gutter. The VStack gap supplies the blank
    // row above the mode line.
    let footer_h = input_h.saturating_add(4);
    let running = u16::from(app.run_active());
    let mut vs = VStack::new();
    vs.fixed(1); // header
    vs.spacer(1);
    vs.fill(); // log viewport
    vs.spacer(1);
    vs.fixed(running); // working indicator (absent when idle)
    vs.spacer(running); // separate an active indicator from the footer
    vs.fixed(footer_h); // mode line + panel
    vs.fixed(u16::from(app.debug.is_some())); // full-width debug bar, no spacer
    let rects = vs.split(f, area);

    if let Some(r) = rects[0] {
        render_header(f, r, app);
    }
    if let Some(r) = rects[2] {
        render_log(f, r, app);
    }
    if let Some(r) = rects[4] {
        render_working(f, r, app);
    }
    if let Some(r) = rects[6] {
        render_footer_block(f, r, app);
    }
    if let Some(r) = rects[7] {
        render_debug_bar(f, r, app);
    }
    if app.picker.is_some() {
        render_picker(f, area, app);
    }
    if app.tree_picker.is_some() {
        render_tree_picker(f, area, app);
    }
    if app.model_picker.is_some() {
        render_model_picker(f, area, app);
    }
    if app.thinking_picker.is_some() {
        render_thinking_picker(f, area, app);
    }
    if app.info.is_some() {
        render_info_modal(f, area, app);
    }
    if !app.pending_confirms.is_empty() {
        render_confirm_modal(f, area, app);
    }
    if app.slash_complete.is_some() {
        render_slash_complete(f, area, app);
    }
}

fn render_header(f: &mut Frame, area: Rect, app: &App) {
    f.render_widget(
        Paragraph::new(app.render_header_line(area.width as usize)).alignment(Alignment::Left),
        area,
    );
}

#[allow(clippy::needless_range_loop)]
/// Push visible lines from one segment (a turn's lines, or a single blank)
/// into the viewport buffers, honoring the scroll offset `off` and the
/// remaining capacity `want`. `pos` is the absolute line index of the
/// segment's first line, advanced as we walk.
fn feed_segment(
    lines: &[RenderLine],
    pos: &mut usize,
    off: usize,
    want: &mut usize,
    vis: &mut Vec<Line<'static>>,
    visv: &mut Vec<VisLine>,
) {
    if *want == 0 {
        return;
    }
    for rl in lines {
        if *want == 0 {
            return;
        }
        if *pos < off {
            *pos += 1;
            continue;
        }
        vis.push(rl.line.clone());
        visv.push(VisLine {
            rendered: rl.line.spans.iter().map(|s| s.content.as_ref()).collect(),
            content: rl.content,
            raw: rl.raw.clone(),
        });
        *pos += 1;
        *want -= 1;
    }
}

fn render_log(f: &mut Frame, area: Rect, app: &mut App) {
    let scroll_area = prim::scroll_area(area);
    let content = scroll_area.content;
    let w = content.width as usize;
    let height = content.height as usize;
    // Detect a re-wrap before `ensure_frozen` updates `frozen_width`: the
    // absolute `top_line` is meaningless across a width change, so it is
    // re-anchored to the viewport's previous relative position below. A height
    // change doesn't re-wrap, but the Navigate cursor's row may no longer fit,
    // so it is re-seated (clamped) on either kind of resize.
    let width_changed = app.frozen_width != w;
    let view_changed = width_changed || app.log_view_h != height;
    // Before `ensure_frozen` clears the old-width frozen cache, capture the
    // Navigate cursor's content anchor so it can be re-seated on the same
    // content line after the re-wrap (an absolute line index would drift).
    let nav_anchor = if width_changed && matches!(app.mode, Mode::Navigate | Mode::Select) {
        app.nav_content_anchor()
    } else {
        None
    };
    // Capture the Select-mode selection anchor's content position too: it's
    // an absolute `(line, col)` that drifts across a re-wrap just like the
    // cursor. Both endpoints must be re-seated so the selection survives a
    // resize on the same content characters.
    let sel_anchor = if width_changed && app.mode == Mode::Select {
        app.sel_content_anchor()
    } else {
        None
    };
    // Capture the cursor's previous viewport row so the viewport can be
    // re-anchored to keep the cursor on that row after the re-wrap. Content
    // tracking preserves the cursor's character, but the lines between the
    // viewport top and cursor re-wrap to a different count, which would
    // otherwise drift the cursor's row by 1-2 lines on a widen. Pinning the
    // row lets the viewport follow the cursor instead.
    let nav_row = if width_changed && matches!(app.mode, Mode::Navigate | Mode::Select) {
        Some(app.nav_cursor.saturating_sub(app.log_off))
    } else {
        None
    };
    // Sync the frozen-turn height index before reading it. Normally this is
    // every turn but the live last one; idle file-backed views may index all.
    app.ensure_frozen(w);
    let theme = app.theme;
    let n_turns = app.turns.len();
    let frozen_turns = app.frozen_heights.len();
    let has_live_turn = frozen_turns < n_turns;
    let running = app.run_active();

    // Measure the live last turn without materializing its styled rows. Tool
    // bodies can be enormous in /verbose; only the viewport window is built
    // below after the scroll offset is known. Idle /tree rollbacks may instead
    // make every turn file-backed, in which case the final turn is included in
    // `frozen_heights` and there is no separate live turn.
    let last_h = if has_live_turn {
        let cx = component::Cx {
            app,
            theme,
            width: w,
            active_turn: running,
        };
        blocks::render_turn_height(&cx, &app.turns[n_turns - 1])
    } else {
        0
    };

    // Total line count mirrors `render_turns`: per-turn lines plus a blank
    // between turns (no trailing blank — the separator below the log is
    // owned by the working/input layout).
    let frozen_total: usize = app.frozen_heights.iter().sum();
    app.last_turn_height = last_h;
    let mut total: usize = frozen_total + last_h;
    total += if n_turns == 0 { 1 } else { n_turns - 1 };

    let base = total.saturating_sub(height);
    app.log_rect = content;
    // A re-wrap shifts absolute line indices, so a `top_line` carried over
    // from the previous width may now point past the new bottom — clamping it
    // would snap a scrolled-up view to the bottom and stick there (`pinned`).
    // Re-anchor to the viewport's previous relative position instead. Integer
    // math floors, so it can't round up to `base` and spuriously pin. A
    // pinned (tail-following) view is left at the bottom by design.
    if width_changed && !app.pinned && app.last_base > 0 {
        app.top_line = ((app.log_off as u64 * base as u64) / app.last_base as u64) as usize;
    }
    // Re-seat the Navigate/Select cursor on its previous *content* character
    // (the frozen cache and last turn were just re-rendered at the new width,
    // so find the line whose content offset matches the captured anchor), then
    // pin it to its previous viewport row by shifting the viewport top to
    // match. Content tracking alone would let the cursor drift 1-2 rows when
    // the lines between the viewport top and cursor re-wrap to a different
    // count; pinning the row makes the viewport follow the cursor instead.
    // `nav_show_cursor` below clamps to the nearest edge if the row no longer
    // fits (e.g. a height shrink).
    if nav_anchor.is_some() || sel_anchor.is_some() {
        // Resize re-seating needs content offsets across the whole live turn.
        // This is an exceptional path; steady-state rendering remains
        // viewport-local.
        let last_lines = if has_live_turn {
            let cx = component::Cx {
                app,
                theme,
                width: w,
                active_turn: running,
            };
            blocks::render_turn_lines(&cx, &app.turns[n_turns - 1])
        } else {
            Vec::new()
        };
        if let Some(anchor) = nav_anchor {
            app.reseat_nav_cursor(anchor, &last_lines, w);
        }
        if let Some(anchor) = sel_anchor {
            app.reseat_sel_anchor(anchor, &last_lines, w);
        }
    }
    // Keep the cursor on its previous viewport row: the re-seat above put it
    // on its content character; shift the viewport top to match so the cursor
    // doesn't drift when the intervening lines re-wrap. Clamped to `base` so a
    // pinned (tail-following) view stays at the bottom.
    if let Some(row) = nav_row {
        app.top_line = app.nav_cursor.saturating_sub(row).min(base);
    }
    app.last_base = base;
    let mut off = if app.pinned {
        base
    } else {
        app.top_line.min(base)
    };
    app.pinned = off >= base;
    app.log_off = off;
    // Stash the total / viewport height so the Navigate cursor can be clamped
    // and scrolled between events; clamp the cursor if the log shrank.
    app.log_total = total;
    app.log_view_h = height;
    // After a resize the re-seated cursor (or a height-shrunk viewport) may
    // have left the cursor off-screen: scroll minimally to bring it to the
    // nearest edge, keeping it on its content line.
    if view_changed && matches!(app.mode, Mode::Navigate | Mode::Select) {
        app.nav_show_cursor();
        off = if app.pinned {
            base
        } else {
            app.top_line.min(base)
        };
        app.log_off = off;
    }
    if app.nav_cursor >= total {
        app.nav_cursor = total.saturating_sub(1);
        if app.mode == Mode::Select {
            app.sel = Some(app.select_sel());
        }
    }

    // Rendered frozen turns are viewport-local: evict turns that moved out of
    // range and materialize only the visible turns plus a one-turn margin.
    app.sync_frozen_cache_for_viewport(off, height, w);

    // Slice the visible window from the line sequence. Frozen turns entirely
    // above the viewport are skipped via their cached height (no line fetch);
    // turns intersecting the viewport are fetched from the bounded cache
    // (re-rendered from `turns` on a miss). Only the visible `height` lines
    // are cloned.
    let blank = prim::rblank();
    let mut vis: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut visv: Vec<VisLine> = Vec::with_capacity(height);
    let mut pos = 0usize;
    let mut want = height;
    if n_turns == 0 {
        feed_segment(
            std::slice::from_ref(&blank),
            &mut pos,
            off,
            &mut want,
            &mut vis,
            &mut visv,
        );
    } else {
        for i in 0..n_turns {
            if i > 0 {
                feed_segment(
                    std::slice::from_ref(&blank),
                    &mut pos,
                    off,
                    &mut want,
                    &mut vis,
                    &mut visv,
                );
                if want == 0 {
                    break;
                }
            }
            if i < frozen_turns {
                let h = app.frozen_heights[i];
                if pos + h <= off {
                    // Entirely above the viewport; advance without fetching.
                    pos += h;
                } else {
                    let turn_start = pos;
                    if let Some(lines) = app.frozen_render.get(i) {
                        feed_segment(lines, &mut pos, off, &mut want, &mut vis, &mut visv);
                    } else {
                        let start = off.saturating_sub(turn_start);
                        let stop = start.saturating_add(want).min(h);
                        let lines = app.frozen_turn_window(i, w, start..stop);
                        pos = turn_start + start;
                        feed_segment(&lines, &mut pos, off, &mut want, &mut vis, &mut visv);
                    }
                }
            } else {
                // Render only the rows of the mutable last turn that intersect
                // the viewport. This is the critical /verbose path: a huge
                // tool result contributes to total height without retaining a
                // styled line for every output row.
                let turn_start = pos;
                if turn_start + last_h <= off {
                    pos += last_h;
                } else {
                    let start = off.saturating_sub(turn_start);
                    let stop = start.saturating_add(want).min(last_h);
                    let cx = component::Cx {
                        app,
                        theme,
                        width: w,
                        active_turn: running,
                    };
                    let lines =
                        blocks::render_turn_window(&cx, &app.turns[n_turns - 1], start..stop);
                    // The returned slice starts at this global row, not at
                    // the turn's first row.
                    pos = turn_start + start;
                    feed_segment(&lines, &mut pos, off, &mut want, &mut vis, &mut visv);
                }
            }
            if want == 0 {
                break;
            }
        }
    }
    app.log_vis = visv;

    // Highlight the active mouse selection over the visible window only.
    if let Some(sel) = &app.sel {
        let (sl, sc) = sel.start;
        let (el, ec) = sel.end;
        let ((sl, sc), (el, ec)) = if (sl, sc) <= (el, ec) {
            ((sl, sc), (el, ec))
        } else {
            ((el, ec), (sl, sc))
        };
        let sel_bg = app.theme.selection;
        let vis_len = vis.len();
        if vis_len > 0 && el >= off && sl < off + vis_len {
            let lo = sl.max(off);
            let hi = el.min(off + vis_len - 1);
            for li in lo..=hi {
                let rel = li - off;
                let s = &app.log_vis[rel].rendered;
                let (cstart, cend) = app
                    .log_vis
                    .get(rel)
                    .map_or((0, s.chars().count()), |v| v.content);
                // Clamp to the content range so the highlight covers only the
                // content — never the leading gutter or the trailing padding.
                let cs = if li == sl { sc } else { 0 };
                let ce = if li == el { ec } else { s.chars().count() };
                let cs = cs.clamp(cstart, cend);
                let ce = ce.clamp(cstart, cend);
                if cs < ce {
                    prim::apply_selection(&mut vis[rel], cs, ce, sel_bg);
                }
            }
        }
    }

    // Cursor line highlight. On content lines, Navigate tints the whole
    // line (per-span, so it overrides Exec tile backgrounds) and Select marks
    // the cursor cell in a distinct color over the selection. Blank spacing
    // lines have no spans to tint, so fill the width — otherwise the cursor
    // vanishes between blocks.
    if app.mode == Mode::Navigate || app.mode == Mode::Select {
        let cur = app.nav_cursor;
        if cur >= off && cur < off + vis.len() {
            let rel = cur - off;
            let (cstart, cend) = app.log_vis.get(rel).map_or((0, 0), |v| v.content);
            // Cursor cell column: within the content, or at the content start
            // for empty-content lines (e.g. a numbered exec line whose body
            // is blank) so the cursor sits at the first content position.
            let col = if cend > cstart {
                app.nav_col.clamp(cstart, cend - 1)
            } else {
                cstart
            };
            // Lines with any spans keep their decoration (gutter, rails, line
            // numbers); only truly empty lines (rblank separators) are built
            // from scratch. This preserves a line number whose body is empty.
            if vis[rel].spans.is_empty() {
                // Truly blank separator: indent + cursor cell, then a line-bg
                // fill in NAV (SELECT leaves the rest plain).
                if app.mode == Mode::Navigate {
                    vis[rel] = Line::from(vec![
                        Span::styled("  ", Style::new().bg(app.theme.cursor_line)),
                        Span::styled(" ", Style::new().bg(app.theme.select_cursor)),
                        Span::styled(
                            " ".repeat(w.saturating_sub(3)),
                            Style::new().bg(app.theme.cursor_line),
                        ),
                    ]);
                } else {
                    vis[rel] = Line::from(vec![
                        Span::raw("  "),
                        Span::styled(" ", Style::new().bg(app.theme.select_cursor)),
                    ]);
                }
            } else {
                if app.mode == Mode::Navigate {
                    prim::apply_line_bg(&mut vis[rel], app.theme.cursor_line);
                    // Pad past the text so the cursor-line bg spans the whole
                    // row, not just the available text length.
                    let used: usize = vis[rel]
                        .spans
                        .iter()
                        .map(|s| prim::width(s.content.as_ref()))
                        .sum();
                    if used < w {
                        vis[rel].spans.push(Span::styled(
                            " ".repeat(w - used),
                            Style::new().bg(app.theme.cursor_line),
                        ));
                    }
                }
                // Cursor cell on top of the line bg / selection.
                prim::apply_selection(&mut vis[rel], col, col + 1, app.theme.select_cursor);
            }
        }
    }

    let para = Paragraph::new(vis).scroll((0, 0));
    f.render_widget(para, content);
    draw_scrollbar(f, scroll_area.gutter, off, height, total, app.theme);
}

/// Draw a scrollbar in its dedicated gutter using the theme's muted tones.
fn draw_scrollbar(
    f: &mut Frame,
    gutter: Rect,
    off: usize,
    visible: usize,
    total: usize,
    t: crate::tui::theme::Theme,
) {
    prim::render_scrollbar(f, gutter, off, visible, total, t.subtle, t.muted);
}

/// One-line working indicator above the prompt, shown only while a run is
/// active. Retry progress belongs to the notification area, so this line
/// remains stable throughout backoff and subsequent attempts.
fn render_working(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let frame = SPINNER[app.spinner_frame() % SPINNER.len()];
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{frame} "), Style::new().fg(active_indicator(t))),
        Span::styled(
            format!(
                "Working for {} with ",
                prim::fmt_duration(app.run_elapsed())
            ),
            Style::new().fg(t.subtle),
        ),
        Span::styled(app.run_label(), Style::new().fg(t.muted)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// The prompt rows on the panel background. The `▌` gutter and 2-cell
/// inset are owned by [`render_footer_block`]; this renders only the text,
/// dimmed when the prompt is unfocused. The gaps above and below are owned
/// by the frame [`VStack`].
fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let scroll_area = prim::scroll_area(area);
    let content = scroll_area.content;
    let content_w = content.width as usize;
    // In Navigate/Select the prompt is inert: dim it and hide the cursor so
    // the transcript cursor is the focus. A centered modal (info, /resume,
    // /tree) likewise hides the cursor — it owns input while open.
    let active = app.mode == Mode::Input && !app.modal_open();
    let text_style = Style::new().fg(if active { t.fg } else { t.muted });

    let rows = app.input_select_rows(content_w);
    let total = rows.len();
    let vis_h = area.height as usize;
    // The prompt caps at MAX_INPUT_LINES rows; when the input overflows,
    // show a scroll window over it and a right-edge scrollbar.
    let start = app.input_scroll.min(total.saturating_sub(vis_h));
    let end = (start + vis_h).min(total);

    let mut lines: Vec<Line<'static>> = Vec::with_capacity(end.saturating_sub(start));
    for seg in &rows[start..end] {
        lines.push(Line::from(vec![Span::styled(seg.clone(), text_style)]));
    }
    f.render_widget(
        Paragraph::new(lines).style(Style::new().bg(t.panel_bg)),
        content,
    );

    if active {
        let (vrow, x_in) = app.input_cursor_pos(content_w);
        if vrow >= start && vrow < end {
            let x = content
                .x
                .saturating_add(u16::try_from(x_in).unwrap_or(u16::MAX));
            let y = content
                .y
                .saturating_add(u16::try_from(vrow - start).unwrap_or(u16::MAX));
            f.set_cursor_position((x, y));
        }
    }
    draw_scrollbar(
        f,
        scroll_area.gutter,
        start,
        content.height as usize,
        total,
        t,
    );
}

/// The footer block: a mode-badge line, the prompt, a blank, and a usage
/// line (the last three on the panel background), stacked without gaps.
fn render_footer_block(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    let w = area.width;
    // Row 0: the mode/notification line on the default background — no
    // gutter, no panel. It sits above the panel as a separate strip.
    render_mode_line(f, Rect::new(area.x, area.y, w, 1), app);

    // The panel below: a leading blank, the prompt, a blank, and the stats,
    // all on panel_bg. A continuous user rail spans every panel row, including
    // those vertical gutters and the usage row.
    let panel = Rect::new(
        area.x,
        area.y.saturating_add(1),
        w,
        area.height.saturating_sub(1),
    );
    f.render_widget(Block::default().style(Style::new().bg(t.panel_bg)), panel);
    let active = app.mode == Mode::Input && !app.modal_open();
    // A leading `!` (including `!!`) switches the prompt rail to the shell
    // accent immediately, making bash mode visible before submission.
    let bar = if !active {
        t.subtle
    } else if app.input.starts_with('!') {
        t.warn
    } else {
        t.user
    };
    for y in panel.y..panel.bottom() {
        let cell = &mut f.buffer_mut()[(panel.x, y)];
        cell.set_char('▌');
        cell.set_fg(bar);
    }
    let inner = Rect::new(
        area.x.saturating_add(2),
        panel.y,
        w.saturating_sub(2),
        panel.height,
    );
    let prompt_h = inner.height.saturating_sub(3);
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(prompt_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(inner);
    // chunks[0] is the leading blank (panel_bg and rail already painted).
    app.input_rect = prim::scroll_area(chunks[1]).content;
    render_input(f, chunks[1], app);
    render_info(f, chunks[3], app);
}

/// Mode-badge line. The right edge carries the ` VERBOSE ` tag (while tool
/// detail is expanded) and the mode chip (` INPUT ` / ` NAV `) — a filled
/// pill in the mode color. The left edge holds one notification badge
/// (quit > yank > transient slash-command status/error); the middle is
/// blank. A long notification is truncated with `…` so the right side
/// always fits.
fn render_mode_line(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let (label, color) = app.mode_badge();
    let w = area.width as usize;
    let chip = format!(" {label} ");
    let bold = Modifier::BOLD;

    // Right: optional ` VERBOSE ` tag and the mode chip.
    let mut right: Vec<Span<'static>> = Vec::new();
    if app.verbose {
        right.push(Span::styled(
            " VERBOSE ",
            Style::new().fg(t.muted).add_modifier(bold),
        ));
    }
    right.push(Span::styled(
        chip,
        Style::new().fg(t.fg).bg(color).add_modifier(bold),
    ));
    let right_w: usize = right.iter().map(|s| prim::width(s.content.as_ref())).sum();

    // Left: one notification badge (quit > yank > notify), else nothing.
    let mut left: Vec<Span<'static>> = Vec::new();
    if let Some(badge) = app.quit_badge() {
        left.push(Span::styled(
            format!(" {badge} "),
            Style::new().fg(t.fg).bg(t.warn).add_modifier(bold),
        ));
    } else if let Some(badge) = app.yank_badge() {
        left.push(Span::styled(
            format!(" {badge} "),
            Style::new().fg(t.fg).bg(t.primary).add_modifier(bold),
        ));
    } else if let Some(retry) = app.retry_badge() {
        // Retry progress is live status, not transcript content. It remains
        // visible until RetryEnd clears it after a successful request (or
        // final failure).
        left.push(Span::styled(
            format!(" {retry} "),
            Style::new().fg(t.fg).bg(t.warn).add_modifier(bold),
        ));
    } else if let Some(queue) = app.queue_badge() {
        // Persistent queue badge (does not expire like transient badges).
        left.push(Span::styled(
            format!(" {queue} "),
            Style::new().fg(t.fg).bg(t.muted).add_modifier(bold),
        ));
    } else if let Some((msg, kind)) = app.notify_badge() {
        let bg = match kind {
            NotifyKind::Info => t.muted,
            NotifyKind::Warn => t.warn,
            NotifyKind::Error => t.error,
        };
        // Reserve room for the 2-cell left padding, the right side, and the
        // badge's wrapping spaces.
        let avail = w
            .saturating_sub(2)
            .saturating_sub(right_w)
            .saturating_sub(2);
        if avail >= 1 {
            let body = if prim::width(msg) > avail {
                let mut s = prim::truncate(msg, avail.saturating_sub(1));
                s.push('…');
                s
            } else {
                msg.to_string()
            };
            left.push(Span::styled(
                format!(" {body} "),
                Style::new().fg(t.fg).bg(bg).add_modifier(bold),
            ));
        }
    }

    // A diagonal rule spans the prompt/notification bar. It starts in the
    // prompt panel's tone and is tinted by the active mode so focus changes
    // remain visible without the old lower-block underline.
    let rule: String = std::iter::repeat_n('╱', w).collect();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rule,
            Style::new().fg(color).bg(t.panel_bg),
        ))),
        area,
    );
    // Left badges sit at the 2-cell inset; only as wide as their content so
    // the rule is not overwritten.
    let left_w: usize = left.iter().map(|s| prim::width(s.content.as_ref())).sum();
    if left_w > 0 {
        let lrect = Rect::new(
            area.x.saturating_add(2),
            area.y,
            u16::try_from(left_w).unwrap_or(0),
            1,
        );
        f.render_widget(Paragraph::new(Line::from(left)), lrect);
    }
    // Right chip flush to the right edge.
    let right_w: usize = right.iter().map(|s| prim::width(s.content.as_ref())).sum();
    let rrect = Rect::new(
        area.x
            .saturating_add(u16::try_from(w.saturating_sub(right_w)).unwrap_or(0)),
        area.y,
        u16::try_from(right_w).unwrap_or(0),
        1,
    );
    f.render_widget(Paragraph::new(Line::from(right)), rrect);
}

/// Full-width diagnostics bar shown as its own bottom-level VStack region.
fn render_debug_bar(f: &mut Frame, area: Rect, app: &App) {
    if let Some(line) = app.debug_memory_line() {
        f.render_widget(
            Paragraph::new(line).style(Style::new().fg(app.theme.panel_bg).bg(app.theme.warn)),
            area,
        );
    }
}

/// Gray usage line below the prompt: `  ↑in ↓out · ctx: used/limit` on the
/// left, `$cost` on the right.
fn render_info(f: &mut Frame, area: Rect, app: &App) {
    let w = area.width as usize;
    let left = app.render_footer_left(w).spans;
    let right = app.render_footer_cost().spans;
    let line = HStack::new(w).left(left).right(right).build();
    f.render_widget(
        Paragraph::new(line).style(Style::new().bg(app.theme.panel_bg)),
        area,
    );
}
