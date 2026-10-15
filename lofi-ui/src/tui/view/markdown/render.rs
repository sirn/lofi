use super::analyze::analyze;
use super::emit::{emit_blank, emit_code, emit_heading, emit_paragraph, emit_quote, emit_table};
use super::model::MdBlock;
use super::plain::{plain_paragraph, render_plain_paragraph_window};
use crate::tui::theme::Theme;
use crate::tui::view::prim::RenderLine;
use ratatui::style::Style;
use ratatui::text::Span;

#[allow(clippy::too_many_arguments)]
pub(in crate::tui::view) fn render_markdown_body_window(
    text: &str,
    t: Theme,
    w: usize,
    content_w: usize,
    base_style: Style,
    lead_fn: impl Fn(usize) -> Vec<Span<'static>>,
    range: std::ops::Range<usize>,
) -> Vec<RenderLine> {
    if text.is_empty() {
        return Vec::new();
    }
    if plain_paragraph(text) {
        return render_plain_paragraph_window(text, content_w, base_style, &lead_fn, range);
    }
    render_markdown_body(text, t, w, content_w, base_style, lead_fn)
        .into_iter()
        .skip(range.start)
        .take(range.end.saturating_sub(range.start))
        .collect()
}

pub(in crate::tui::view) fn render_markdown_body(
    text: &str,
    t: Theme,
    w: usize,
    content_w: usize,
    base_style: Style,
    lead_fn: impl Fn(usize) -> Vec<Span<'static>>,
) -> Vec<RenderLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut row = 0usize;
    for block in analyze(text, t, base_style) {
        match block {
            MdBlock::Code { lang, lines, fence } => {
                emit_code(
                    &mut out,
                    &mut row,
                    text,
                    lang.as_deref(),
                    &lines,
                    &fence,
                    t,
                    w,
                    content_w,
                    &lead_fn,
                );
            }
            MdBlock::Table {
                header,
                aligns,
                rows,
                src,
            } => {
                emit_table(
                    &mut out, &mut row, text, &header, &aligns, &rows, &src, t, content_w, &lead_fn,
                );
            }
            MdBlock::Heading {
                level,
                content_start,
                src,
                ..
            } => {
                emit_heading(
                    &mut out,
                    &mut row,
                    text,
                    level,
                    content_start,
                    &src,
                    t,
                    base_style,
                    content_w,
                    &lead_fn,
                );
            }
            MdBlock::Quote { lines, .. } => {
                emit_quote(&mut out, &mut row, text, &lines, t, content_w, &lead_fn);
            }
            MdBlock::Paragraph { spans, src } => {
                emit_paragraph(&mut out, &mut row, text, &spans, &src, content_w, &lead_fn);
            }
            MdBlock::Blank { src } => {
                emit_blank(&mut out, &mut row, text, &src, &lead_fn);
            }
        }
    }
    out
}
