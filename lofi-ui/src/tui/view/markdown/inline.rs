use super::links::{is_safe_link_url, mapped_hyperlinks};
use crate::tui::theme::Theme;
use crate::tui::view::prim::{self, Hyperlink};
use pulldown_cmark::{Event, Options as MdOptions, Parser as MdParser, Tag as MdTag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use std::sync::Arc;

pub(super) fn inline_spans(line: &str, t: Theme, base: Style) -> Vec<Span<'static>> {
    inline_spans_and_links(line, t, base).0
}

pub(super) fn inline_spans_and_links(
    line: &str,
    t: Theme,
    base: Style,
) -> (Vec<Span<'static>>, Vec<Hyperlink>) {
    let mapped = inline_spans_mapped(line, t, base);
    let links = mapped_hyperlinks(&mapped);
    let spans = mapped.into_iter().map(|mapped| mapped.span).collect();
    (spans, links)
}

/// A styled span paired with the byte offsets it occupies in the source
/// line, so a display selection can be mapped back to the raw markdown.
/// `content_start` is where the span's text begins; `boundary_start` is
/// where a selection *starting* at this span should slice from — the
/// position of any opening marker (`**`, backtick) preceding the content,
/// so wrapped markers come along with the selection.
#[derive(Clone)]
pub(super) struct MappedSpan {
    pub(super) span: Span<'static>,
    pub(super) content_start: usize,
    pub(super) boundary_start: usize,
    pub(super) link: Option<Arc<str>>,
}

/// `pulldown-cmark` treats single and double tildes as strikethrough.
/// Agent prose commonly uses single tildes as literals, so this adapter
/// preserves them and permits strikethrough only for `~~` spans.
pub(super) struct DoubleTildeOnly<'a> {
    pub(super) src: &'a str,
    pub(super) inner: pulldown_cmark::OffsetIter<'a>,
    pub(super) literal: Vec<bool>,
}

impl<'a> DoubleTildeOnly<'a> {
    pub(super) fn new(src: &'a str) -> Self {
        Self {
            src,
            inner: MdParser::new_ext(src, MdOptions::ENABLE_STRIKETHROUGH).into_offset_iter(),
            literal: Vec::new(),
        }
    }
}

impl<'a> Iterator for DoubleTildeOnly<'a> {
    type Item = (Event<'a>, std::ops::Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        let (event, range) = self.inner.next()?;
        match event {
            Event::Start(MdTag::Strikethrough) => {
                if self.src[range.start..].starts_with("~~") {
                    self.literal.push(false);
                    Some((Event::Start(MdTag::Strikethrough), range))
                } else {
                    self.literal.push(true);
                    Some((Event::Text("~".into()), range.start..range.start + 1))
                }
            }
            Event::End(TagEnd::Strikethrough) => {
                if self.literal.pop().unwrap_or(false) {
                    Some((Event::Text("~".into()), range.end - 1..range.end))
                } else {
                    Some((Event::End(TagEnd::Strikethrough), range))
                }
            }
            other => Some((other, range)),
        }
    }
}

/// Render one source line to styled spans with source-offset tracking, using
/// `pulldown-cmark` for `CommonMark` inline parsing. Unterminated markers stay
/// literal, so a still-growing streamed line keeps the same laid-out width
/// until its span closes.
pub(super) fn inline_spans_mapped(line: &str, t: Theme, base: Style) -> Vec<MappedSpan> {
    let code_style = Style::new().fg(t.info).bg(t.inline_bg);
    let mut out: Vec<MappedSpan> = Vec::new();
    // Active inline formatting, outermost first. Each entry carries the byte
    // offset of its opening marker so nested content snaps its selection
    // boundary to the outermost open, plus the modifier and link it applies.
    let mut fmt_stack: Vec<(usize, Modifier, Option<Arc<str>>)> = Vec::new();

    let current_style = |fmt_stack: &[(usize, Modifier, Option<Arc<str>>)]| {
        fmt_stack
            .iter()
            .fold(base, |style, (_, m, _)| style.add_modifier(*m))
    };
    let current_link = |fmt_stack: &[(usize, Modifier, Option<Arc<str>>)]| {
        fmt_stack.iter().rev().find_map(|(_, _, l)| l.clone())
    };
    let boundary_of = |fmt_stack: &[(usize, Modifier, Option<Arc<str>>)], fallback: usize| {
        fmt_stack.first().map_or(fallback, |(open, _, _)| *open)
    };

    for (event, range) in DoubleTildeOnly::new(line) {
        match event {
            Event::Start(tag) => {
                let (modifier, link) = match &tag {
                    MdTag::Strong => (Some(Modifier::BOLD), None),
                    MdTag::Emphasis => (Some(Modifier::ITALIC), None),
                    MdTag::Strikethrough => (Some(Modifier::CROSSED_OUT), None),
                    MdTag::Link { dest_url, .. } => {
                        // Only hyperlink safe URLs (non-empty, no control or
                        // whitespace); an unsafe destination renders as plain
                        // underlined text without a clickable target.
                        let url = dest_url.as_ref();
                        let link = is_safe_link_url(url).then(|| Arc::from(url));
                        (Some(Modifier::UNDERLINED), link)
                    }
                    _ => (None, None),
                };
                if modifier.is_some() || link.is_some() {
                    fmt_stack.push((range.start, modifier.unwrap_or(Modifier::empty()), link));
                }
            }
            Event::End(tag) => {
                let pops = match tag {
                    TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => 1,
                    _ => 0,
                };
                for _ in 0..pops {
                    fmt_stack.pop();
                }
            }
            Event::Code(code) => {
                // Content-only (no backticks, no padding): the tile width is
                // stable across the span boundary.
                out.push(MappedSpan {
                    span: Span::styled(code.to_string(), code_style),
                    content_start: range.start + 1,
                    boundary_start: range.start,
                    link: None,
                });
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if text.is_empty() {
                    continue;
                }
                out.push(MappedSpan {
                    span: Span::styled(text.to_string(), current_style(&fmt_stack)),
                    content_start: range.start,
                    boundary_start: boundary_of(&fmt_stack, range.start),
                    link: current_link(&fmt_stack),
                });
            }
            Event::SoftBreak | Event::HardBreak => {
                out.push(MappedSpan {
                    span: Span::raw(" "),
                    content_start: range.start,
                    boundary_start: boundary_of(&fmt_stack, range.start),
                    link: current_link(&fmt_stack),
                });
            }
            _ => {}
        }
    }
    nonempty_mapped(out)
}

