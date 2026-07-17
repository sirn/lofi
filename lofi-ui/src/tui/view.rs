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
use ratatui::widgets::{Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use crate::tui::theme::{active_indicator, user_indicator};
use crate::tui::{App, SPINNER};

pub(crate) mod blocks;
pub(crate) mod component;
mod prim;

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

    let mut vs = VStack::new(1);
    vs.fixed(1);                               // header
    vs.fill();                                 // log viewport
    vs.fixed(u16::from(app.run_active()));     // working indicator (absent when idle)
    vs.fixed(input_h);                         // prompt
    vs.fixed(1);                               // status footer
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
        render_input(f, r, app);
    }
    if let Some(r) = rects[4] {
        render_status(f, r, app);
    }
    if app.picker.is_some() {
        render_picker(f, area, app);
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
    lines: &[Line<'static>],
    plain: &[String],
    pos: &mut usize,
    off: usize,
    want: &mut usize,
    vis: &mut Vec<Line<'static>>,
    visp: &mut Vec<String>,
) {
    if *want == 0 {
        return;
    }
    for (i, l) in lines.iter().enumerate() {
        if *want == 0 {
            return;
        }
        if *pos < off {
            *pos += 1;
            continue;
        }
        vis.push(l.clone());
        visp.push(plain.get(i).cloned().unwrap_or_default());
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
    let last_lines: Vec<Line<'static>> = if n_turns == 0 {
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
    let last_plain: Vec<String> = last_lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();

    // Total line count mirrors `render_turns`: per-turn lines plus a blank
    // between turns (no trailing blank — the separator below the log is
    // owned by the working/input layout).
    let frozen_total: usize = app.frozen_heights.iter().sum();
    let mut total: usize = frozen_total + last_lines.len();
    total += if n_turns == 0 { 1 } else { n_turns - 1 };

    let height = area.height as usize;
    let base = total.saturating_sub(height);
    app.log_rect = area;
    app.last_base = base;
    let off = if app.pinned { base } else { app.top_line.min(base) };
    app.pinned = off >= base;
    app.log_off = off;

    // Slice the visible window from the line sequence. Frozen turns entirely
    // above the viewport are skipped via their cached height (no line fetch);
    // turns intersecting the viewport are fetched from the bounded cache
    // (re-rendered from `turns` on a miss). Only the visible `height` lines
    // are cloned.
    let blank = prim::blank();
    let blank_plain: String = blank.spans.iter().map(|s| s.content.as_ref()).collect();
    let mut vis: Vec<Line<'static>> = Vec::with_capacity(height);
    let mut visp: Vec<String> = Vec::with_capacity(height);
    let mut pos = 0usize;
    let mut want = height;
    if n_turns == 0 {
        feed_segment(
            std::slice::from_ref(&blank),
            std::slice::from_ref(&blank_plain),
            &mut pos,
            off,
            &mut want,
            &mut vis,
            &mut visp,
        );
    } else {
        for i in 0..n_turns {
            if i > 0 {
                feed_segment(
                    std::slice::from_ref(&blank),
                    std::slice::from_ref(&blank_plain),
                    &mut pos,
                    off,
                    &mut want,
                    &mut vis,
                    &mut visp,
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
                    if let Some((lines, plain)) = app.frozen_render.get(i) {
                        feed_segment(lines, plain, &mut pos, off, &mut want, &mut vis, &mut visp);
                    }
                }
            } else {
                feed_segment(
                    &last_lines,
                    &last_plain,
                    &mut pos,
                    off,
                    &mut want,
                    &mut vis,
                    &mut visp,
                );
            }
            if want == 0 {
                break;
            }
        }
    }
    app.log_lines = visp;

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
                let cs = if li == sl { sc } else { 0 };
                let ce = if li == el { ec } else { prim::width(&app.log_lines[rel]) };
                prim::apply_selection(&mut vis[rel], cs, ce, sel_bg);
            }
        }
    }

    let para = Paragraph::new(vis).scroll((0, 0));
    f.render_widget(para, area);
    draw_scrollbar(f, area, off, total, app.theme);
}

