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
    clippy::too_many_lines
)]

use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::tui::theme::active_indicator;
use crate::tui::{App, Mode, NotifyKind, SPINNER};
use crate::tui::SLASH_COMMANDS;

pub(crate) mod blocks;
pub(crate) mod component;
mod prim;
mod modals;
use modals::{render_info_modal, render_model_picker, render_picker, render_slash_complete, render_tree_picker};

pub(crate) use prim::RenderLine;

pub(crate) use prim::HStack;

/// Frame-level vertical stack: the analogue of the log's [`Stack`] for the
/// fixed chrome. Each region renders only its own content; this layout owns
/// the gaps — one blank row between adjacent present regions, with
/// zero-height regions skipped so they leave no gap (the idle working
/// indicator collapses without doubling the separator).
enum VRegion {
    /// Fixed-height region; a height of 0 means the region is absent.
    Fixed(u16),
    /// Flexible region that fills the space left by the fixed regions.
    Fill,
}

struct VStack {
    gap: u16,
    regions: Vec<VRegion>,
}

impl VStack {
    fn new(gap: u16) -> Self {
        Self {
            gap,
            regions: Vec::new(),
        }
    }
    fn fixed(&mut self, h: u16) {
        self.regions.push(VRegion::Fixed(h));
    }
    fn fill(&mut self) {
        self.regions.push(VRegion::Fill);
    }

    /// Split `area` into one rect per region, clearing the gap rows between
    /// present regions. Absent (height-0) regions yield `None` and leave no
    /// gap, mirroring [`Stack`]'s skip-empty rule. The returned rect order
    /// matches the region order; index by the position passed to
    /// [`fixed`](Self::fixed)/[`fill`](Self::fill).
    fn split(&self, f: &mut Frame, area: Rect) -> Vec<Option<Rect>> {
        let present: Vec<usize> = self
            .regions
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                matches!(r, VRegion::Fill) || matches!(r, VRegion::Fixed(h) if *h > 0)
            })
            .map(|(i, _)| i)
            .collect();
        let mut constraints: Vec<Constraint> = Vec::new();
        for (k, &i) in present.iter().enumerate() {
            if k > 0 {
                constraints.push(Constraint::Length(self.gap));
            }
            constraints.push(match self.regions[i] {
                VRegion::Fixed(h) => Constraint::Length(h),
                VRegion::Fill => Constraint::Min(0),
            });
        }
        let chunks = Layout::vertical(constraints).split(area);
        let mut out = vec![None; self.regions.len()];
        let mut ci = 0;
        for (k, &i) in present.iter().enumerate() {
            if k > 0 {
                f.render_widget(Clear, chunks[ci]);
                ci += 1;
            }
            out[i] = Some(chunks[ci]);
            ci += 1;
        }
        out
    }
}

pub(crate) fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let input_lines = app.input_lines(area.width as usize).max(1);
    let input_h = u16::try_from(input_lines).unwrap_or(u16::MAX);
    // Keep the prompt cursor on screen within its capped height.
    app.sync_input_scroll(area.width.saturating_sub(2) as usize, input_lines);

    // The footer: a mode-badge line on the default background, then the
    // panel — a leading blank, the prompt, a blank, and the usage line —
    // on panel_bg with the `▌` gutter. The VStack gap supplies the blank
    // row above the mode line.
    let footer_h = input_h.saturating_add(4);
    let mut vs = VStack::new(1);
    vs.fixed(1);                               // header
    vs.fill();                                 // log viewport
    vs.fixed(u16::from(app.run_active()));     // working indicator (absent when idle)
    vs.fixed(footer_h);                        // mode line + panel
    let rects = vs.split(f, area);

    if let Some(r) = rects[0] {
        render_header(f, r, app);
    }
    if let Some(r) = rects[1] {
        render_log(f, r, app);
    }
    if let Some(r) = rects[2] {
        render_working(f, r, app);
    }
    if let Some(r) = rects[3] {
        render_footer_block(f, r, app);
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
    if app.info.is_some() {
        render_info_modal(f, area, app);
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
    visp: &mut Vec<String>,
    visc: &mut Vec<(usize, usize)>,
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
        visp.push(
            rl.line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect(),
        );
        visc.push(rl.content);
        *pos += 1;
        *want -= 1;
    }
}

