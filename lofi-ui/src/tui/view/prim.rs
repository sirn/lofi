//! Atomic rendering primitives shared by every component: line padding,
//! truncation, word-wrap, duration formatting, and styled-span constructors.
//!
//! Components compose these into larger units; nothing here knows about
//! turns or blocks.

use std::sync::Arc;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::tui::theme::{active_indicator, Theme};
use crate::tui::SPINNER;

/// Display width of `s` in terminal cells: wide chars (CJK, emoji) count as
/// 2, combining/control chars as 0. This is what every `used`/`avail`/cursor
/// calculation must use instead of `chars().count()`.
pub fn width(s: &str) -> usize {
    s.width()
}

/// Byte offset of the `char_idx`-th char of `s` (i.e. where the substring
/// starting at that char begins). `char_idx == char_count` yields `s.len()`.
fn char_byte_offset(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(b, _)| b)
}

/// Paint a selection background over the char range `[start, end)` of a line
/// by splitting its spans at the boundaries. The original foreground and
/// modifiers are preserved; only the background is swapped for `bg`.
/// Overlay `bg` across every span on the line, preserving foreground colors.
/// Used for the Navigate cursor-line highlight so it wins over per-span
/// backgrounds (e.g. Exec tile fills) that would otherwise hide a plain
/// `Line::style` background.
pub fn apply_line_bg(line: &mut Line<'static>, bg: Color) {
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
}

pub fn apply_selection(line: &mut Line<'static>, start: usize, end: usize, bg: Color) {
    if start >= end {
        return;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut pos = 0usize;
    for span in line.spans.drain(..) {
        let style = span.style;
        let content = span.content;
        let n = content.chars().count();
        let span_start = pos;
        let span_end = pos + n;
        pos = span_end;
        if span_end <= start || span_start >= end {
            out.push(Span::styled(content, style));
            continue;
        }
        let lo = start.saturating_sub(span_start).min(n);
        let hi = end.saturating_sub(span_start).min(n);
        let b_lo = char_byte_offset(&content, lo);
        let b_hi = char_byte_offset(&content, hi);
        if lo > 0 {
            out.push(Span::styled(content[..b_lo].to_string(), style));
        }
        let sel_style = style.bg(bg);
        out.push(Span::styled(
            content[b_lo..b_hi].to_string(),
            sel_style,
        ));
        if hi < n {
            out.push(Span::styled(content[b_hi..].to_string(), style));
        }
    }
    line.spans = out;
}

/// A blank gap line on the default background, used to separate components.
pub fn blank() -> Line<'static> {
    Line::default()
}

/// A rendered log line paired with the char range of its selectable
/// *content* within [`Self::line`]. Decoration (gutter, rails, line numbers,
/// branch glyphs, status icons) sits before `content.0`; trailing padding
/// sits after `content.1`. Selection — both the highlight and the copied
/// text — is clamped to `content`, so it covers exactly the meaningful text
/// and never the surrounding decoration, while content's own leading spaces
/// (indentation) are preserved.
/// Raw markdown source backing a rendered line, for clipboard yank.
/// Present only for lines derived from a markdown source (assistant text,
/// code fences, user messages); `None` for decoration (borders, blanks,
/// tool glyphs) where the rendered text is the canonical form.
///
/// `map` translates a *content-relative* display position (0 = before the
/// first selectable content char, `len` = after the last) to a byte offset
/// within `source`. Markdown markers stripped during rendering (the
/// `**` in `**bold**`, backticks, `## ` prefixes) create gaps in the map,
/// so a display selection `[cs, ce)` slices the raw source as
/// `source[map[cs]..map[ce]]` — markers wrapped around the selection are
/// included, while decoration (gutter, padding) is excluded. Soft-wrap
/// continuation rows share their source line with the preceding row; their
/// maps are contiguous (`row[i].map[last] == row[i+1].map[0]`), so a
/// multi-row selection within one source line is a single source slice,
/// and a `\n` is inserted only at hard breaks (a new source line).
#[derive(Clone, Debug)]
pub struct RawLine {
    /// The full source line (markdown markers intact).
    pub source: Arc<str>,
    /// Content-relative display position (0..=`content_len`) → source byte
    /// offset. Empty when no per-char mapping is available (whole-row yank
    /// still uses `source`; partial falls back to rendered text).
    pub map: Vec<usize>,
    /// `true` on the first visual row of a source line (begun at a hard
    /// newline); `false` on soft-wrap continuations.
    pub hard_break: bool,
}

