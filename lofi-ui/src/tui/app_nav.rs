#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    /// Enter Navigate mode, parking the viewport where it is (stop following
    /// new output) and placing the cursor on the last visible line.
    pub(super) fn enter_nav(&mut self) {
        self.mode = Mode::Navigate;
        self.sel = None;
        self.pinned = false;
        self.top_line = self.log_off;
        let last = self
            .log_off
            .saturating_add(self.log_view_h)
            .saturating_sub(1);
        self.nav_cursor = last.min(self.log_total.saturating_sub(1));
        // Park the column at the content start; h/l snap it into range.
        self.nav_col = 0;
    }

    /// Return to Input mode, dropping any selection.
    pub(super) fn enter_input(&mut self) {
        self.mode = Mode::Input;
        self.sel = None;
    }

    /// Snap the viewport to the bottom (latest) transcript line. Used when
    /// leaving Navigate/Select so the user lands on the newest output.
    pub(super) fn pin_to_latest(&mut self) {
        self.pinned = true;
        self.top_line = self.last_base;
    }

    /// From Navigate, start a charwise selection at the cursor.
    pub(super) fn enter_select(&mut self) {
        self.mode = Mode::Select;
        self.select_anchor = (self.nav_cursor, self.nav_col);
        self.sel = Some(self.select_sel());
    }

    /// Charwise selection from the anchor to the cursor (inclusive of both
    /// endpoints, vim-style). The max-end column is shifted by +1 so the
    /// content-aware clamp treats it as an exclusive bound for rendering and
    /// [`selection_text`].
    pub(super) fn select_sel(&self) -> Selection {
        let a = self.select_anchor;
        let c = (self.nav_cursor, self.nav_col);
        let (s, mut e) = if a <= c { (a, c) } else { (c, a) };
        e.1 = e.1.saturating_add(1);
        Selection { start: s, end: e }
    }

    /// Content char range [cstart, cend] of the cursor line (absolute char
    /// indices), from the last render's visible window. Valid because
    /// `nav_show_cursor` keeps the cursor on screen between events.
    pub(super) fn cursor_content_range(&self) -> (usize, usize) {
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        self.log_content.get(rel).copied().unwrap_or((0, 0))
    }

    /// Move the cursor column by `delta` chars, clamped to the cursor line's
    /// content. In Select the selection follows.
    pub(super) fn nav_col_delta(&mut self, delta: i32) {
        let (cstart, cend) = self.cursor_content_range();
        let raw = if delta > 0 {
            self.nav_col.saturating_add(1)
        } else {
            self.nav_col.saturating_sub(1)
        };
        self.nav_col = raw.clamp(cstart, cend);
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
    }

    /// Set the cursor column to `target`, clamped to the cursor line's
    /// content. In Select the selection follows.
    pub(super) fn nav_set_col(&mut self, target: usize) {
        let (cstart, cend) = self.cursor_content_range();
        self.nav_col = if cend > cstart {
            target.clamp(cstart, cend - 1)
        } else {
            cstart
        };
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
    }

    /// Column of the first non-blank content character on the cursor line
    /// (vim `^`). Falls back to the content start when all blank.
    pub(super) fn first_nonblank_col(&self) -> usize {
        let (cstart, cend) = self.cursor_content_range();
        if cend <= cstart {
            return cstart;
        }
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let Some(s) = self.log_lines.get(rel) else {
            return cstart;
        };
        for (i, c) in s.chars().enumerate() {
            if i >= cend {
                break;
            }
            if i >= cstart && !c.is_whitespace() {
                return i;
            }
        }
        cstart
    }

    /// Vim word/WORD motion target column for the cursor line.
    pub(super) fn nav_word_target(&self, motion: WordMotion) -> usize {
        let (cstart, cend) = self.cursor_content_range();
        if cend <= cstart {
            return cstart;
        }
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let Some(s) = self.log_lines.get(rel) else {
            return cstart;
        };
        let chars: Vec<char> = s.chars().collect();
        let content = &chars[cstart..cend];
        let n = content.len();
        let p = self.nav_col.clamp(cstart, cend - 1) - cstart;
        let class = |c: char, big: bool| -> u8 {
            if c.is_whitespace() {
                0
            } else if big || c.is_alphanumeric() || c == '_' {
                1
            } else {
                2
            }
        };
        let np = match motion {
            WordMotion::NextStart { big } => {
                let mut i = p;
                let cur = class(content[i], big);
                if cur != 0 {
                    while i < n && class(content[i], big) == cur {
                        i += 1;
                    }
                }
                while i < n && class(content[i], big) == 0 {
                    i += 1;
                }
                i.min(n.saturating_sub(1))
            }
            WordMotion::PrevStart { big } => {
                let mut i = p.saturating_sub(1);
                while i > 0 && class(content[i], big) == 0 {
                    i -= 1;
                }
                let cur = class(content[i], big);
                while i > 0 && class(content[i - 1], big) == cur {
                    i -= 1;
                }
                i
            }
            WordMotion::NextEnd { big } => {
                let mut i = (p + 1).min(n.saturating_sub(1));
                while i < n && class(content[i], big) == 0 {
                    i += 1;
                }
                if i >= n {
                    n.saturating_sub(1)
                } else {
                    let cur = class(content[i], big);
                    while i + 1 < n && class(content[i + 1], big) == cur {
                        i += 1;
                    }
                    i
                }
            }
        };
        cstart + np
    }

    pub(super) fn nav_word_motion(&mut self, motion: WordMotion) {
        let target = self.nav_word_target(motion);
        self.nav_set_col(target);
    }

    /// Move the Navigate/Select cursor by `delta` lines, clamping to the
    /// transcript. In Select the selection follows; the viewport scrolls only
    /// when the cursor leaves it.
    pub(super) fn nav_move(&mut self, delta: i32) {
        let max = self.log_total.saturating_sub(1);
        let step = delta.unsigned_abs() as usize;
        self.nav_cursor = if delta > 0 {
            self.nav_cursor.saturating_add(step).min(max)
        } else {
            self.nav_cursor.saturating_sub(step)
        };
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    pub(super) fn nav_top(&mut self) {
        self.nav_cursor = 0;
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    pub(super) fn nav_bottom(&mut self) {
        self.nav_cursor = self.log_total.saturating_sub(1);
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    /// Adjust [`top_line`] so the cursor is visible. Navigate never auto-pins
    /// (it doesn't follow new output); only the minimal scroll needed to keep
    /// the cursor on screen is applied.
    pub(super) fn nav_show_cursor(&mut self) {
        let h = self.log_view_h;
        let total = self.log_total;
        if h == 0 || total == 0 {
            return;
        }
        let base = total.saturating_sub(h);
        let cur = self.nav_cursor;
        let off = self.log_off;
        let new_top = if cur < off {
            cur
        } else if cur >= off + h {
            cur + 1 - h
        } else {
            off
        };
        self.pinned = false;
        self.top_line = new_top.min(base);
    }

    /// Render turn `idx` at `width` without touching the frozen cache. Used to
    /// (re)compute a content anchor for the cursor at the old or new width.
    fn render_turn_at(&self, idx: usize, width: usize, active_turn: bool) -> Vec<view::RenderLine> {
        let theme = self.theme;
        let turn = self.materialize_turn(idx);
        let cx = view::component::Cx {
            app: self,
            theme,
            width,
            active_turn,
        };
        view::blocks::render_turn_lines(&cx, &turn)
    }

    /// Capture the Navigate cursor's content anchor — its turn index and the
    /// cumulative selectable-content char offset of its line's start within
    /// that turn — so the cursor can be re-seated on the same content line
    /// after a re-wrap. The content text is unchanged by a width change, so a
    /// content offset is a stable anchor where an absolute line index is not.
    ///
    /// Must run *before* `ensure_frozen` clears the old-width frozen cache.
    pub(super) fn nav_content_anchor(&self) -> Option<(usize, usize)> {
        let n = self.turns.len();
        if n == 0 {
            return None;
        }
        let cur = self.nav_cursor;
        // Largest turn whose start line is at or below the cursor.
        let mut k = 0;
        for i in 0..n {
            if self.turn_start_line(i) <= cur {
                k = i;
            } else {
                break;
            }
        }
        let intra = cur.saturating_sub(self.turn_start_line(k));
        let last = k + 1 == n;
        // Old-width lines: the frozen cache (exact — rendered with
        // `active_turn = false`) when present, else a re-render at the old
        // width. The last turn is never frozen, so it always re-renders with
        // the live `active_turn` to match the previous frame.
        //
        // The anchor is the cursor's exact content-char position — the line's
        // cumulative content start plus the cursor's column mapped into the
        // content — so both the line and `nav_col` can be re-seated onto the
        // same character after the re-wrap (an absolute line index or a
        // line-start offset alone would drift the cell).
        let char_pos = if last {
            let v = self.render_turn_at(k, self.frozen_width, self.run_active());
            cursor_char_pos(&v, intra, self.nav_col)
        } else if let Some(v) = self.frozen_render.get(k) {
            cursor_char_pos(v, intra, self.nav_col)
        } else {
            let v = self.render_turn_at(k, self.frozen_width, false);
            cursor_char_pos(&v, intra, self.nav_col)
        };
        Some((k, char_pos))
    }

    /// Re-seat the cursor on its previous content line after a re-wrap: find
    /// the line in the (new-width) turn whose cumulative content offset is the
    /// largest not exceeding the captured anchor. `last_lines` is the freshly
    /// rendered last turn at the new width.
    pub(super) fn reseat_nav_cursor(
        &mut self,
        anchor: (usize, usize),
        last_lines: &[view::RenderLine],
        width: usize,
    ) {
        let (k, char_pos) = anchor;
        let n = self.turns.len();
        if k >= n {
            return;
        }
        let last = k + 1 == n;
        // Find the (new-width) line containing `char_pos` and the display
        // column that lands on that character, so the cell cursor stays on the
        // same content char instead of drifting to the new line's start.
        let seated = if last {
            reseat_at(last_lines, char_pos)
        } else if let Some(v) = self.frozen_render.get(k) {
            reseat_at(v, char_pos)
        } else {
            let v = self.render_turn_at(k, width, false);
            reseat_at(&v, char_pos)
        };
        if let Some((j, col)) = seated {
            self.nav_cursor = self.turn_start_line(k).saturating_add(j);
            self.nav_col = col;
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
        }
    }

    /// First transcript line of turn `i` (0-based). Turns are laid out as
    /// `turn0, blank, turn1, blank, ...`, so turn `i` starts at the sum of all
    /// preceding turn heights plus one blank separator per preceding turn.
    pub(super) fn turn_start_line(&self, i: usize) -> usize {
        let n = self.turns.len();
        if i == 0 || n == 0 {
            return 0;
        }
        let mut start = 0usize;
        for j in 0..i.min(n) {
            let h = if j + 1 < n {
                self.frozen_heights.get(j).copied().unwrap_or(0)
            } else {
                self.last_turn_height
            };
            start += h + 1;
        }
        start
    }

    /// Jump the cursor to the start of the next (`dir > 0`) or previous turn.
    /// In Select the selection extends; otherwise it is cleared.
    pub(super) fn nav_jump_turn(&mut self, dir: i32) {
        let n = self.turns.len();
        if n == 0 {
            return;
        }
        let mut cur = 0;
        for i in 0..n {
            if self.nav_cursor < self.turn_start_line(i) {
                break;
            }
            cur = i;
        }
        let target_turn = if dir > 0 {
            (cur + 1).min(n - 1)
        } else {
            cur.saturating_sub(1)
        };
        let max = self.log_total.saturating_sub(1);
        self.nav_cursor = self.turn_start_line(target_turn).min(max);
        self.nav_col = 0;
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        } else {
            self.sel = None;
        }
        self.nav_show_cursor();
    }

    /// Move by one viewport page. In [`Mode::Input`] this scrolls the
    /// transcript (unpinning from the bottom on `PgUp`); in
    /// [`Mode::Navigate`]/[`Mode::Select`] the cursor moves a page and the
    /// viewport follows.
    pub(super) fn page_up(&mut self) {
        let h = self.log_view_h;
        if h == 0 {
            return;
        }
        let step: i32 = h.try_into().unwrap_or(i32::MAX);
        match self.mode {
            Mode::Input => {
                self.top_line = self.top_line.saturating_sub(h);
                self.pinned = false;
            }
            Mode::Navigate | Mode::Select => self.nav_move(-step)
        }
    }

    pub(super) fn page_down(&mut self) {
        let h = self.log_view_h;
        if h == 0 {
            return;
        }
        let step: i32 = h.try_into().unwrap_or(i32::MAX);
        match self.mode {
            Mode::Input => {
                let base = self.last_base;
                let new = self.top_line.saturating_add(h);
                if new >= base {
                    self.pinned = true;
                } else {
                    self.top_line = new;
                    self.pinned = false;
                }
            }
            Mode::Navigate | Mode::Select => self.nav_move(step),
        }
    }

    /// Copy the current selection to the system clipboard via OSC 52.
    pub(super) fn yank_selection(&mut self) {
        if let Some(text) = self.selection_text() {
            self.yank_text(&text);
        }
    }

    /// Yank the cursor line's content (decoration excluded) to the clipboard.
    /// Used by Navigate's `y`.
    pub(super) fn yank_line(&mut self) {
        if let Some(text) = self.current_line_text() {
            self.yank_text(&text);
        }
    }

    /// Copy `text` to the system clipboard via OSC 52 and arm the
    /// "Copied to clipboard" rule-line badge.
    pub(super) fn yank_text(&mut self, text: &str) {
        Self::osc52(text);
        self.yank_notify = Some(Instant::now());
    }

    pub(super) fn osc52(text: &str) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        let _ = write!(io::stdout(), "\x1b]52;c;{b64}\x07");
        let _ = io::stdout().flush();
    }

    /// Multi-line scroll for the mouse wheel; negative scrolls up (towards
    /// older output), positive scrolls down. Scrolling up un-pins follow mode.
    pub(super) fn scroll_by(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
        // Scrolling moves the viewport; a mouse selection no longer maps to
        // the visible lines, so drop it (terminals clear selection on scroll).
        self.sel = None;
        if delta < 0 {
            let n = delta.unsigned_abs() as usize;
            if self.pinned {
                self.pinned = false;
                self.top_line = self.last_base.saturating_sub(n);
            } else {
                self.top_line = self.top_line.saturating_sub(n);
            }
        } else {
            let n = usize::try_from(delta).unwrap_or(0);
            if self.pinned {
                return;
            }
            self.top_line = self.top_line.saturating_add(n);
            if self.top_line >= self.last_base {
                self.pinned = true;
            }
        }
    }

    /// Viewport offset that will be used at the next render: `base` when
    /// pinned, otherwise `top_line` clamped to `base`.
    pub(super) fn view_off(&self) -> usize {
        let base = self.last_base;
        if self.pinned { base } else { self.top_line.min(base) }
    }

    /// Mouse-wheel scroll: enter Navigate and move the viewport, clamping the
    /// cursor to the near edge when it leaves the viewport. Scrolling up parks
    /// the cursor on the bottom edge (it falls below as older lines enter);
    /// scrolling down parks it on the top edge. A no-op scroll (already at the
    /// boundary) leaves the mode untouched.
    pub(super) fn scroll_nav(&mut self, delta: i32) {
        let before = self.view_off();
        self.scroll_by(delta);
        let after = self.view_off();
        if after == before {
            return;
        }
        self.mode = Mode::Navigate;
        self.sel = None;
        let last = after
            .saturating_add(self.log_view_h)
            .saturating_sub(1)
            .min(self.log_total.saturating_sub(1));
        if self.nav_cursor < after {
            self.nav_cursor = after;
        } else if self.nav_cursor > last {
            self.nav_cursor = last;
        }
    }

    /// Plain text of the current mouse selection, or `None` when the selection
    /// is empty (a bare click with no drag).
    ///
    /// Lines carrying a raw markdown position map ([`log_raw`]) are copied
    /// from the source — markers intact — by mapping the display selection
    /// `[cs, ce)` through the map to a source slice `source[map[cs]..map[ce]]`.
    /// Soft-wrap continuation rows share their source line with the preceding
    /// row and their maps are contiguous, so their slices concatenate without
    /// a separator; a `\n` is inserted only at hard breaks (a new source
    /// line) or at the boundary to a decoration-only line. Decoration-only
    /// lines (tool glyphs, borders) fall back to their rendered content slice.
    #[allow(clippy::needless_range_loop)]
    pub(super) fn selection_text(&self) -> Option<String> {
        let sel = self.sel.as_ref()?;
        let (sl, sc) = sel.start;
        let (el, ec) = sel.end;
        let ((sl, sc), (el, ec)) = if (sl, sc) <= (el, ec) {
            ((sl, sc), (el, ec))
        } else {
            ((el, ec), (sl, sc))
        };
        if sl == el && sc == ec {
            return None;
        }
        let lines = &self.log_lines;
        let content = &self.log_content;
        let raw = &self.log_raw;
        let off = self.log_off;
        let vis_len = lines.len();
        if vis_len == 0 || el < off || sl >= off + vis_len {
            return None;
        }
        let lo = sl.max(off) - off;
        let hi = el.min(off + vis_len - 1) - off;
        let mut out = String::new();
        let mut prev_src: Option<std::sync::Arc<str>> = None;
        for rel in lo..=hi {
            let li_abs = off + rel;
            let s = &lines[rel];
            let n = s.chars().count();
            let (cstart, cend) = content.get(rel).copied().unwrap_or((0, n));
            let cs = (if li_abs == sl { sc } else { 0 }).clamp(cstart, cend);
            let ce = (if li_abs == el { ec } else { n }).clamp(cstart, cend);
            // Raw markdown: map the content-relative selection to a source slice.
            if let Some(rl) = raw.get(rel).and_then(|r| r.as_ref()) {
                if rl.map.len() >= 2 {
                    let start_rel = cs.saturating_sub(cstart).min(rl.map.len() - 1);
                    let end_rel = ce.saturating_sub(cstart).min(rl.map.len() - 1);
                    if start_rel < end_rel {
                        let start = rl.map[start_rel];
                        let end = rl.map[end_rel];
                        // A soft-wrap continuation of the same source line
                        // concatenates without a separator; anything else
                        // starts a new line.
                        let cont = !rl.hard_break
                            && prev_src.as_ref().is_some_and(|p| std::sync::Arc::ptr_eq(p, &rl.source));
                        if !cont && !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&rl.source[start..end]);
                        prev_src = Some(rl.source.clone());
                        continue;
                    }
                    // Empty contribution — still track the source for contiguity.
                    prev_src = if rl.hard_break { None } else { Some(rl.source.clone()) };
                    continue;
                }
                // Degenerate raw (no per-char map): table data/header rows
                // carry the full markdown source; border rows carry an
                // empty source and are suppressed.
                if !rl.source.is_empty() && cs < ce {
                    let cont = !rl.hard_break
                        && prev_src.as_ref().is_some_and(|p| std::sync::Arc::ptr_eq(p, &rl.source));
                    if !cont && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&rl.source);
                    prev_src = Some(rl.source.clone());
                } else {
                    prev_src = None;
                }
                continue;
            }
            // Decoration-only line: rendered content slice, hard break.
            let chars: Vec<(usize, char)> = s.char_indices().collect();
            let b0 = if cs == 0 || cs >= ce { 0 } else { chars[cs - 1].0 + chars[cs - 1].1.len_utf8() };
            let b1 = if ce == 0 || cs >= ce { 0 } else { chars[ce - 1].0 + chars[ce - 1].1.len_utf8() };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&s[b0..b1]);
            prev_src = None;
        }
        if out.is_empty() { None } else { Some(out) }
    }

    pub(super) fn clear_log(&mut self) {
        self.turns.clear();
        self.turn_byte_ranges.clear();
        self.pinned = true;
        self.top_line = 0;
        self.bump_render_epoch();
    }
}