fn render_log(f: &mut Frame, area: Rect, app: &mut App) {
    let w = area.width as usize;
    // Sync the frozen-turn cache (all turns but the last) before reading it.
    app.ensure_frozen(w);
    let theme = app.theme;
    let n_turns = app.turns.len();
    let running = app.run_active();

    // The last turn is the only mutable one; rebuild it fresh this frame.
    let last_lines: Vec<RenderLine> = if n_turns == 0 {
        Vec::new()
    } else {
        let cx = component::Cx {
            app,
            theme,
            width: w,
            active_turn: running,
        };
        blocks::render_turn_lines(&cx, &app.turns[n_turns - 1])
    };

    // Total line count mirrors `render_turns`: per-turn lines plus a blank
    // between turns (no trailing blank — the separator below the log is
    // owned by the working/input layout).
    let frozen_total: usize = app.frozen_heights.iter().sum();
    let last_h = last_lines.len();
    app.last_turn_height = last_h;
    let mut total: usize = frozen_total + last_h;
    total += if n_turns == 0 { 1 } else { n_turns - 1 };

    let height = area.height as usize;
    let base = total.saturating_sub(height);
    app.log_rect = area;
    app.last_base = base;
    let off = if app.pinned { base } else { app.top_line.min(base) };
    app.pinned = off >= base;
    app.log_off = off;
    // Stash the total / viewport height so the Navigate cursor can be clamped
    // and scrolled between events; clamp the cursor if the log shrank.
    app.log_total = total;
    app.log_view_h = height;
    if app.nav_cursor >= total {
        app.nav_cursor = total.saturating_sub(1);
        if app.mode == Mode::Select {
            app.sel = Some(app.select_sel());
        }
    }

    // Slice the visible window from the line sequence. Frozen turns entirely
    // above the viewport are skipped via their cached height (no line fetch);
    // turns intersecting the viewport are fetched from the bounded cache
    // (re-rendered from `turns` on a miss). Only the visible `height` lines
    // are cloned.
    let blank = prim::rblank();
    let mut vis: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut visp: Vec<String> = Vec::with_capacity(height);
    let mut visc: Vec<(usize, usize)> = Vec::with_capacity(height);
    let mut pos = 0usize;
    let mut want = height;
    if n_turns == 0 {
        feed_segment(
            std::slice::from_ref(&blank),
            &mut pos,
            off,
            &mut want,
            &mut vis,
            &mut visp,
            &mut visc,
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
                    &mut visp,
                    &mut visc,
                );
                if want == 0 {
                    break;
                }
            }
            if i < n_turns - 1 {
                let h = app.frozen_heights[i];
                if pos + h <= off {
                    // Entirely above the viewport; advance without fetching.
                    pos += h;
                } else {
                    app.ensure_frozen_turn(i, w);
                    if let Some(lines) = app.frozen_render.get(i) {
                        feed_segment(lines, &mut pos, off, &mut want, &mut vis, &mut visp, &mut visc);
                    }
                }
            } else {
                feed_segment(
                    &last_lines,
                    &mut pos,
                    off,
                    &mut want,
                    &mut vis,
                    &mut visp,
                    &mut visc,
                );
            }
            if want == 0 {
                break;
            }
        }
    }
    app.log_lines = visp;
    app.log_content = visc;

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
                let s = &app.log_lines[rel];
                let (cstart, cend) = app
                    .log_content
                    .get(rel)
                    .copied()
                    .unwrap_or((0, s.chars().count()));
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
            let (cstart, cend) = app.log_content.get(rel).copied().unwrap_or((0, 0));
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
    f.render_widget(para, area);
    draw_scrollbar(f, area, off, total, app.theme);
}

/// Thin right-edge scrollbar for a chrome viewport: delegates to the
/// shared [`prim::render_scrollbar`] on the rightmost column of `area`,
/// using the theme's thumb/track tones. Only drawn when content overflows.
fn draw_scrollbar(f: &mut Frame, area: Rect, off: usize, total: usize, t: crate::tui::theme::Theme) {
    let track = Rect::new(area.right().saturating_sub(1), area.y, 1, area.height);
    prim::render_scrollbar(f, track, off, area.height as usize, total, t.subtle, t.muted);
}