impl RawLine {
    /// Build a raw line with a position map.
    pub fn new(source: Arc<str>, map: Vec<usize>, hard_break: bool) -> Self {
        Self { source, map, hard_break }
    }

    /// Build a raw line whose entire source is one unbroken span (no
    /// interior markers to skip): the map is the identity `0..=len`. Used
    /// for code and other pre-formatted content where display chars map 1:1
    /// to a contiguous source range starting at `start`.
    pub fn linear(source: Arc<str>, start: usize, display_len: usize, hard_break: bool) -> Self {
        let map = (0..=display_len).map(|k| start + k).collect();
        Self { source, map, hard_break }
    }
}

pub struct RenderLine {
    pub line: Line<'static>,
    pub content: (usize, usize),
    /// Raw source for yank; `None` when the rendered text is canonical.
    pub raw: Option<RawLine>,
}

impl RenderLine {
    /// Number of selectable-content chars on this line (excludes decoration
    /// and trailing padding). Stable across a re-wrap, so the cumulative sum
    /// up to a line is a content anchor for re-seating the cursor.
    pub fn content_len(&self) -> usize {
        self.content.1.saturating_sub(self.content.0)
    }

    /// Attach raw markdown source to this line for clipboard yank.
    pub fn with_raw(mut self, raw: RawLine) -> Self {
        self.raw = Some(raw);
        self
    }
}

fn char_count(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

/// A stashed *visible* log line: the rendered plain text, selectable
/// content char range, and optional raw markdown source for yank.
/// Populated at render time from [`RenderLine`] so mouse selection and
/// cursor tracking can map screen coords back to text without re-rendering.
pub struct VisLine {
    /// Rendered plain text (all spans concatenated).
    pub rendered: String,
    /// Selectable content char range (excludes decoration and padding).
    pub content: (usize, usize),
    /// Raw markdown source for yank; `None` when the rendered text is
    /// canonical (decoration, tool glyphs, etc.).
    pub raw: Option<RawLine>,
}

/// Build a line from `deco` + `content` + optional `suffix` (trailing
/// decoration, e.g. a dash fill), recording the content char range. The
/// suffix is rendered but excluded from selection.
pub fn render(
    deco: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    suffix: Vec<Span<'static>>,
) -> RenderLine {
    let deco_len = char_count(&deco);
    let content_len = char_count(&content);
    // Exclude leading whitespace from the selectable content range.
    // `wrap_pre` re-prepends a line's indent to every wrapped row for
    // alignment; counting it would make the cumulative content length (and
    // thus the Navigate cursor's content anchor) depend on the number of
    // wraps — i.e. on width. The indent is still rendered (visible) but not
    // selectable, so the anchor tracks only the line's actual text.
    let lead_ws = content
        .iter()
        .flat_map(|s| s.content.chars())
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    let start = deco_len + lead_ws;
    let end = deco_len + content_len;
    let mut all = deco;
    all.extend(content);
    all.extend(suffix);
    RenderLine {
        line: Line::from(all),
        content: (start, end),
        raw: None,
    }
}

/// A plain line with no trailing decoration.
pub fn rline(deco: Vec<Span<'static>>, content: Vec<Span<'static>>) -> RenderLine {
    render(deco, content, Vec::new())
}

/// A blank gap line: nothing selectable.
pub fn rblank() -> RenderLine {
    RenderLine {
        line: Line::default(),
        content: (0, 0),
        raw: None,
    }
}

/// A `bg` tile padded to the full `width`. The padding tail is trailing
/// decoration — outside the content range — so the background spans
/// edge-to-edge without ever being selected or copied.
pub fn rtile(
    deco: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    bg: Color,
    width: usize,
) -> RenderLine {
    let mut rl = render(deco, content, Vec::new());
    let used = char_count(&rl.line.spans);
    rl.line
        .spans
        .push(Span::styled(" ".repeat(width.saturating_sub(used)), Style::new().bg(bg)));
    rl
}

/// Styled wrapped tile: word-wraps a line of pre-styled spans (preserving
/// each span's color/modifier across wraps) to fit within `width` minus the
/// decoration width, emitting one `RenderLine` per wrapped row. The first
/// row carries `deco`; continuation rows carry `cont_deco` (typically an
/// indented variant). Each row is padded to `width` with `bg`.
///
/// This is the wrapped counterpart to [`rtile`]: use it whenever the content
/// might exceed the available width (tool headers, result bodies, etc.).
/// Use [`rtile`] only when the content is guaranteed to fit (short labels,
/// padding rows, icons).
pub fn rtile_wrapped(
    deco: Vec<Span<'static>>,
    cont_deco: Vec<Span<'static>>,
    content: Vec<Span<'static>>,
    bg: Color,
    width: usize,
) -> Vec<RenderLine> {
    let deco_w = span_width(&deco);
    let avail = width.saturating_sub(deco_w);
    let line = Line::from(content);
    let wrapped = wrap_line_styled(&line, avail);
    let mut out = Vec::new();
    for (i, wl) in wrapped.into_iter().enumerate() {
        let d = if i == 0 { deco.clone() } else { cont_deco.clone() };
        out.push(rtile(d, wl.spans, bg, width));
    }
    if out.is_empty() {
        out.push(rtile(deco, vec![], bg, width));
    }
    out
}

/// Display width of a sequence of spans (sum of each span's content width).
fn span_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| width(&s.content)).sum()
}
/// Truncate `s` to at most `max_w` display cells, never splitting a wide
/// character: if the next char would overflow, it is dropped entirely.
/// Callers add `… (N hidden)` themselves when capping a list.
pub fn truncate(s: &str, max_w: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max_w {
            break;
        }
        out.push(c);
        w += cw;
    }
    out
}