/// Thin right-edge scrollbar: a faint track with a solid thumb sized from the
/// visible/total ratio. Only drawn when content overflows the viewport.
fn draw_scrollbar(f: &mut Frame, area: Rect, off: usize, total: usize, t: crate::tui::theme::Theme) {
    let visible = area.height as usize;
    if total <= visible || visible == 0 {
        return;
    }
    let x = area.right().saturating_sub(1);
    let max_top = total - visible;
    let thumb_h = ((visible * visible) / total).max(1);
    // Map `off ∈ [0, max_top]` onto the thumb travel `[0, visible - thumb_h]`
    // so the thumb reaches both the top and the bottom edge exactly — a plain
    // `off * visible / total` floor-divides short of the bottom and leaves a
    // stray `│` at the last row.
    let thumb_top = (off * (visible - thumb_h)).checked_div(max_top).unwrap_or(0);
    let buf = f.buffer_mut();
    for i in 0..visible {
        let y = area.y.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
        let in_thumb = i >= thumb_top && i < thumb_top + thumb_h;
        let cell = &mut buf[(x, y)];
        cell.set_char(if in_thumb { '█' } else { '│' });
        cell.set_style(Style::new().fg(if in_thumb { t.muted } else { t.subtle }));
    }
}

/// One-line working indicator above the prompt, shown only while a run is
/// active. A spinner in the active tone plus a muted label.
fn render_working(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let frame = SPINNER[app.spinner_frame() % SPINNER.len()];
    let line = Line::from(vec![
        Span::raw("  "),
        Span::styled(format!("{frame} "), Style::new().fg(active_indicator(t))),
        Span::styled(app.run_label(), Style::new().fg(t.muted)),
        Span::styled(
            format!(" working for {}...", prim::fmt_duration(app.run_elapsed())),
            Style::new().fg(t.subtle),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// The prompt: `❯` lead on the first visual row, 2-space indent on wrapped
/// continuations, no background. The gaps above and below are owned by the
/// frame [`VStack`]; this renders only the prompt rows.
fn render_input(f: &mut Frame, area: Rect, app: &App) {
    let t = app.theme;
    let w = area.width as usize;
    let content_w = w.saturating_sub(2);
    let prompt = Style::new().fg(user_indicator(t));
    let text_style = Style::new().fg(t.fg);

    let rows = app.input_visual_rows(content_w);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len());
    for (i, seg) in rows.iter().enumerate() {
        let prefix = if i == 0 {
            Span::styled("❯ ", prompt)
        } else {
            Span::raw("  ")
        };
        lines.push(Line::from(vec![
            prefix,
            Span::styled(seg.clone(), text_style),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);

    let (vrow, x_in) = app.input_cursor_pos(content_w);
    let x = area
        .x
        .saturating_add(2)
        .saturating_add(u16::try_from(x_in).unwrap_or(u16::MAX));
    let y = area
        .y
        .saturating_add(u16::try_from(vrow).unwrap_or(u16::MAX));
    f.set_cursor_position((x, y));
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let w = area.width as usize;
    let left = app.render_footer_left(w);
    let right = app.render_footer_right();
    let line = HStack::new(w).left(left.spans).right(right.spans).build();
    f.render_widget(Paragraph::new(line).alignment(Alignment::Left), area);
}

fn render_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{Block as WidgetBlock, BorderType, ListState};
    let Some(picker) = &app.picker else {
        return;
    };
    let h = u16::try_from(picker.entries.len().min(12) + 2).unwrap_or(14);
    let w = area.width.min(72);
    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(h)]).split(area);
    let horiz = Layout::horizontal([Constraint::Min(0), Constraint::Length(w)]).split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .map(|e| {
            let id = e.id();
            ListItem::new(format!("{}  ({} msgs, {})", id, e.message_count, e.meta.model))
        })
        .collect();
    let list = List::new(items)
        .block(
            WidgetBlock::bordered()
                .border_type(BorderType::Rounded)
                .title(Span::styled(
                    " Resume a session ",
                    Style::new().fg(app.theme.primary).add_modifier(Modifier::BOLD),
                )),
        )
        .style(Style::default().fg(app.theme.fg))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    f.render_stateful_widget(
        list,
        popup,
        &mut ListState::default().with_selected(Some(picker.selected)),
    );
}