use std::sync::Arc;

use ratatui::style::Style;
use ratatui::text::Span;

use crate::tui::{DetailKey, DETAIL_VIEW_ROWS};

use crate::tui::view::component::Cx;
use crate::tui::view::prim::{self, RawLine, RenderLine};

struct DetailVisualRow {
    text: String,
    raw: RawLine,
}

fn detail_visual_rows(lines: &[String], inner: usize) -> Vec<DetailVisualRow> {
    let mut rows = Vec::new();
    for line in lines {
        let source: Arc<str> = Arc::from(line.as_str());
        let indent_len = line
            .bytes()
            .take_while(|&byte| byte == b' ' || byte == b'\t')
            .count();
        let indent_chars = line[..indent_len].chars().count();
        let body = &line[indent_len..];
        let body_offsets = std::iter::once(0)
            .chain(body.char_indices().map(|(byte, c)| byte + c.len_utf8()))
            .collect::<Vec<_>>();
        let segments = prim::wrap_pre(line, inner.max(1));
        let mut consumed = 0usize;
        for (index, segment) in segments.into_iter().enumerate() {
            let body_chars = segment.chars().count().saturating_sub(indent_chars);
            let map = (0..=body_chars)
                .map(|offset| {
                    indent_len
                        + body_offsets
                            .get(consumed + offset)
                            .copied()
                            .unwrap_or(body.len())
                })
                .collect();
            rows.push(DetailVisualRow {
                text: segment,
                raw: RawLine::new(source.clone(), map, index == 0),
            });
            consumed += body_chars;
        }
    }
    rows
}

pub(super) fn detail_row_count(lines: &[String], width: usize, deco: &[Span<'static>]) -> usize {
    let inner = width.saturating_sub(prim::span_width(deco) + 2);
    detail_visual_rows(lines, inner).len()
}

pub(super) fn detail_box(
    lines: &[String],
    key: &DetailKey,
    tail: bool,
    cx: &Cx,
    deco: &[Span<'static>],
    style: Style,
) -> Vec<RenderLine> {
    let expanded = cx.app.expanded_details.get(key);
    let Some(expanded) = expanded else {
        return Vec::new();
    };
    // Reserve one cell after the mini-scrollbar as outer right margin.
    let inner = cx.width.saturating_sub(prim::span_width(deco) + 3);
    let rows = detail_visual_rows(lines, inner);
    let total = rows.len();
    let target =
        |line: RenderLine, row: usize| line.with_detail_row((*key).clone(), total, tail, row);
    let max = total.saturating_sub(DETAIL_VIEW_ROWS);
    let start = expanded
        .scroll
        .unwrap_or(if tail { max } else { 0 })
        .min(max);
    let visible_rows = total.min(DETAIL_VIEW_ROWS);
    let visible = &rows[start..total.min(start + visible_rows)];
    let raised = style.bg(cx.theme.surface);
    let focused = cx
        .app
        .detail_focus
        .as_ref()
        .is_some_and(|focus| focus.key == *key);
    let track = Style::new().fg(cx.theme.subtle).bg(cx.theme.surface);
    let thumb = Style::new()
        .fg(if focused {
            cx.theme.user
        } else {
            cx.theme.muted
        })
        .bg(cx.theme.surface);
    let thumb_row = (start * visible_rows.saturating_sub(1)).checked_div(max);
    let mut out = Vec::with_capacity(visible_rows);
    for (row, visual) in visible.iter().enumerate() {
        let used = prim::width(&visual.text);
        let mut line_deco = deco.to_vec();
        line_deco.push(Span::styled(" ", raised));
        let detail_span = prim::render(
            line_deco,
            vec![Span::styled(visual.text.clone(), raised)],
            vec![
                Span::styled(" ".repeat(inner.saturating_sub(used)), raised),
                if max == 0 {
                    Span::styled(" ", raised)
                } else {
                    Span::styled(
                        if thumb_row == Some(row) { "┃" } else { "│" },
                        if thumb_row == Some(row) { thumb } else { track },
                    )
                },
                Span::raw(" "),
            ],
        )
        .with_raw(visual.raw.clone());
        out.push(target(detail_span, start + row));
    }
    out
}