/// Greedy word-wrap by display width so text fills the column exactly. A
/// word wider than `max_w` (e.g. a long CJK run with no spaces) is broken
/// mid-word on a wide-char boundary. Empty input yields a single empty
/// string so callers always emit at least one line.
///
/// Whitespace is collapsed as flow text: runs of spaces become a single
/// space and lines are trimmed. Styled, whitespace-preserving wrapping
/// lives in [`wrap_line_styled`]; both share [`wrap_cells`].
pub fn wrap(s: &str, max_w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.split('\n') {
        if max_w == 0 {
            out.push(line.to_string());
            continue;
        }
        // Collapse whitespace runs (flow text): drop the empties that
        // `split(' ')` yields for runs, then rejoin with single spaces.
        let cells: Vec<(char, Style)> = line
            .split(' ')
            .filter(|w| !w.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .map(|c| (c, Style::default()))
            .collect();
        for group in wrap_cells(&cells, max_w) {
            out.push(group.iter().map(|(c, _)| *c).collect());
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Indent-preserving greedy word-wrap for pre-formatted text — code,
/// command output, file contents — where leading whitespace and column
/// alignment carry meaning. Unlike [`wrap`] this never collapses whitespace:
/// the leading indent is stripped before wrapping (so the wrapper has no
/// leading spaces to break inside), then prepended to every output row so a
/// wrapped line stays aligned under its own indentation. The body breaks at
/// the last space that fits and hard-breaks a token wider than the content
/// width on a wide-char boundary; the single break space is dropped so
/// continuation rows start flush (matching [`wrap_line_styled`]). A blank or
/// empty source line yields one empty row so a line-number rail stays visible.
pub fn wrap_pre(s: &str, max_w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.split('\n') {
        if max_w == 0 || line.is_empty() {
            out.push(line.to_string());
            continue;
        }
        // Indent = leading ASCII whitespace, stripped before wrapping and
        // prepended to every output row.
        let indent_len = line.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
        let indent = &line[..indent_len];
        let body = &line[indent_len..];
        let content_w = max_w.saturating_sub(width(indent));
        if content_w == 0 {
            // Indent alone fills the row; emit verbatim (terminal clips).
            out.push(line.to_string());
            continue;
        }
        let cells: Vec<(char, Style)> = body.chars().map(|c| (c, Style::default())).collect();
        for group in wrap_cells(&cells, content_w) {
            out.push(format!("{indent}{}", group.iter().map(|(c, _)| *c).collect::<String>()));
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Shared greedy word-wrap core: break a styled cell run into `max_w`-wide
/// visual rows. Fills each row greedily, breaking at the last space that
/// fits; a token wider than `max_w` is hard-broken on a char boundary
/// (never splitting a wide char). The break space is dropped so
/// continuation rows start flush. Returns slices into `cells`, one per
/// row; empty input yields a single empty slice.
fn wrap_cells(cells: &[(char, Style)], max_w: usize) -> Vec<&[(char, Style)]> {
    let n = cells.len();
    let mut out: Vec<&[(char, Style)]> = Vec::new();
    let mut start = 0;
    while start < n {
        let mut w = 0usize;
        let mut k = start;
        let mut break_at = start;
        while k < n {
            let cw = cells[k].0.width().unwrap_or(0);
            if w + cw > max_w {
                break;
            }
            w += cw;
            if cells[k].0 == ' ' {
                break_at = k;
            }
            k += 1;
        }
        // Keep the break space in the slice (as trailing content) so the
        // cumulative selectable-content length stays stable across re-wraps —
        // dropping it would make the content anchor drift, since the number
        // of wraps (and thus dropped spaces) depends on width. The space fits
        // in the word-wrap case; when the line is already full and the next
        // char is a space it overflows by one cell and is clipped by the
        // renderer, staying invisible while still being counted.
        let mut end = if k == n {
            n
        } else if cells[k].0 == ' ' {
            k + 1 // boundary space: include it (clipped if it overflows)
        } else if break_at > start {
            break_at + 1 // word-wrap space: include it (it fit)
        } else {
            k // no whitespace to break at: hard-break at max_w
        };
        if end > n {
            end = n;
        }
        // A single char wider than max_w can't be split — emit it anyway.
        if end <= start {
            end = start + 1;
        }
        out.push(&cells[start..end]);
        start = end;
    }
    if out.is_empty() {
        out.push(&cells[0..0]);
    }
    out
}

/// Span-aware word-wrap of a styled [`Line`] to `max_w` display cells,
/// preserving per-span styles and all whitespace (unlike [`wrap`], which
/// collapses spaces for flow text). Breaks at the last space that fits; a
/// single token wider than `max_w` is broken on a wide-char boundary.
/// Empty input yields one empty line. Used for info-modal bodies so the
/// pre-wrap row count (for the scrollbar) and the rendered output agree.
pub fn wrap_line_styled(line: &Line<'static>, max_w: usize) -> Vec<Line<'static>> {
    if max_w == 0 {
        return vec![line.clone()];
    }
    let cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect();
    wrap_cells(&cells, max_w)
        .iter()
        .map(|group| cells_to_line(group))
        .collect()
}

/// Merge a run of (char, style) cells into a [`Line`], fusing adjacent
/// cells that share a style into one span.
fn cells_to_line(cells: &[(char, Style)]) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut cur: Option<Style> = None;
    for &(ch, st) in cells {
        if cur != Some(st) {
            if let Some(s) = cur.take() {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), s));
                }
            }
            cur = Some(st);
        }
        buf.push(ch);
    }
    if let Some(s) = cur.take() {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut buf), s));
        }
    }
    Line::from(spans)
}



