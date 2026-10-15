use super::analyze::analyze;
use super::inline::{inline_spans_and_links, wrap_with_map};
use super::model::{MdBlock, MdCell};
use super::plain::{plain_paragraph, plain_paragraph_height};
use super::split::line_spans;
use super::table::{table_column_widths, Align};
use crate::tui::theme::Theme;
use crate::tui::view::prim;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

/// Count the visual rows `render_markdown_body` would emit for `text`, without
/// allocating any styled lines. `Component::height` for the markdown-heavy
/// components routes here so measuring a resumed transcript's heights does not
/// build and immediately discard thousands of `RenderLine` graphs — that
/// transient was the dominant startup/resize allocation. The row arithmetic
/// must mirror `render_markdown_body` exactly; `markdown_height_matches_lines`
/// (test) guards against drift.
pub(in crate::tui::view) fn markdown_body_height(text: &str, content_w: usize) -> usize {
    if text.is_empty() {
        return 0;
    }
    if plain_paragraph(text) {
        return plain_paragraph_height(text, content_w);
    }
    // Walk the same block tree the renderer emits, counting rows with the
    // exact wrap calls each emitter uses but allocating no styled lines. This
    // keeps `height == render(...).len()` structural rather than maintained by
    // a parallel line-scanner.
    let t = Theme::default();
    let base = Style::default();
    let mut row = 0usize;
    for block in analyze(text, t, base) {
        match block {
            MdBlock::Blank { .. } => row += 1,
            MdBlock::Code { lines, .. } => {
                // Opening fence + body rows + closing fence.
                row += 1;
                for (line_text, _) in &lines {
                    row += prim::wrap_pre(line_text, content_w.saturating_sub(2)).len();
                }
                row += 1;
            }
            MdBlock::Table {
                header,
                aligns,
                rows,
                ..
            } => {
                let cell_text = |c: &MdCell| text[c.src.clone()].trim().to_string();
                let header_txt: Vec<String> = header.iter().map(&cell_text).collect();
                let data_txt: Vec<Vec<String>> = rows
                    .iter()
                    .map(|r| r.iter().map(&cell_text).collect())
                    .collect();
                row += table_height_cells(&header_txt, &data_txt, &aligns, content_w);
            }
            MdBlock::Heading {
                content_start, src, ..
            } => {
                let trimmed = text[src.clone()].trim_end();
                let h = text[content_start..src.end].trim_end();
                row += wrap_with_map(h, content_start - src.start, trimmed.len(), content_w).len();
            }
            MdBlock::Quote { lines, .. } => {
                for ql in &lines {
                    if ql.spans.is_empty() {
                        row += 1;
                        continue;
                    }
                    let qtrimmed = text[ql.src.clone()].trim_end();
                    let body = &qtrimmed[ql.prefix_len..];
                    row += wrap_with_map(
                        body,
                        ql.prefix_len,
                        qtrimmed.len(),
                        content_w.saturating_sub(2),
                    )
                    .len();
                }
            }
            MdBlock::Paragraph { src, .. } => {
                let raw = text[src.clone()].trim_end();
                // Match the render path exactly: `line_spans` keeps a leading
                // list marker literal, so a list line wraps to the same width.
                let mapped = line_spans(raw, t, base);
                let spans: Vec<Span> = mapped.into_iter().map(|m| m.span).collect();
                let line = Line::from(spans);
                row += prim::wrap_line_styled(&line, content_w).len();
            }
        }
    }
    row
}

/// Count the visual rows a table would occupy, mirroring `render_table` +
/// `table_row` without allocating the styled grid. Used by `markdown_body_height`.
/// Operates on the same parsed cell set as `render_table`, so both agree on the
/// grid even when pulldown classifies adjacent `|`-lines as one table.
pub(super) fn table_height_cells(
    header: &[String],
    data: &[Vec<String>],
    _aligns: &[Align],
    content_w: usize,
) -> usize {
    let n_cols = header.len();
    if n_cols == 0 {
        return 0;
    }
    let t = Theme::default();
    let base = Style::new().fg(t.fg);

    let col_w = table_column_widths(header, data, content_w, t);

    // A row group occupies the tallest wrapped cell across its columns.
    let row_group = |cells: &[String]| -> usize {
        col_w
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let cell = cells.get(i).map_or("", String::as_str);
                let (spans, _) = inline_spans_and_links(cell, t, base);
                prim::wrap_line_styled(&Line::from(spans), w).len()
            })
            .max()
            .unwrap_or(1)
            .max(1)
    };

    let mut rows = 1; // top border
    rows += row_group(header);
    rows += 1; // header separator
    for (i, row) in data.iter().enumerate() {
        rows += row_group(row);
        if i + 1 < data.len() {
            rows += 1; // interior border
        }
    }
    rows + 1 // bottom border
}
