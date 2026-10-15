use super::inline::{inline_spans, inline_spans_and_links};
use super::links::split_links_by_rows;
use crate::tui::theme::Theme;
use crate::tui::view::prim::{self, Hyperlink, RawLine, RenderLine};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(super) enum Align {
    Left,
    Right,
    Center,
}

pub(super) fn table_column_widths(
    header: &[String],
    data: &[Vec<String>],
    content_w: usize,
    t: Theme,
) -> Vec<usize> {
    let n_cols = header.len();
    if n_cols == 0 {
        return Vec::new();
    }
    let base = Style::new().fg(t.fg);
    let mut col_w = vec![0usize; n_cols];
    for (i, cell) in header.iter().enumerate() {
        col_w[i] = col_w[i].max(rendered_width(cell, t, base));
    }
    for row in data {
        for (i, cell) in row.iter().enumerate().take(n_cols) {
            col_w[i] = col_w[i].max(rendered_width(cell, t, base));
        }
    }
    // Distribute available width proportionally across all columns when
    // the natural table width exceeds the content area. Two chars are
    // reserved for right padding so the table doesn't hug the edge.
    let table_w = content_w.saturating_sub(2);
    let total: usize = col_w.iter().map(|&w| w + 2).sum::<usize>() + n_cols + 1;
    if total > table_w {
        let overhead = n_cols * 2 + n_cols + 1;
        let avail = table_w.saturating_sub(overhead).max(n_cols);
        let natural = col_w.iter().sum::<usize>().max(1);
        let mut assigned = 0usize;
        for cw in col_w.iter_mut().take(n_cols - 1) {
            *cw = (*cw * avail / natural).max(1);
            assigned += *cw;
        }
        col_w[n_cols - 1] = avail.saturating_sub(assigned).max(1);
    }

    col_w
}

