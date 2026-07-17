//! Atomic rendering primitives shared by every component: line padding,
//! truncation, word-wrap, duration formatting, and styled-span constructors.
//!
//! Components compose these into larger units; nothing here knows about
//! turns or blocks.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
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



/// Pad `spans` to the full `width` with a `bg`-colored tail so the
/// background runs edge-to-edge. `used` is the visible width already
/// consumed by `spans`.
pub fn pad(spans: Vec<Span<'static>>, bg: Color, width: usize, used: usize) -> Line<'static> {
    let pad_w = width.saturating_sub(used);
    let mut v = spans;
    v.push(Span::styled(" ".repeat(pad_w), Style::new().bg(bg)));
    Line::from(v)
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
pub fn wrap(s: &str, max_w: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.split('\n') {
        if max_w == 0 {
            out.push(line.to_string());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in line.split(' ') {
            let ww = word.width();
            let add = if cur.is_empty() { ww } else { cur_w + 1 + ww };
            if add > max_w && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                if ww > max_w {
                    // Word itself overflows: break it on a wide-char boundary.
                    let (piece, w) = break_wide(word, max_w, &mut out);
                    cur = piece;
                    cur_w = w;
                } else {
                    cur = word.to_string();
                    cur_w = ww;
                }
            } else {
                if !cur.is_empty() {
                    cur.push(' ');
                    cur_w += 1;
                }
                cur.push_str(word);
                cur_w += ww;
            }
        }
        out.push(cur);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Break `word` into `max_w`-wide pieces, pushing all but the last to `out`
/// and returning `(last_piece, its_width)`. Never splits a wide char.
fn break_wide(word: &str, max_w: usize, out: &mut Vec<String>) -> (String, usize) {
    let mut piece = String::new();
    let mut w = 0;
    for c in word.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw > max_w && !piece.is_empty() {
            out.push(std::mem::take(&mut piece));
            w = 0;
        }
        piece.push(c);
        w += cw;
    }
    (piece, w)
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
}