/// Cumulative selectable-content char offset of line `intra`'s start within a
/// turn's rendered lines. Blanks contribute zero, so separators don't shift it.
fn content_offset(lines: &[view::RenderLine], intra: usize) -> usize {
    lines.iter().take(intra).map(view::RenderLine::content_len).sum()
}

/// Cursor's content-char position within a turn: the line's cumulative content
/// start offset plus the cursor's column mapped into that line's selectable
/// content (decoration stripped, clamped to the content length).
pub(super) fn cursor_char_pos(lines: &[view::RenderLine], intra: usize, nav_col: usize) -> usize {
    let start = content_offset(lines, intra);
    match lines.get(intra) {
        Some(rl) => {
            let clen = rl.content_len();
            let col = nav_col.saturating_sub(rl.content.0).min(clen);
            start + col
        }
        None => start,
    }
}

/// Re-seat onto `char_pos`: the line whose cumulative content start is the
/// largest not exceeding it, and the display column (decoration + offset into
/// that line) that lands on the character. Returns `None` only for empty input.
fn reseat_at(lines: &[view::RenderLine], char_pos: usize) -> Option<(usize, usize)> {
    let j = line_at_content_offset(lines, char_pos)?;
    let start = content_offset(lines, j);
    let rl = lines.get(j)?;
    let clen = rl.content_len();
    let col = char_pos.saturating_sub(start).min(clen);
    Some((j, rl.content.0 + col))
}

/// Index of the line whose cumulative content start offset is the largest not
/// exceeding `c` — i.e. the line containing the `c`-th content char. This is
/// the same content line across a re-wrap.
fn line_at_content_offset(lines: &[view::RenderLine], c: usize) -> Option<usize> {
    let mut acc = 0usize;
    let mut found = None;
    for (i, rl) in lines.iter().enumerate() {
        if acc > c {
            break;
        }
        found = Some(i);
        acc += rl.content_len();
    }
    found
}