/// Format a duration compactly: `12s` past ten seconds, `3.4s` below.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs >= 10.0 {
        format!("{}s", secs.round() as u64)
    } else {
        format!("{secs:.1}s")
    }
}

/// A two-cell `bg`-colored gutter, the left margin of exec-tree rows (no
/// indicator). Pairs with the `used` counts in the exec components, which
/// assume a 2-cell gutter.
pub fn gutter(bg: Color) -> Span<'static> {
    Span::styled("  ", Style::new().bg(bg))
}

/// A vertical rail `│ ` in the subtle tone.
pub fn rail(t: Theme, bg: Color) -> Span<'static> {
    Span::styled("│ ", Style::new().fg(t.subtle).bg(bg))
}

/// A tree branch connector: `└ ` when `is_last`, else `├ `.
pub fn branch(t: Theme, bg: Color, is_last: bool) -> Span<'static> {
    Span::styled(
        if is_last { "└ " } else { "├ " },
        Style::new().fg(t.subtle).bg(bg),
    )
}

/// A status icon: a spinner frame while `working`, `✗` on error, `✓` on done.
/// `frame` is the current spinner frame index (from [`Cx::spinner`]).
pub fn status_icon(
    t: Theme,
    bg: Color,
    working: bool,
    error: bool,
    frame: usize,
) -> Span<'static> {
    if working {
        Span::styled(
            format!("{} ", SPINNER[frame % SPINNER.len()]),
            Style::new().fg(active_indicator(t)).bg(bg),
        )
    } else if error {
        Span::styled("✗ ", Style::new().fg(t.error).bg(bg))
    } else {
        Span::styled("✓ ", Style::new().fg(t.success).bg(bg))
    }
}

