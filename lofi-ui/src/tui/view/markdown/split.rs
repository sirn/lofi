use super::emit::spans_text;
use super::inline::{inline_spans_mapped, MappedSpan};
use super::model::{MdBlock, MdQuoteLine};
use crate::tui::theme::Theme;
use ratatui::style::Style;
use ratatui::text::Span;

/// Split a paragraph block that spans multiple source lines into one
/// `Paragraph` per line, preserving each line's source range and inline parse.
pub(super) fn split_paragraph_lines(
    text: &str,
    src: &std::ops::Range<usize>,
    t: Theme,
    base: Style,
    out: &mut Vec<MdBlock>,
) {
    // Block ranges may fold in a trailing newline; strip it so the split does
    // not yield a phantom empty final line.
    let block = text[src.clone()].trim_end_matches('\n');
    if !block.contains('\n') {
        // Single line: keep spans as analyzed.
        let spans = line_spans(block.trim_end(), t, base);
        let mut spans = spans;
        for m in &mut spans {
            m.content_start += src.start;
            m.boundary_start += src.start;
        }
        if !spans_text(&spans).trim().is_empty() {
            out.push(MdBlock::Paragraph {
                spans,
                src: src.clone(),
            });
        }
        return;
    }
    let mut off = src.start;
    for line in block.split('\n') {
        let trimmed = line.trim_end();
        let line_range = off..off + trimmed.len();
        let spans = line_spans(trimmed, t, base);
        let mut spans = spans;
        for m in &mut spans {
            m.content_start += off;
            m.boundary_start += off;
        }
        if trimmed.trim().is_empty() {
            out.push(MdBlock::Blank {
                src: off..off + line.len(),
            });
        } else {
            out.push(MdBlock::Paragraph {
                spans,
                src: line_range,
            });
        }
        off += line.len() + 1;
    }
}

/// Byte length of the list marker (bullet or ordered) plus its trailing
/// whitespace at the start of `line`, after any indentation. Returns 0 when
/// the line does not open with a list marker.
pub(super) fn list_marker_len(line: &str) -> usize {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let rest = &line[indent..];
    let after_bullet = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "));
    if let Some(r) = after_bullet {
        return indent + (rest.len() - r.len());
    }
    // Ordered list: `N.` or `N)` followed by a space.
    let digit_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len > 0 {
        if let Some(r) = rest[digit_len..]
            .strip_prefix(". ")
            .or_else(|| rest[digit_len..].strip_prefix(") "))
        {
            return indent + (rest.len() - r.len());
        }
    }
    0
}

/// Parse one source line into spans, preserving a leading list marker as
/// literal text. The renderer is line-preserving: a list line such as
/// `- item` must keep its `- ` prefix on screen, but `pulldown`'s block
/// parser consumes the marker, so re-parsing the whole line would drop it.
/// The marker is emitted verbatim and only the item body is inline-parsed.
pub(super) fn line_spans(line: &str, t: Theme, base: Style) -> Vec<MappedSpan> {
    let marker_len = list_marker_len(line);
    if marker_len == 0 {
        return inline_spans_mapped(line, t, base);
    }
    let marker = &line[..marker_len];
    let body = &line[marker_len..];
    let mut spans = Vec::with_capacity(2);
    spans.push(MappedSpan {
        span: Span::styled(marker.to_string(), Style::new().fg(t.muted)),
        content_start: 0,
        boundary_start: 0,
        link: None,
    });
    if !body.is_empty() {
        for m in inline_spans_mapped(body, t, base) {
            spans.push(MappedSpan {
                span: m.span,
                content_start: m.content_start + marker_len,
                boundary_start: m.boundary_start + marker_len,
                link: m.link,
            });
        }
    }
    spans
}

/// Split a fenced code body into per-source-line strings with byte ranges.
/// `body_range` is the concatenated body text range; when absent (empty block)
/// there are no body lines.
pub(super) fn split_code_lines(
    body: &str,
    body_range: Option<std::ops::Range<usize>>,
) -> Vec<(String, std::ops::Range<usize>)> {
    let Some(br) = body_range else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let base = br.start;
    let mut line_start = 0usize;
    for (i, &b) in body.as_bytes().iter().enumerate() {
        if b == b'\n' {
            out.push((body[line_start..i].to_string(), base + line_start..base + i));
            line_start = i + 1;
        }
    }
    if line_start < body.len() {
        out.push((
            body[line_start..].to_string(),
            base + line_start..base + body.len(),
        ));
    }
    out
}

/// Build per-source-line quote content from the quote block source range. Each
/// line keeps its `> ` prefix length (for yank) and inline-parsed body.
pub(super) fn quote_lines(
    text: &str,
    src: &std::ops::Range<usize>,
    t: Theme,
    base: Style,
) -> Vec<MdQuoteLine> {
    // The quote block range may include a trailing newline; strip it so
    // `split` does not yield a phantom empty final line.
    let block = text[src.clone()].trim_end_matches('\n');
    let mut out = Vec::new();
    let mut off = src.start;
    for line in block.split('\n') {
        let trimmed = line.trim_end();
        let line_len = trimmed.len();
        let prefix_len = if trimmed == ">" {
            1
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            line_len - rest.len()
        } else {
            0
        };
        let body = &trimmed[prefix_len..];
        let spans = if body.is_empty() {
            Vec::new()
        } else {
            let mut s = inline_spans_mapped(body, t, base);
            for m in &mut s {
                m.content_start += off + prefix_len;
                m.boundary_start += off + prefix_len;
            }
            s
        };
        out.push(MdQuoteLine {
            spans,
            src: off..off + line_len,
            prefix_len,
        });
        off += line.len() + 1;
    }
    out
}
