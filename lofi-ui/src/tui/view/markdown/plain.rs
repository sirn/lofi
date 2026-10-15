use crate::tui::view::prim::{self, RawLine, RenderLine};
use pulldown_cmark::{Event, Options as MdOptions, Parser as MdParser, Tag as MdTag, TagEnd};
use ratatui::style::Style;
use ratatui::text::Span;
use std::sync::Arc;

pub(super) fn plain_paragraph(text: &str) -> bool {
    let opts = MdOptions::ENABLE_STRIKETHROUGH
        | MdOptions::ENABLE_TABLES
        | MdOptions::ENABLE_FOOTNOTES
        | MdOptions::ENABLE_TASKLISTS
        | MdOptions::ENABLE_HEADING_ATTRIBUTES;
    let mut events = MdParser::new_ext(text, opts);
    matches!(events.next(), Some(Event::Start(MdTag::Paragraph)))
        && matches!(events.next(), Some(Event::Text(content)) if content.as_ref() == text)
        && matches!(events.next(), Some(Event::End(TagEnd::Paragraph)))
        && events.next().is_none()
}

pub(super) fn for_each_plain_row(text: &str, max_w: usize, mut visit: impl FnMut(usize, usize)) {
    if max_w == 0 {
        visit(0, text.len());
        return;
    }
    let mut start = 0usize;
    while start < text.len() {
        let mut width = 0usize;
        let mut end = start;
        let mut overflow = None;
        let mut break_after = None;
        for (offset, ch) in text[start..].char_indices() {
            let byte = start + offset;
            let char_end = byte + ch.len_utf8();
            let char_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width + char_w > max_w {
                overflow = Some((byte, char_end, ch));
                break;
            }
            width += char_w;
            end = char_end;
            if ch == ' ' {
                break_after = Some(char_end);
            }
        }
        if let Some((byte, char_end, ch)) = overflow {
            end = if ch == ' ' {
                char_end
            } else if let Some(boundary) = break_after.filter(|boundary| *boundary > start) {
                boundary
            } else if byte > start {
                byte
            } else {
                char_end
            };
        }
        visit(start, end);
        start = end;
    }
}

pub(super) fn plain_paragraph_height(text: &str, content_w: usize) -> usize {
    let mut height = 0usize;
    for_each_plain_row(text, content_w, |_, _| height += 1);
    height
}

pub(super) fn render_plain_paragraph_window(
    text: &str,
    content_w: usize,
    base_style: Style,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
    range: std::ops::Range<usize>,
) -> Vec<RenderLine> {
    let mut row = 0usize;
    let mut out = Vec::with_capacity(range.end.saturating_sub(range.start).min(256));
    for_each_plain_row(text, content_w, |start, end| {
        if range.contains(&row) {
            let source: Arc<str> = Arc::from(&text[start..end]);
            let display_len = text[start..end].chars().count();
            let map = if source.is_ascii() {
                (0..=display_len).collect()
            } else {
                std::iter::once(0)
                    .chain(source.char_indices().map(|(byte, ch)| byte + ch.len_utf8()))
                    .collect()
            };
            out.push(
                prim::rline(
                    lead_fn(row),
                    vec![Span::styled(text[start..end].to_string(), base_style)],
                )
                .with_raw(RawLine::new(source, map, row == 0)),
            );
        }
        row += 1;
    });
    out
}