/// Bold foreground span on `bg`.
pub fn bold(text: String, t: Theme, bg: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(t.fg).add_modifier(Modifier::BOLD).bg(bg))
}

/// Muted foreground span on `bg`.
pub fn muted(text: String, t: Theme, bg: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(t.muted).bg(bg))
}

/// Subtle foreground span on `bg`.
pub fn subtle(text: String, t: Theme, bg: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(t.subtle).bg(bg))
}

/// Plain foreground span on `bg`.
pub fn fg(text: String, t: Theme, bg: Color) -> Span<'static> {
    Span::styled(text, Style::new().fg(t.fg).bg(bg))
}

/// Total display width of a slice of spans.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

/// Horizontal arrangement of up to three span groups — left, center, right —
/// within a fixed `width`, built as a single [`Line`] with the leftover
/// space as padding. The line-level counterpart to the frame's `VStack`:
/// the caller declares what goes on each edge and this owns the gutter
/// between them. Left and right hug the edges; center sits in the middle of
/// the remainder. Content is not truncated here — abbreviate before feeding
/// in if it might overflow.
pub(crate) struct HStack<'a> {
    width: usize,
    left: Vec<Span<'a>>,
    center: Vec<Span<'a>>,
    right: Vec<Span<'a>>,
}

impl<'a> HStack<'a> {
    pub(crate) fn new(width: usize) -> Self {
        Self {
            width,
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
        }
    }

    pub(crate) fn left<I: IntoIterator<Item = Span<'a>>>(mut self, spans: I) -> Self {
        self.left = spans.into_iter().collect();
        self
    }

    #[allow(dead_code)] // part of the arrangement API; exercised by tests
    pub(crate) fn center<I: IntoIterator<Item = Span<'a>>>(mut self, spans: I) -> Self {
        self.center = spans.into_iter().collect();
        self
    }

    pub(crate) fn right<I: IntoIterator<Item = Span<'a>>>(mut self, spans: I) -> Self {
        self.right = spans.into_iter().collect();
        self
    }

    pub(crate) fn build(self) -> Line<'a> {
        let lw = spans_width(&self.left);
        let cw = spans_width(&self.center);
        let rw = spans_width(&self.right);
        // Space between the left and right edges after the pinned content.
        let rem = self.width.saturating_sub(lw + rw);
        // Center the center group within `rem`; the two half-gaps collapse
        // into a single gutter when there is no center content.
        let mid_gap = rem.saturating_sub(cw);
        let mut out = self.left;
        if self.center.is_empty() {
            out.push(Span::raw(" ".repeat(rem)));
        } else {
            let left_gap = mid_gap / 2;
            let right_gap = mid_gap - left_gap;
            out.push(Span::raw(" ".repeat(left_gap)));
            out.extend(self.center);
            out.push(Span::raw(" ".repeat(right_gap)));
        }
        out.extend(self.right);
        Line::from(out)
    }
}

