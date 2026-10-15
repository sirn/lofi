use super::inline::{build_content_map, split_map_by_rows, wrap_with_map, MappedSpan};
use super::links::{mapped_hyperlinks, split_links_by_rows};
use super::model::{MdCell, MdQuoteLine};
use super::table::{render_table, Align};
use crate::tui::theme::Theme;
use crate::tui::view::prim::{self, Hyperlink, RawLine, RenderLine};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::Arc;

/// Emit a blank source line as a single empty content row, matching how an
/// empty paragraph wraps (one row, no content).
pub(super) fn emit_blank(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    src: &std::ops::Range<usize>,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let lead = lead_fn(*row);
    let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
    // The blank maps to its (empty) source line so selection reproduces the
    // blank as an empty line rather than dropping it.
    let raw_src = &text[src.clone()];
    out.push(RenderLine {
        line: Line::from(lead),
        content: (deco_len, deco_len),
        raw: Some(RawLine::new(
            Arc::from(raw_src),
            vec![0, raw_src.len()],
            true,
        )),
        links: Vec::new(),
        detail: None,
    });
    *row += 1;
}

/// Emit a fenced code block: opening/closing fence tiles and one tile row per
/// wrapped body segment. Source ranges come from `analyze`; body rows map back
/// to their source line for yank/selection.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_code(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    lang: Option<&str>,
    lines: &[(String, std::ops::Range<usize>)],
    fence: &std::ops::Range<usize>,
    t: Theme,
    w: usize,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let fence_text = &text[fence.clone()];
    let open_label = match lang {
        Some(l) => format!("```{l}"),
        None => "```".to_string(),
    };
    let open_raw = fence_text.split('\n').next().unwrap_or("```");
    out.push(
        prim::rtile(
            lead_fn(*row),
            vec![Span::styled(
                open_label,
                Style::new().fg(t.muted).bg(t.surface),
            )],
            t.surface,
            w,
        )
        .with_raw(RawLine::linear(
            Arc::from(open_raw),
            0,
            open_raw.chars().count(),
            true,
        )),
    );
    *row += 1;
    let avail = content_w.saturating_sub(2);
    for (line_text, _line_range) in lines {
        let src: Arc<str> = Arc::from(line_text.as_str());
        let indent_len = line_text
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .count();
        let body = &line_text[indent_len..];
        // Byte offsets of every char boundary within `body`, including the end.
        // Indexing past the recorded boundary (a wrap segment wider than the
        // remaining body) clamps to `body.len()`, a valid boundary, so the map
        // never yields a mid-UTF8 or out-of-range source offset.
        let body_offs: Vec<usize> = std::iter::once(0)
            .chain(body.char_indices().map(|(b, c)| b + c.len_utf8()))
            .collect();
        let indent_chars = line_text[..indent_len].chars().count();
        let segments = prim::wrap_pre(line_text, avail);
        let mut cum = 0usize;
        for (i, seg) in segments.into_iter().enumerate() {
            let body_chars = seg.chars().count().saturating_sub(indent_chars);
            let map: Vec<usize> = (0..=body_chars)
                .map(|k| {
                    let byte = body_offs
                        .get(cum + k)
                        .copied()
                        .unwrap_or(body.len())
                        .min(body.len());
                    indent_len + byte
                })
                .collect();
            out.push(
                prim::rtile(
                    lead_fn(*row),
                    vec![Span::styled(seg, Style::new().fg(t.fg).bg(t.surface))],
                    t.surface,
                    w,
                )
                .with_raw(RawLine::new(src.clone(), map, i == 0)),
            );
            *row += 1;
            cum += body_chars;
        }
    }
    let close_raw = fence_text.split('\n').next_back().unwrap_or("```");
    out.push(
        prim::rtile(
            lead_fn(*row),
            vec![Span::styled("```", Style::new().fg(t.muted).bg(t.surface))],
            t.surface,
            w,
        )
        .with_raw(RawLine::linear(
            Arc::from(close_raw),
            0,
            close_raw.chars().count(),
            true,
        )),
    );
    *row += 1;
}

/// Concatenate inline spans into plain text for the table grid (which
/// re-parses inline styling itself).
pub(super) fn spans_text(spans: &[MappedSpan]) -> String {
    spans.iter().map(|m| m.span.content.as_ref()).collect()
}

/// Emit a pipe table via the shared grid renderer. Cells carry inline spans
/// and source ranges from `analyze`; the raw `| ... |` source lines feed yank.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_table(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    header: &[MdCell],
    aligns: &[Align],
    rows: &[Vec<MdCell>],
    src: &std::ops::Range<usize>,
    t: Theme,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let src_block = &text[src.clone()];
    let src_lines: Vec<&str> = src_block.split('\n').collect();
    // Pass each cell's raw source text so render_table re-parses inline
    // formatting (bold/code/links) exactly as any other inline context.
    let cell_src = |c: &MdCell| text[c.src.clone()].trim().to_string();
    let header_txt: Vec<String> = header.iter().map(&cell_src).collect();
    let data_txt: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(&cell_src).collect())
        .collect();
    let before = out.len();
    out.extend(render_table(
        &header_txt,
        &data_txt,
        aligns,
        content_w,
        t,
        &src_lines,
        *row,
        lead_fn,
    ));
    *row += out.len() - before;
}