/// One-line working indicator above the prompt, shown only while a run is
/// active. A spinner in the active tone plus a muted label. When a retry is
/// in flight, switches to a `⟳ retry N/3 in Xs: <error>` line counting down
/// to the backoff deadline.
fn render_working(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let frame = SPINNER[app.spinner_frame() % SPINNER.len()];
    let line = if let Some(r) = app.retry_state() {
        let remaining = r.remaining();
        Line::from(vec![
            Span::raw("  "),
            Span::styled("⟳ ", Style::new().fg(active_indicator(t))),
            Span::styled(
                format!(
                    "retry {}/{} in {}",
                    r.attempt,
                    r.max_attempts,
                    prim::fmt_duration(remaining)
                ),
                Style::new().fg(t.muted),
            ),
            Span::styled(format!(": {}", r.error), Style::new().fg(t.subtle)),
        ])
    } else {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{frame} "), Style::new().fg(active_indicator(t))),
            Span::styled(app.run_label(), Style::new().fg(t.muted)),
            Span::styled(
                format!(" working for {}...", prim::fmt_duration(app.run_elapsed())),
                Style::new().fg(t.subtle),
            ),
        ])
    };
    f.render_widget(Paragraph::new(line), area);
}

/// The prompt rows on the panel background. The `▌` gutter and 2-cell
/// inset are owned by [`render_footer_block`]; this renders only the text,
/// dimmed when the prompt is unfocused. The gaps above and below are owned
/// by the frame [`VStack`].
fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let w = area.width as usize;
    let content_w = w;
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
    f.render_widget(Paragraph::new(lines).style(Style::new().bg(t.panel_bg)), area);

    if active {
        let (vrow, x_in) = app.input_cursor_pos(content_w);
        if vrow >= start && vrow < end {
            let x = area.x.saturating_add(u16::try_from(x_in).unwrap_or(u16::MAX));
            let y = area
                .y
                .saturating_add(u16::try_from(vrow - start).unwrap_or(u16::MAX));
            f.set_cursor_position((x, y));
        }
    }
    draw_scrollbar(f, area, start, total, t);
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
    // all on panel_bg with the `▌` gutter down the left edge.
    let panel = Rect::new(area.x, area.y.saturating_add(1), w, area.height.saturating_sub(1));
    f.render_widget(Block::default().style(Style::new().bg(t.panel_bg)), panel);
    let active = app.mode == Mode::Input && !app.modal_open();
    let bar = if active { t.primary } else { t.subtle };
    for y in panel.y..panel.bottom() {
        let cell = &mut f.buffer_mut()[(area.x, y)];
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
    // chunks[0] is the leading blank (panel_bg + gutter already painted).
    app.input_rect = chunks[1];
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
    } else if let Some((msg, kind)) = app.notify_badge() {
        let bg = match kind {
            NotifyKind::Info => t.muted,
            NotifyKind::Warn => t.warn,
            NotifyKind::Error => t.error,
        };
        // Reserve room for the 2-cell left padding, the right side, and the
        // badge's wrapping spaces.
        let avail = w.saturating_sub(2).saturating_sub(right_w).saturating_sub(2);
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

    // A thin rule of `╱` spans the full width in the current mode color,
    // visually tying the notification line to the mode chip on the right and
    // the panel below. The chip and badges render on top as narrow widgets so
    // the rule shows through the gaps between them. `╱` is used over `🮙`
    // (the stripped-box glyph) for font coverage — the latter is not
    // universally implemented.
    let rule: String = std::iter::repeat_n('╱', w).collect();
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            rule,
            Style::new().fg(color),
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
        area.x.saturating_add(u16::try_from(w.saturating_sub(right_w)).unwrap_or(0)),
        area.y,
        u16::try_from(right_w).unwrap_or(0),
        1,
    );
    f.render_widget(Paragraph::new(Line::from(right)), rrect);
}

/// Gray usage line below the prompt: `  ↑in ↓out · ctx: used/limit` on the
/// left, `$cost` on the right.
fn render_info(f: &mut Frame, area: Rect, app: &App) {
    let w = area.width as usize;
    let left = app.render_footer_left(w).spans;
    let right = app.render_footer_cost().spans;
    let line = HStack::new(w).left(left).right(right).build();
    f.render_widget(Paragraph::new(line).style(Style::new().bg(app.theme.panel_bg)), area);
}
