use super::model::{
    heading_level_num, inline_extent_spans, observe_inline, stack_in_container, trimmed_range,
    BlockFrame, InlineExtent, MdBlock, MdCell,
};
use super::split::{quote_lines, split_code_lines, split_paragraph_lines};
use super::table::Align;
use crate::tui::theme::Theme;
use pulldown_cmark::{Event, Options as MdOptions, Parser as MdParser, Tag as MdTag, TagEnd};
use ratatui::style::Style;

/// Parse `text` into structural blocks via a single `pulldown-cmark` pass.
pub(super) fn analyze(text: &str, t: Theme, base: Style) -> Vec<MdBlock> {
    if text.is_empty() {
        return Vec::new();
    }
    let opts = MdOptions::ENABLE_STRIKETHROUGH
        | MdOptions::ENABLE_TABLES
        | MdOptions::ENABLE_FOOTNOTES
        | MdOptions::ENABLE_TASKLISTS
        | MdOptions::ENABLE_HEADING_ATTRIBUTES;
    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut stack: Vec<BlockFrame> = Vec::new();

    for (ev, range) in MdParser::new_ext(text, opts).into_offset_iter() {
        match ev {
            // CommonMark treats tags like `<input>` as HTML, not text.
            // Keep them as paragraphs so the transcript shows the source.
            Event::Start(MdTag::Paragraph | MdTag::HtmlBlock) => {
                stack.push(BlockFrame::Para {
                    ext: InlineExtent::new(),
                    src: range,
                });
            }
            Event::End(TagEnd::Paragraph | TagEnd::HtmlBlock) => {
                if let Some(BlockFrame::Para { ext, src }) = stack.pop() {
                    let spans = if ext.seen {
                        inline_extent_spans(&ext, text, t, base)
                    } else {
                        Vec::new()
                    };
                    // Skip source-blank paragraphs (whitespace-only): they carry
                    // no renderable content and would emit a spurious block. A
                    // paragraph inside a block quote is owned by that quote.
                    if !spans.is_empty() && !stack_in_container(&stack) {
                        blocks.push(MdBlock::Paragraph { spans, src });
                    }
                }
            }
            Event::Start(MdTag::Heading { level, .. }) => stack.push(BlockFrame::Heading {
                level: heading_level_num(level),
                ext: InlineExtent::new(),
                src: range,
            }),
            Event::End(TagEnd::Heading(_)) => {
                if let Some(BlockFrame::Heading { level, ext, src }) = stack.pop() {
                    // `InlineExtent` initializes `start` to 0 before any inline event is
                    // observed. A bare heading marker (`#` alone, e.g. mid-stream) yields
                    // no inline events, so `ext.start` is still 0 while `src.start > 0`,
                    // and `content_start - src.start` would underflow. Clamp to the block
                    // range: with no content the heading renders as one row regardless.
                    let content_start = if ext.seen {
                        ext.start.max(src.start)
                    } else {
                        src.start
                    };
                    blocks.push(MdBlock::Heading {
                        level,
                        content_start,
                        src,
                    });
                }
            }
            Event::Start(MdTag::CodeBlock(kind)) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(l) if !l.is_empty() => {
                        Some(l.to_string())
                    }
                    _ => None,
                };
                stack.push(BlockFrame::Code {
                    lang,
                    fence: range,
                    body: String::new(),
                    body_range: None,
                });
            }
            Event::Text(t) => {
                if let Some(BlockFrame::Code {
                    body, body_range, ..
                }) = stack.last_mut()
                {
                    body.push_str(&t);
                    *body_range = Some(match body_range.take() {
                        Some(br) => br.start..range.end,
                        None => range,
                    });
                } else {
                    observe_inline(&mut stack, &range);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(BlockFrame::Code {
                    lang,
                    fence,
                    body,
                    body_range,
                }) = stack.pop()
                {
                    let lines = split_code_lines(&body, body_range);
                    blocks.push(MdBlock::Code { lang, lines, fence });
                }
            }
            Event::Start(MdTag::Table(aligns)) => stack.push(BlockFrame::Table {
                aligns: aligns
                    .iter()
                    .map(|a| match a {
                        pulldown_cmark::Alignment::Center => Align::Center,
                        pulldown_cmark::Alignment::Right => Align::Right,
                        _ => Align::Left,
                    })
                    .collect(),
                src: range,
                header: Vec::new(),
                rows: Vec::new(),
                cur: Vec::new(),
                in_head: false,
                cell: None,
            }),
            Event::Start(MdTag::TableHead) => {
                if let Some(BlockFrame::Table { in_head, .. }) = stack.last_mut() {
                    *in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(BlockFrame::Table {
                    in_head,
                    header,
                    cur,
                    ..
                }) = stack.last_mut()
                {
                    *in_head = false;
                    *header = std::mem::take(cur);
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(BlockFrame::Table {
                    in_head, rows, cur, ..
                }) = stack.last_mut()
                {
                    if !*in_head {
                        rows.push(std::mem::take(cur));
                    }
                }
            }
            Event::Start(MdTag::TableCell) => {
                if let Some(BlockFrame::Table { cell, .. }) = stack.last_mut() {
                    *cell = Some((InlineExtent::new(), range));
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(BlockFrame::Table { cell, cur, .. }) = stack.last_mut() {
                    if let Some((cext, csrc)) = cell.take() {
                        let src = trimmed_range(text, &cext).unwrap_or(csrc);
                        cur.push(MdCell { src });
                    }
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(BlockFrame::Table {
                    aligns,
                    src,
                    header,
                    rows,
                    ..
                }) = stack.pop()
                {
                    blocks.push(MdBlock::Table {
                        header,
                        aligns,
                        rows,
                        src,
                    });
                }
            }
            Event::Start(MdTag::BlockQuote(_)) => stack.push(BlockFrame::Quote { src: range }),
            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(BlockFrame::Quote { src }) = stack.pop() {
                    let lines = quote_lines(text, &src, t, base);
                    blocks.push(MdBlock::Quote { lines, src });
                }
            }
            // Lists are rendered line-preserving: only the outermost list frame
            // is captured (nested lists fall inside its range), and on close the
            // whole range is split into per-line paragraphs keeping the literal
            // markers and indentation.
            Event::Start(MdTag::List(_)) => {
                match stack.last_mut() {
                    // Already inside a list or quote: count the nested list so
                    // its End does not close the outer frame.
                    Some(BlockFrame::List { depth, .. }) => *depth += 1,
                    Some(BlockFrame::Quote { .. }) => {}
                    _ => stack.push(BlockFrame::List {
                        src: range,
                        depth: 0,
                    }),
                }
            }
            Event::End(TagEnd::List(_)) => {
                if let Some(BlockFrame::List { src, depth }) = stack.last_mut() {
                    if *depth > 0 {
                        *depth -= 1;
                    } else {
                        let src = src.clone();
                        stack.pop();
                        split_paragraph_lines(text, &src, t, base, &mut blocks);
                    }
                }
            }
            _ => observe_inline(&mut stack, &range),
        }
    }
    preserve_lines(text, blocks, t, base)
}

/// The source byte range a block occupies.
pub(super) fn block_src(b: &MdBlock) -> std::ops::Range<usize> {
    match b {
        MdBlock::Blank { src }
        | MdBlock::Paragraph { src, .. }
        | MdBlock::Code { fence: src, .. }
        | MdBlock::Heading { src, .. }
        | MdBlock::Table { src, .. }
        | MdBlock::Quote { src, .. } => src.clone(),
    }
}

/// Restore source-line granularity over the pulldown block structure.
///
/// pulldown is structure-semantic: it collapses blank lines and merges
/// soft-wrapped lines into a single paragraph. The transcript renderer is
/// line-preserving — a user's blank lines and explicit line breaks must each
/// occupy their own row (the user/agent rail is drawn per source line). This
/// pass walks the emitted blocks against the raw source and:
/// - inserts a `Blank` block for every run of empty source lines between or
///   around blocks, and
/// - splits any paragraph that spans multiple source lines into one paragraph
///   per line (pulldown still owns the inline parse of each line).
///
/// Block classification remains entirely pulldown's; this only enforces line
/// granularity for rendering.
pub(super) fn preserve_lines(
    text: &str,
    blocks: Vec<MdBlock>,
    t: Theme,
    base: Style,
) -> Vec<MdBlock> {
    let mut out: Vec<MdBlock> = Vec::new();
    // `cursor` tracks the byte just past the prior block's content. Block
    // ranges from pulldown inconsistently include a trailing newline, so the
    // cursor is normalized to the end of the block's last content line
    // (trailing newlines excluded) before each gap is measured.
    let mut cursor = 0usize;
    for block in blocks {
        let src = block_src(&block);
        // Fill any blank-line gap before this block.
        emit_blanks(text, cursor, src.start, &mut out);
        // Split multi-line paragraphs into per-line paragraphs.
        match block {
            MdBlock::Paragraph { src, .. } => {
                split_paragraph_lines(text, &src, t, base, &mut out);
            }
            other => out.push(other),
        }
        cursor = content_end(text, &src);
    }
    // Trailing blank lines after the last block.
    emit_blanks(text, cursor, text.len(), &mut out);
    out
}

/// The byte offset just past a block's last content character, excluding any
/// trailing newline(s) pulldown folded into the block range.
pub(super) fn content_end(text: &str, src: &std::ops::Range<usize>) -> usize {
    let mut end = src.end;
    while end > src.start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    end
}

/// Emit a `Blank` block for each empty source line in `text[start..end]`,
/// tracking byte offsets so each blank maps to its (empty) source line.
pub(super) fn emit_blanks(text: &str, start: usize, end: usize, out: &mut Vec<MdBlock>) {
    if start >= end {
        return;
    }
    // The gap between two blocks opens with the newline terminating the prior
    // block's last line; that separator is not itself a blank line. Each
    // additional newline introduces one blank line.
    let gap = &text[start..end];
    let newlines = gap.bytes().filter(|&b| b == b'\n').count();
    let blank_count = newlines.saturating_sub(1);
    // Byte offset of each blank line: walk the gap's lines, skipping the first
    // fragment (the separator's empty head) and the last (the next block's line
    // start), emitting one Blank per interior empty line.
    let mut off = start;
    let mut emitted = 0usize;
    for (i, line) in gap.split('\n').enumerate() {
        let line_end = off + line.len();
        if i > 0 && emitted < blank_count {
            out.push(MdBlock::Blank { src: off..line_end });
            emitted += 1;
        }
        off = line_end + 1;
    }
}
