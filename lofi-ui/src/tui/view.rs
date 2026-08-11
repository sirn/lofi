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
use crate::tui::{App, Mode, NotifyKind, NOTIFY_MAX_LINES, SPINNER};

pub(crate) mod blocks;
pub(crate) mod component;
mod modals;
mod prim;
use modals::{
    render_confirm_modal, render_info_modal, render_jobs_modal, render_model_picker, render_picker,
    render_slash_complete, render_theme_picker, render_thinking_picker, render_tree_picker,
};

#[allow(unused_imports)] // used by tests and render harnesses
pub(crate) use prim::RawLine;
pub(crate) use prim::RenderLine;
pub(crate) use prim::VisLine;
pub(crate) use prim::{width, wrap};

pub(crate) use prim::HStack;

/// Frame-level vertical stack for fixed chrome. Spacing is explicit via
/// [`VStack::spacer`], so adjacent components are joined unless the caller
/// deliberately inserts blank space between them.
enum VRegion {
    Fixed(u16),
    Fill,
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
    app.sync_input_scroll(area.width.saturating_sub(3) as usize, input_lines);

    let footer_h = input_h
        .saturating_add(3)
        .saturating_add(app.notify_lines(area.width as usize));
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
    if app.theme_picker.is_some() {
        render_theme_picker(f, area, app);
    }
    if app.jobs_modal.is_some() {
        render_jobs_modal(f, area, app);
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
fn feed_segment(
    lines: &[RenderLine],
    pos: &mut usize,
    off: usize,
    want: &mut usize,
    vis: &mut Vec<Line<'static>>,
    visv: &mut Vec<VisLine>,
    links: &mut Vec<Vec<prim::Hyperlink>>,
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
        links.push(rl.links.clone());
        *pos += 1;
        *want -= 1;
    }
}

fn render_log(f: &mut Frame, area: Rect, app: &mut App) {
    let scroll_area = prim::scroll_area(area);
    let content = scroll_area.content;
    let w = content.width as usize;
    let height = content.height as usize;
    let width_changed = app.frozen_width != w;
    let log_started = std::time::Instant::now();
    app.render_profile.width = w;
    app.render_profile.height = height;
    app.render_profile.turns = app.turns.len();
    app.render_profile.width_changed = width_changed;
    let view_changed = width_changed || app.log_view_h != height;
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
    let phase_started = std::time::Instant::now();
    app.ensure_frozen(w);
    app.render_profile.ensure_frozen_us = phase_started.elapsed().as_micros();
    app.render_profile.frozen_turns = app.frozen_heights.len();
    let theme = app.theme;
    let n_turns = app.turns.len();
    let frozen_turns = app.frozen_heights.len();
    let has_live_turn = frozen_turns < n_turns;
    let running = app.run_active();

    let phase_started = std::time::Instant::now();
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
    app.render_profile.live_height_us = phase_started.elapsed().as_micros();

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

    let phase_started = std::time::Instant::now();
    app.sync_frozen_cache_for_viewport(off, height, w);
    app.render_profile.viewport_cache_us = phase_started.elapsed().as_micros();

    let blank = prim::rblank();
    let mut vis: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut visv: Vec<VisLine> = Vec::with_capacity(height);
    let mut links: Vec<Vec<prim::Hyperlink>> = Vec::with_capacity(height);
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
            &mut links,
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
                    &mut links,
                );
                if want == 0 {
                    break;
                }
            }
            if i < frozen_turns {
                let h = app.frozen_heights[i];
                if pos + h <= off {
                    pos += h;
                } else {
                    let turn_start = pos;
                    if let Some(lines) = app.frozen_render.get(i) {
                        feed_segment(
                            lines, &mut pos, off, &mut want, &mut vis, &mut visv, &mut links,
                        );
                    } else {
                        let start = off.saturating_sub(turn_start);
                        let stop = start.saturating_add(want).min(h);
                        let phase_started = std::time::Instant::now();
                        let lines = app.frozen_turn_window(i, w, start..stop);
                        app.render_profile.frozen_window_us += phase_started.elapsed().as_micros();
                        pos = turn_start + start;
                        feed_segment(
                            &lines, &mut pos, off, &mut want, &mut vis, &mut visv, &mut links,
                        );
                    }
                }
            } else {
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
                    let phase_started = std::time::Instant::now();
                    let lines =
                        blocks::render_turn_window(&cx, &app.turns[n_turns - 1], start..stop);
                    app.render_profile.live_window_us += phase_started.elapsed().as_micros();
                    pos = turn_start + start;
                    feed_segment(
                        &lines, &mut pos, off, &mut want, &mut vis, &mut visv, &mut links,
                    );
                }
            }
            if want == 0 {
                break;
            }
        }
    }
    app.log_vis = visv;
    app.render_profile.log_total_us = log_started.elapsed().as_micros();

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
            if vis[rel].spans.is_empty() {
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
                prim::apply_selection(&mut vis[rel], col, col + 1, app.theme.select_cursor);
            }
        }
    }

    let para = Paragraph::new(vis).scroll((0, 0));
    f.render_widget(para, content);
    prim::apply_hyperlinks(f.buffer_mut(), content, &app.log_vis, &links);
    draw_scrollbar(f, scroll_area.gutter, off, height, total, app.theme);
}

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

fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let scroll_area = prim::scroll_area(area);
    let content = scroll_area.content;
    let content_w = content.width as usize;
    let active = app.mode == Mode::Input && !app.modal_open();
    let text_style = Style::new().fg(if active { t.fg } else { t.muted });

    let rows = app.input_select_rows(content_w);
    let total = rows.len();
    let vis_h = area.height as usize;
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

fn render_footer_block(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    let w = area.width;
    let nh = app.notify_lines(w as usize);
    render_mode_line(f, Rect::new(area.x, area.y, w, nh), app);

    let panel = Rect::new(
        area.x,
        area.y.saturating_add(nh),
        w,
        area.height.saturating_sub(nh),
    );
    f.render_widget(Block::default().style(Style::new().bg(t.panel_bg)), panel);
    let active = app.mode == Mode::Input && !app.modal_open();
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
    app.input_rect = prim::scroll_area(chunks[1]).content;
    render_input(f, chunks[1], app);
    render_info(f, chunks[3], app);
}

/// Mode-badge strip. The bottom row carries the ` VERBOSE ` tag (while tool
/// detail is expanded) and the mode chip (` INPUT ` / ` NAV `) on the right,
/// and one notification badge on the left (quit > yank > retry > queue >
/// transient status/error). A long notification wraps across up to
/// [`NOTIFY_MAX_LINES`] rows above the chips instead of truncating to one,
/// so the right-side chrome always stays put.
fn render_mode_line(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let (label, color) = app.mode_badge();
    let w = area.width as usize;
    let chip = format!(" {label} ");
    let bold = Modifier::BOLD;

    // Chips anchor the bottom row of the strip.
    let chip_row = area.bottom().saturating_sub(1);

    let mut right: Vec<Span<'static>> = Vec::new();
    // Persistent running-jobs count sits left of the mode chip; transient
    // badges own the left edge, so this stays out of their way.
    if let Some(badge) = app.jobs_badge() {
        // Non-subtle background: the badge is active state, not ambient
        // chrome, so it should read at a glance.
        right.push(Span::styled(
            format!(" {badge} "),
            Style::new().fg(t.fg).bg(t.info).add_modifier(bold),
        ));
    }
    if app.verbose {
        right.push(Span::styled(
            " VERBOSE ",
            Style::new().fg(t.muted).add_modifier(bold),
        ));
    }
    // INPUT is the default state, so render it quietly (muted text on panel
    // background) instead of as an accent bg chip. NAV/SELECT are modal
    // shifts the user should notice, so they keep the accent background
    // treatment. Notably, `t.muted` mid-gray clashes with `t.fg` under
    // either theme polarity, so it cannot safely serve as chip bg with fg text.
    let chip_style = if matches!(app.mode, Mode::Input) {
        Style::new().fg(t.muted).bg(t.panel_bg).add_modifier(bold)
    } else {
        Style::new().fg(t.fg).bg(color).add_modifier(bold)
    };
    right.push(Span::styled(chip, chip_style));
    let right_w: usize = right.iter().map(|s| prim::width(s.content.as_ref())).sum();

    // Background rule for the full strip, in the mode color.
    let rule: String = std::iter::repeat_n('╱', w).collect();
    for row in area.y..=chip_row {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                rule.clone(),
                Style::new().fg(color).bg(t.panel_bg),
            ))),
            Rect::new(area.x, row, area.width, 1),
        );
    }

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
        left.push(Span::styled(
            format!(" {retry} "),
            Style::new().fg(t.fg).bg(t.warn).add_modifier(bold),
        ));
    } else if let Some(queue) = app.queue_badge() {
        left.push(Span::styled(
            format!(" {queue} "),
            Style::new().fg(t.fg).bg(t.muted).add_modifier(bold),
        ));
    }
    if !left.is_empty() {
        let left_w: usize = left.iter().map(|s| prim::width(s.content.as_ref())).sum();
        let lrect = Rect::new(
            area.x.saturating_add(2),
            chip_row,
            u16::try_from(left_w).unwrap_or(0),
            1,
        );
        f.render_widget(Paragraph::new(Line::from(left)), lrect);
    } else if let Some((msg, kind)) = app.notify_badge() {
        // Wrapped notification block: first NOTIFY_MAX_LINES - 1 lines get the
        // full width, the last shares its row with the chips.
        let last_avail = w.saturating_sub(4).saturating_sub(right_w);
        let full_avail = w.saturating_sub(4);
        let mut lines = prim::wrap(msg, full_avail);
        let overflow = lines.len() > NOTIFY_MAX_LINES;
        lines.truncate(NOTIFY_MAX_LINES);
        // Re-wrap the final line against the narrower bottom-row width.
        if let Some(last) = lines.last_mut() {
            let mut pieces = prim::wrap(last, last_avail);
            let display_overflow = overflow || pieces.len() > 1;
            let mut shown = pieces.drain(..).next().unwrap_or_default();
            if display_overflow {
                shown = prim::truncate(&shown, last_avail.saturating_sub(1));
                shown.push('…');
            }
            *last = shown;
        }
        let bg = match kind {
            NotifyKind::Info => t.muted,
            NotifyKind::Warn => t.warn,
            NotifyKind::Error => t.error,
        };
        let n = lines.len() as u16;
        let top = chip_row.saturating_add(1).saturating_sub(n);
        for (i, line) in lines.iter().enumerate() {
            let y = top.saturating_add(i as u16);
            // Keep each row inside the allocated strip.
            if y < area.y || y > chip_row {
                continue;
            }
            let body = if i as u16 + 1 == n {
                format!(" {line} ")
            } else {
                format!(" {line}")
            };
            let lw = prim::width(&body);
            let lrect = Rect::new(
                area.x.saturating_add(2),
                y,
                u16::try_from(lw.min(w.saturating_sub(2))).unwrap_or(0),
                1,
            );
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    body,
                    Style::new().fg(t.fg).bg(bg).add_modifier(bold),
                ))),
                lrect,
            );
        }
    }

    let rrect = Rect::new(
        area.x
            .saturating_add(u16::try_from(w.saturating_sub(right_w)).unwrap_or(0)),
        chip_row,
        u16::try_from(right_w).unwrap_or(0),
        1,
    );
    f.render_widget(Paragraph::new(Line::from(right)), rrect);
}

fn render_debug_bar(f: &mut Frame, area: Rect, app: &App) {
    if let Some(line) = app.debug_memory_line() {
        f.render_widget(
            Paragraph::new(line).style(Style::new().fg(app.theme.panel_bg).bg(app.theme.warn)),
            area,
        );
    }
}

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