/// A 1-cell-wide vertical scrollbar drawn in `track`. `position` is the
/// top visible row, `visible` the viewport height, `total` the full row
/// count. The thumb is sized proportional to `visible/total` and positioned
/// by `position`; nothing is drawn when everything fits.
///
/// The thumb uses the heavy box-drawing `┃` over a light `│` track — a
/// thin, calm indicator rather than a solid block.
pub fn render_scrollbar(
    f: &mut Frame,
    track: Rect,
    position: usize,
    visible: usize,
    total: usize,
    track_color: Color,
    thumb_color: Color,
) {
    if total == 0 || visible >= total || track.height == 0 {
        return;
    }
    let h = track.height as usize;
    let thumb_h = ((visible * h) / total).clamp(1, h);
    let max_pos = total.saturating_sub(visible);
    let max_top = h.saturating_sub(thumb_h);
    let thumb_top = position
        .checked_mul(max_top)
        .and_then(|n| n.checked_div(max_pos))
        .unwrap_or(0);
    let buf = f.buffer_mut();
    for y in 0..h {
        let cell = &mut buf[(track.x, track.y + y as u16)];
        let is_thumb = y >= thumb_top && y < thumb_top + thumb_h;
        cell.set_char(if is_thumb { '┃' } else { '│' });
        cell.set_fg(if is_thumb { thumb_color } else { track_color });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_of(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn hstack_left_right_fills_width_with_gutter() {
        let line = HStack::new(10)
            .left([Span::raw("ab")])
            .right([Span::raw("cd")])
            .build();
        assert_eq!(line.width(), 10);
        assert_eq!(spans_of(&line), "ab      cd");
    }

    #[test]
    fn hstack_center_is_centered() {
        let line = HStack::new(10)
            .center([Span::raw("ab")])
            .build();
        assert_eq!(line.width(), 10);
        assert_eq!(spans_of(&line), "    ab    ");
    }

    #[test]
    fn hstack_left_center_right() {
        let line = HStack::new(12)
            .left([Span::raw("L")])
            .center([Span::raw("C")])
            .right([Span::raw("R")])
            .build();
        assert_eq!(line.width(), 12);
        assert_eq!(spans_of(&line), "L    C     R");
    }

    #[test]
    fn wrap_collapses_whitespace_and_wraps() {
        // Flow text: runs of spaces collapse, lines wrap at word boundaries.
        let w = wrap("  aa   bb   cc dd", 7);
        assert_eq!(w, vec!["aa bb ", "cc dd"]);
    }

    #[test]
    fn wrap_breaks_long_word_at_width() {
        // A spaceless token longer than the width breaks at max_w chunks.
        let w = wrap("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(w, vec!["abcdefghij", "klmnopqrst", "uvwxyz"]);
    }

    #[test]
    fn wrap_empty_yields_one_blank_line() {
        assert_eq!(wrap("", 10), vec![""]);
    }

    #[test]
    fn wrap_pre_preserves_indent_on_every_row() {
        // Indent is stripped before wrapping (so it is never broken inside)
        // and prepended to every continuation row.
        let w = wrap_pre("    indented code here", 12);
        assert_eq!(w, vec!["    indented ", "    code ", "    here"]);
    }

    #[test]
    fn wrap_pre_breaks_long_word_at_width() {
        // No spaces to break at: hard-break at max_w chunks.
        let w = wrap_pre("abcdefghijklmnopqrstuvwxyz", 10);
        assert_eq!(w, vec!["abcdefghij", "klmnopqrst", "uvwxyz"]);
    }

    #[test]
    fn wrap_pre_keeps_blank_source_lines() {
        // A blank line yields one empty row; surrounding lines wrap.
        let w = wrap_pre("a\n\nb", 10);
        assert_eq!(w, vec!["a", "", "b"]);
    }

    #[test]
    fn wrap_pre_empty_yields_one_blank_line() {
        assert_eq!(wrap_pre("", 10), vec![""]);
    }

    #[test]
    fn wrap_pre_no_wrap_when_it_fits() {
        assert_eq!(wrap_pre("short line", 80), vec!["short line"]);
    }

    #[test]
    fn wrap_line_styled_preserves_indent_and_styles() {
        let key = Style::new().fg(Color::Red).add_modifier(Modifier::BOLD);
        let val = Style::new().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("  hi    ".to_string(), key),
            Span::styled("hello world".to_string(), val),
        ]);
        // Fits in 30: one line, both spans kept with their styles.
        let one = wrap_line_styled(&line, 30);
        assert_eq!(one.len(), 1);
        assert_eq!(spans_of(&one[0]), "  hi    hello world");
        assert_eq!(one[0].spans.len(), 2);
        assert_eq!(one[0].spans[0].style, key);
        assert_eq!(one[0].spans[1].style, val);
    }

    #[test]
    fn wrap_line_styled_wraps_at_space_preserving_width() {
        let line = Line::from(Span::raw("  aa bb cc dd".to_string()));
        // Width 7: "  aa bb" (7) fills the row; the break space is kept as a
        // trailing char (clipped by the renderer) so the content anchor stays
        // stable across re-wraps.
        let wrapped = wrap_line_styled(&line, 7);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(spans_of(&wrapped[0]), "  aa bb ");
        assert_eq!(spans_of(&wrapped[1]), "cc dd");
        // each visual line fits the width; the trailing break space may
        // overflow by one cell (clipped, invisible).
        for l in &wrapped {
            assert!(l.width() <= 8);
        }
    }

    #[test]
    fn wrap_line_styled_empty_yields_one_blank_line() {
        let line = Line::from("");
        let wrapped = wrap_line_styled(&line, 10);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(spans_of(&wrapped[0]), "");
    }

    #[test]
    fn wrap_line_styled_breaks_long_word_at_width() {
        // A spaceless value longer than the width must break at max_w
        // chunks, not one char per line.
        let line = Line::from(vec![
            Span::raw("key ".to_string()),
            Span::raw("abcdefghijklmnopqrstuvwxyz".to_string()),
        ]);
        let wrapped = wrap_line_styled(&line, 10);
        assert_eq!(spans_of(&wrapped[0]), "key ");
        assert_eq!(spans_of(&wrapped[1]), "abcdefghij");
        assert_eq!(spans_of(&wrapped[2]), "klmnopqrst");
        assert_eq!(spans_of(&wrapped[3]), "uvwxyz");
        for l in &wrapped {
            assert!(l.width() <= 10);
        }
    }

    #[test]
    fn wrap_pre_content_length_is_stable_across_widths() {
        // The cumulative selectable-content length must not depend on wrap
        // width, or the Navigate cursor's content anchor drifts on resize.
        let s = "  4   - Accept `prompt`, `model`, `thinking`, `workspace`, `output_limit` or `system`, `world`, `options`.";
        let indent_len = s.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
        let body = &s[indent_len..];
        let body_len = body.chars().count();
        for &w in &[10usize, 20, 30, 40, 50, 80] {
            let segs = wrap_pre(s, w);
            let sum: usize = segs
                .iter()
                .map(|seg| seg.chars().count().saturating_sub(indent_len))
                .sum();
            assert_eq!(sum, body_len, "width {w}");
        }
    }

    #[test]
    fn wrap_content_length_is_stable_across_widths() {
        // `wrap` collapses whitespace before wrapping, but `wrap_cells`
        // keeps the break space as trailing content, so the total char
        // count of the collapsed text is preserved across widths.
        let s = "The quick  brown   fox jumps over the lazy dog and keeps going";
        let collapsed: String = s.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
        let expected = collapsed.chars().count();
        for &w in &[5usize, 10, 20, 40, 80] {
            let segs = wrap(s, w);
            let sum: usize = segs.iter().map(|seg| seg.chars().count()).sum();
            assert_eq!(sum, expected, "width {w}");
        }
    }

    #[test]
    fn wrap_line_styled_content_length_is_stable_across_widths() {
        // Same invariant for styled lines: `wrap_cells` keeps break spaces
        // so the total content char count is stable across widths.
        let line = Line::from(vec![
            Span::raw("The quick brown fox jumps over the lazy dog".to_string()),
        ]);
        let expected: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
        for &w in &[5usize, 10, 20, 40, 80] {
            let wrapped = wrap_line_styled(&line, w);
            let sum: usize = wrapped.iter().map(|l| l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>()).sum();
            assert_eq!(sum, expected, "width {w}");
        }
    }

    #[test]
    fn render_excludes_leading_whitespace_from_content() {
        // `wrap_pre` re-prepends a line's indent to every wrapped row for
        // alignment; that indent must not be selectable content, or the
        // cumulative content length (the Navigate cursor's anchor) depends
        // on the number of wraps — i.e. on width.
        let rl = rline(vec![Span::raw("  ")], vec![Span::raw("    indented body")]);
        let chars: Vec<char> = rl.line.spans.iter().flat_map(|s| s.content.chars()).collect();
        let content: String = chars[rl.content.0..rl.content.1].iter().collect();
        assert_eq!(content, "indented body");
        assert_eq!(rl.content_len(), "indented body".len());
    }
}