pub(super) fn nonempty_mapped(mut spans: Vec<MappedSpan>) -> Vec<MappedSpan> {
    if spans.is_empty() {
        spans.push(MappedSpan {
            span: Span::raw(String::new()),
            content_start: 0,
            boundary_start: 0,
            link: None,
        });
    }
    spans
}

pub(super) fn build_content_map(source_len: usize, spans: &[MappedSpan]) -> Vec<usize> {
    let mut full = Vec::new();
    for m in spans {
        let c = m.span.content.chars().count();
        // A span deep inside a formatting run reports the outermost opening
        // marker as its boundary, which precedes the previous span's content
        // (e.g. bold text after an inline code span). Fall back to the
        // span's own content start so the map never moves backwards; a
        // backwards entry panics selection slicing with begin > end.
        let boundary = match full.last() {
            Some(&prev) if m.boundary_start < prev => m.content_start,
            _ => m.boundary_start,
        };
        full.push(boundary);
        let chars: Vec<(usize, char)> = m.span.content.char_indices().collect();
        for k in 1..c {
            let off = chars.get(k).map_or(m.span.content.len(), |(b, _)| *b);
            full.push(m.content_start + off);
        }
    }
    full.push(source_len);
    // Drop leading-whitespace positions so the map is content-relative
    // (render excludes leading whitespace from the selectable range).
    let lead_ws = spans
        .iter()
        .flat_map(|m| m.span.content.chars())
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    if lead_ws < full.len() {
        full.drain(..lead_ws);
    }
    full
}

pub(super) fn split_map_by_rows(
    full_map: &[usize],
    lead_ws: usize,
    rows: &[ratatui::text::Line<'static>],
) -> Vec<Vec<usize>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut pos = 0usize;
    let mut first = true;
    for row in rows {
        let display_len: usize = row.spans.iter().map(|s| s.content.chars().count()).sum();
        let row_lead = if first { lead_ws } else { 0 };
        let content_len = display_len.saturating_sub(row_lead);
        let end = (pos + content_len + 1).min(full_map.len());
        out.push(full_map[pos..end].to_vec());
        pos += content_len;
        first = false;
    }
    out
}

pub(super) fn collapse(source: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut map = Vec::new();
    let mut byte_pos = 0usize;
    let mut prev_end: Option<usize> = None;
    for word in source.split(' ') {
        if word.is_empty() {
            byte_pos += 1;
            continue;
        }
        if let Some(pe) = prev_end {
            out.push(' ');
            map.push(pe);
        }
        for (b, c) in word.char_indices() {
            out.push(c);
            map.push(byte_pos + b);
        }
        byte_pos += word.len();
        prev_end = Some(byte_pos);
        byte_pos += 1;
    }
    (out, map)
}

pub(super) fn wrap_with_map(
    content: &str,
    prefix_len: usize,
    source_len: usize,
    max_w: usize,
) -> Vec<(String, Vec<usize>)> {
    let (collapsed, cmap) = collapse(content);
    let rows = prim::wrap(&collapsed, max_w);
    let mut offset = 0usize;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let len = row.chars().count();
        let mut map = Vec::with_capacity(len + 1);
        for k in 0..len {
            let src = cmap.get(offset + k).copied().unwrap_or(content.len());
            map.push(prefix_len + src);
        }
        let last = cmap.get(offset + len).copied().unwrap_or(content.len());
        map.push((prefix_len + last).min(source_len));
        // The first row's start boundary snaps to the source start so the
        // prefix (e.g. `## `, `> `) is included in whole-row yank — the
        // display strips it, but the raw markdown retains it. Continuation
        // rows keep their contiguous start (the next row's beginning).
        if out.is_empty() {
            map[0] = 0;
        }
        out.push((row, map));
        offset += len;
    }
    out
}