/// Emit an ATX heading. `content_start` is the byte offset of the text after
/// the `# ` prefix; the heading text is the trimmed remainder of the source.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_heading(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    level: u8,
    content_start: usize,
    src: &std::ops::Range<usize>,
    t: Theme,
    base_style: Style,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let trimmed = text[src.clone()].trim_end();
    let h = text[content_start..src.end].trim_end();
    let head_fg = if level <= 2 {
        base_style.fg.unwrap_or(t.fg)
    } else {
        t.muted
    };
    let style = Style::new().fg(head_fg).add_modifier(Modifier::BOLD);
    let prefix_len = content_start - src.start;
    let src_arc: Arc<str> = Arc::from(trimmed);
    let rows = wrap_with_map(h, prefix_len, trimmed.len(), content_w);
    for (i, (seg, map)) in rows.into_iter().enumerate() {
        out.push(
            prim::rline(lead_fn(*row), vec![Span::styled(seg, style)]).with_raw(RawLine::new(
                src_arc.clone(),
                map,
                i == 0,
            )),
        );
        *row += 1;
    }
}

/// Emit a block quote: a `▎ ` bar followed by each wrapped body line. Each
/// source line maps back to its `> ...` raw text for yank.
pub(super) fn emit_quote(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    lines: &[MdQuoteLine],
    t: Theme,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let quote_style = Style::new().fg(t.muted);
    let bar = Span::styled("▎ ", Style::new().fg(t.subtle));
    for ql in lines {
        let qtrimmed = text[ql.src.clone()].trim_end();
        let src_arc: Arc<str> = Arc::from(qtrimmed);
        if ql.spans.is_empty() {
            // Bare `>` line: just the bar with a thin-space content span so
            // selection yanks the raw `>` markdown.
            let map = vec![0, src_arc.len()];
            let mut lead = lead_fn(*row);
            lead.push(bar.clone());
            let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
            let content_span = Span::styled("\u{2009}", quote_style);
            let content_len = content_span.content.chars().count();
            let mut all = lead.clone();
            all.push(content_span);
            out.push(RenderLine {
                line: Line::from(all),
                content: (deco_len, deco_len + content_len),
                raw: Some(RawLine::new(src_arc, map, true)),
                links: Vec::new(),
                detail: None,
            });
            *row += 1;
            continue;
        }
        let body = &qtrimmed[ql.prefix_len..];
        let rows = wrap_with_map(
            body,
            ql.prefix_len,
            qtrimmed.len(),
            content_w.saturating_sub(2),
        );
        for (i, (seg, map)) in rows.into_iter().enumerate() {
            let mut lead = lead_fn(*row);
            lead.push(bar.clone());
            out.push(
                prim::rline(lead, vec![Span::styled(seg, quote_style)]).with_raw(RawLine::new(
                    src_arc.clone(),
                    map,
                    i == 0,
                )),
            );
            *row += 1;
        }
    }
}

/// Emit a paragraph: inline-parse the source sub-slice, wrap to the content
/// width, and split the source-offset map and hyperlinks across wrapped rows.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit_paragraph(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    block_spans: &[MappedSpan],
    src: &std::ops::Range<usize>,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let raw = text[src.clone()].trim_end();
    // Reuse the spans already produced by `analyze`'s single inline pass. They
    // carry global source offsets; shift them to the block-local sub-slice so
    // the content-map / hyperlink logic below works unchanged.
    let base_off = src.start;
    let mapped: Vec<MappedSpan> = block_spans
        .iter()
        .cloned()
        .map(|mut m| {
            m.content_start = m.content_start.saturating_sub(base_off);
            m.boundary_start = m.boundary_start.saturating_sub(base_off);
            m
        })
        .collect();
    let lead_ws = mapped
        .iter()
        .flat_map(|m| m.span.content.chars())
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    let full_map = build_content_map(raw.len(), &mapped);
    let links = mapped_hyperlinks(&mapped);
    let spans: Vec<Span> = mapped.into_iter().map(|m| m.span).collect();
    let line = Line::from(spans);
    let src_arc: Arc<str> = Arc::from(raw);
    let rows = prim::wrap_line_styled(&line, content_w);
    let row_maps = split_map_by_rows(&full_map, lead_ws, &rows);
    let row_links = split_links_by_rows(&links, &rows);
    for (i, wrapped) in rows.into_iter().enumerate() {
        let lead = lead_fn(*row);
        let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
        let links = row_links.get(i).map_or_else(Vec::new, |links| {
            links
                .iter()
                .map(|link| Hyperlink {
                    start: deco_len + link.start,
                    end: deco_len + link.end,
                    url: link.url.clone(),
                })
                .collect()
        });
        out.push(
            prim::rline(lead, wrapped.spans)
                .with_raw(RawLine::new(
                    src_arc.clone(),
                    row_maps.get(i).cloned().unwrap_or_default(),
                    i == 0,
                ))
                .with_links(links),
        );
        *row += 1;
    }
}
