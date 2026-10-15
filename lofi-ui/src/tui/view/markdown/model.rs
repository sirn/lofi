use super::inline::{inline_spans_mapped, MappedSpan};
use super::table::Align;
use crate::tui::theme::Theme;
use ratatui::style::Style;

// `analyze` is the single structural entry point for markdown. One
// `into_offset_iter` pass produces a flat `Vec<MdBlock>` in which every block
// carries the source byte ranges needed to rebuild yank/selection raw text.
// Both `render_markdown_body` and `markdown_body_height` walk this same tree,
// so the render/height row contract (guarded by `markdown_height_matches_lines`)
// holds by construction rather than by two parallel line-scanners that drift.

/// A table cell: inline content plus the source range it came from.
#[derive(Clone)]
pub(super) struct MdCell {
    /// Source byte range of the cell text (without surrounding `|`/padding).
    pub(super) src: std::ops::Range<usize>,
}

/// One source line inside a block quote.
#[derive(Clone)]
pub(super) struct MdQuoteLine {
    pub(super) spans: Vec<MappedSpan>,
    /// Byte range of the full source line including the `>` marker.
    pub(super) src: std::ops::Range<usize>,
    /// Byte length of the `> `/`>` prefix.
    pub(super) prefix_len: usize,
}

/// One structural block of a markdown document.
#[derive(Clone)]
pub(super) enum MdBlock {
    /// A run of one or more blank source lines (rendered as empty rows).
    Blank { src: std::ops::Range<usize> },
    Paragraph {
        spans: Vec<MappedSpan>,
        src: std::ops::Range<usize>,
    },
    Heading {
        level: u8,
        src: std::ops::Range<usize>,
        /// Byte offset of the first content char (after the `# ` prefix).
        content_start: usize,
    },
    Code {
        lang: Option<String>,
        /// Body source lines with byte ranges (fence markers excluded).
        lines: Vec<(String, std::ops::Range<usize>)>,
        fence: std::ops::Range<usize>,
    },
    Table {
        header: Vec<MdCell>,
        aligns: Vec<Align>,
        rows: Vec<Vec<MdCell>>,
        src: std::ops::Range<usize>,
    },
    Quote {
        lines: Vec<MdQuoteLine>,
        src: std::ops::Range<usize>,
    },
}

/// Tracks the tightest source extent of one inline-bearing node while the block
/// pass streams events. The node tag range is the outer bound; the inner text
/// extent lets us re-parse exactly the inline sub-slice.
#[derive(Clone, Copy)]
pub(super) struct InlineExtent {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) seen: bool,
}

impl InlineExtent {
    pub(super) fn new() -> Self {
        Self {
            start: 0,
            end: 0,
            seen: false,
        }
    }
    pub(super) fn observe(&mut self, range: &std::ops::Range<usize>) {
        if self.seen {
            self.start = self.start.min(range.start);
            self.end = self.end.max(range.end);
        } else {
            self.start = range.start;
            self.end = range.end;
            self.seen = true;
        }
    }
}

/// Re-parse the inline sub-slice with the shared inline machine and shift its
/// span offsets to global source bytes. The extent is narrowed to non-whitespace
/// content first: pulldown tag ranges (notably table cells) include surrounding
/// padding, and cell spans must point at the trimmed text.
pub(super) fn inline_extent_spans(
    ext: &InlineExtent,
    text: &str,
    t: Theme,
    base: Style,
) -> Vec<MappedSpan> {
    let raw = &text[ext.start..ext.end];
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace());
    if trimmed.is_empty() {
        return Vec::new();
    }
    let lead = raw.len() - raw.trim_start_matches(|c: char| c.is_whitespace()).len();
    let base_off = ext.start + lead;
    let mut spans = inline_spans_mapped(trimmed, t, base);
    for m in &mut spans {
        m.content_start += base_off;
        m.boundary_start += base_off;
    }
    spans
}

/// The sub-range of `ext` with leading/trailing whitespace removed, or `None`
/// when the extent is entirely whitespace.
pub(super) fn trimmed_range(text: &str, ext: &InlineExtent) -> Option<std::ops::Range<usize>> {
    if !ext.seen {
        return None;
    }
    let raw = &text[ext.start..ext.end];
    let lead = raw.len() - raw.trim_start_matches(|c: char| c.is_whitespace()).len();
    let trail = raw.len() - raw.trim_end_matches(|c: char| c.is_whitespace()).len();
    if lead + trail >= raw.len() {
        return None;
    }
    Some(ext.start + lead..ext.end - trail)
}

/// Numeric level (1..=6) for a `HeadingLevel`.
pub(super) fn heading_level_num(level: pulldown_cmark::HeadingLevel) -> u8 {
    use pulldown_cmark::HeadingLevel as H;
    match level {
        H::H1 => 1,
        H::H2 => 2,
        H::H3 => 3,
        H::H4 => 4,
        H::H5 => 5,
        H::H6 => 6,
    }
}

/// In-progress block frames for `analyze`. The renderer is flat (no nested
/// block containers), so an explicit stack suffices; quote/table frames own
/// their inline extents and no nested block events are expected at depth.
pub(super) enum BlockFrame {
    Para {
        ext: InlineExtent,
        src: std::ops::Range<usize>,
    },
    Heading {
        level: u8,
        ext: InlineExtent,
        src: std::ops::Range<usize>,
    },
    Code {
        lang: Option<String>,
        fence: std::ops::Range<usize>,
        body: String,
        body_range: Option<std::ops::Range<usize>>,
    },
    Table {
        aligns: Vec<Align>,
        src: std::ops::Range<usize>,
        header: Vec<MdCell>,
        rows: Vec<Vec<MdCell>>,
        cur: Vec<MdCell>,
        in_head: bool,
        cell: Option<(InlineExtent, std::ops::Range<usize>)>,
    },
    Quote {
        src: std::ops::Range<usize>,
    },
    /// The outermost list. Rendered line-preserving (each source line a
    /// paragraph, markers and indent intact), so the frame captures the source
    /// range plus a depth counter for the nested lists pulldown reports inside
    /// it (their Start/End must not close this frame).
    List {
        src: std::ops::Range<usize>,
        depth: usize,
    },
}

/// True when a container frame (block quote or list) remains on the stack.
/// Nested blocks inside a container are rendered by the container, so the block
/// pass must not also emit them at top level.
pub(super) fn stack_in_container(stack: &[BlockFrame]) -> bool {
    stack
        .iter()
        .any(|f| matches!(f, BlockFrame::Quote { .. } | BlockFrame::List { .. }))
}

/// Route an inline event range to the innermost inline-owning frame. The
/// structure is flat (no nested inline containers), so only the top frame can
/// own inline extents; table cells delegate to their open cell.
pub(super) fn observe_inline(stack: &mut [BlockFrame], range: &std::ops::Range<usize>) {
    match stack.last_mut() {
        Some(BlockFrame::Para { ext, .. } | BlockFrame::Heading { ext, .. }) => {
            ext.observe(range);
        }
        Some(BlockFrame::Table {
            cell: Some((cext, _)),
            ..
        }) => cext.observe(range),
        _ => {}
    }
}