pub(super) fn render_table(
    header: &[String],
    data: &[Vec<String>],
    aligns: &[Align],
    content_w: usize,
    t: Theme,
    src_lines: &[&str],
    start_row: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) -> Vec<RenderLine> {
    let pad: Vec<Span<'static>> = vec![Span::raw("  ")];
    let n_cols = header.len();
    if n_cols == 0 {
        return Vec::new();
    }
    let col_w = table_column_widths(header, data, content_w, t);

    let border = Style::new().fg(t.subtle);
    let hdr_style = Style::new().fg(t.fg).add_modifier(Modifier::BOLD);
    let body_style = Style::new().fg(t.fg);
    let mut out = Vec::new();
    let mut tr = start_row;

    let border_row = |left: char, mid: char, right: char, r: usize| -> RenderLine {
        let mut s = String::from(left);
        for (i, &cw) in col_w.iter().enumerate() {
            for _ in 0..cw + 2 {
                s.push('─');
            }
            s.push(if i + 1 < n_cols { mid } else { right });
        }
        // Border rows carry an empty-source raw so they are suppressed
        // in SELECT yank (no corresponding markdown source line).
        let mut rl = prim::render(lead_fn(r), vec![Span::styled(s, border)], pad.clone());
        rl.raw = Some(RawLine::new(Arc::from(""), Vec::new(), true));
        rl
    };

    out.push(border_row('┌', '┬', '┐', tr));
    tr += 1;
    // Attach the raw markdown source to the first row of each header/data
    // group so whole-line yank recovers the `| … |` line. The display grid
    // doesn't map 1:1 to the source, so the map is empty (degenerate).
    let hdr_rows = table_row(
        &col_w, header, aligns, hdr_style, border, &pad, t, tr, lead_fn,
    );
    let hdr_len = hdr_rows.len();
    let mut hdr_rows = hdr_rows;
    if let Some(first) = hdr_rows.first_mut() {
        if let Some(src) = src_lines.first() {
            first.raw = Some(RawLine::new(Arc::from(*src), Vec::new(), true));
        }
    }
    out.extend(hdr_rows);
    tr += hdr_len;
    let mut sep = border_row('├', '┼', '┤', tr);
    if let Some(src) = src_lines.get(1) {
        sep.raw = Some(RawLine::new(Arc::from(*src), Vec::new(), true));
    }
    out.push(sep);
    tr += 1;
    for (i, row) in data.iter().enumerate() {
        let rows = table_row(
            &col_w, row, aligns, body_style, border, &pad, t, tr, lead_fn,
        );
        let rows_len = rows.len();
        let mut rows = rows;
        if let Some(first) = rows.first_mut() {
            if let Some(src) = src_lines.get(2 + i) {
                first.raw = Some(RawLine::new(Arc::from(*src), Vec::new(), true));
            }
        }
        out.extend(rows);
        tr += rows_len;
        if i + 1 < data.len() {
            out.push(border_row('├', '┼', '┤', tr));
            tr += 1;
        }
    }
    out.push(border_row('└', '┴', '┘', tr));
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) fn table_row(
    col_w: &[usize],
    cells: &[String],
    aligns: &[Align],
    style: Style,
    border: Style,
    pad: &[Span<'static>],
    t: Theme,
    start_row: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) -> Vec<RenderLine> {
    let wrapped: Vec<(Vec<Line<'static>>, Vec<Vec<Hyperlink>>)> = col_w
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let cell = cells.get(i).map_or("", String::as_str);
            let (spans, links) = inline_spans_and_links(cell, t, style);
            let rows = prim::wrap_line_styled(&Line::from(spans), w);
            let links = split_links_by_rows(&links, &rows);
            (rows, links)
        })
        .collect();
    let max_lines = wrapped
        .iter()
        .map(|(rows, _)| rows.len())
        .max()
        .unwrap_or(1)
        .max(1);
    let mut out = Vec::with_capacity(max_lines);
    for line_idx in 0..max_lines {
        let lead = lead_fn(start_row + line_idx);
        let mut offset: usize = lead.iter().map(|span| span.content.chars().count()).sum();
        let mut spans = vec![Span::styled("│", border)];
        let mut links = Vec::new();
        offset += 1;
        for (i, &w) in col_w.iter().enumerate() {
            let cell_spans = wrapped[i]
                .0
                .get(line_idx)
                .map_or(Vec::new(), |line| line.spans.clone());
            let cell_width: usize = cell_spans
                .iter()
                .map(|span| prim::width(&span.content))
                .sum();
            let align = aligns.get(i).copied().unwrap_or(Align::Left);
            let left_pad = match align {
                Align::Left => 0,
                Align::Right => w.saturating_sub(cell_width),
                Align::Center => w.saturating_sub(cell_width) / 2,
            };
            let aligned = align_spans(cell_spans, w, align, style);
            spans.push(Span::raw(" "));
            offset += 1;
            if let Some(row_links) = wrapped[i].1.get(line_idx) {
                links.extend(row_links.iter().map(|link| Hyperlink {
                    start: offset + left_pad + link.start,
                    end: offset + left_pad + link.end,
                    url: link.url.clone(),
                }));
            }
            spans.extend(aligned);
            spans.push(Span::raw(" "));
            spans.push(Span::styled("│", border));
            offset += w + 2;
        }
        out.push(prim::render(lead, spans, pad.to_vec()).with_links(links));
    }
    out
}

pub(super) fn rendered_width(cell: &str, t: Theme, base: Style) -> usize {
    inline_spans(cell, t, base)
        .iter()
        .map(|s| prim::width(&s.content))
        .sum()
}

pub(super) fn align_spans(
    spans: Vec<Span<'static>>,
    w: usize,
    align: Align,
    pad_style: Style,
) -> Vec<Span<'static>> {
    let len: usize = spans.iter().map(|s| prim::width(&s.content)).sum();
    if len > w {
        let mut out = Vec::new();
        let mut remaining = w;
        for span in spans {
            let count = prim::width(&span.content);
            if remaining == 0 {
                break;
            }
            if count <= remaining {
                out.push(span);
                remaining -= count;
            } else {
                let clipped = prim::truncate(&span.content, remaining);
                remaining = remaining.saturating_sub(prim::width(&clipped));
                if !clipped.is_empty() {
                    out.push(Span::styled(clipped, span.style));
                }
                break;
            }
        }
        if remaining > 0 {
            out.push(Span::styled(" ".repeat(remaining), pad_style));
        }
        return out;
    }
    let pad = w - len;
    match align {
        Align::Left => {
            let mut out = spans;
            out.push(Span::styled(" ".repeat(pad), pad_style));
            out
        }
        Align::Right => {
            let mut out = vec![Span::styled(" ".repeat(pad), pad_style)];
            out.extend(spans);
            out
        }
        Align::Center => {
            let left = pad / 2;
            let mut out = vec![Span::styled(" ".repeat(left), pad_style)];
            out.extend(spans);
            out.push(Span::styled(" ".repeat(pad - left), pad_style));
            out
        }
    }
}
