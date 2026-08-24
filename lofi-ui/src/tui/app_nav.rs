#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(super) fn enter_nav(&mut self) {
        self.mode = Mode::Navigate;
        self.sel = None;
        // If the user yanked from a non-last-line position, jump back to
        // that line so they can continue reading from where they were.
        // Clamp to the current transcript bounds in case lines were added.
        if let Some((cursor, col)) = self.yank_cursor.take() {
            let total = self.log_total;
            let last = total.saturating_sub(1);
            self.nav_cursor = cursor.min(last);
            self.nav_col = col;
            // Un-pin so the viewport stays where the cursor is, not the
            // bottom of the transcript. The render loop will clamp the
            // viewport to keep the cursor visible.
            self.pinned = false;
            self.top_line = self.nav_cursor.saturating_sub(self.log_view_h / 2);
        } else {
            self.pinned = false;
            self.top_line = self.log_off;
            let last = self
                .log_off
                .saturating_add(self.log_view_h)
                .saturating_sub(1);
            self.nav_cursor = last.min(self.log_total.saturating_sub(1));
            self.nav_col = 0;
        }
    }

    pub(super) fn enter_input(&mut self) {
        self.mode = Mode::Input;
        self.sel = None;
        self.collapse_detail_focus();
    }

    /// Collapse the focused detail and return the cursor to its header. The
    /// expanded area only exists while it is focused; leaving it (to the
    /// input area or elsewhere in the transcript) closes it.
    pub(super) fn collapse_detail_focus(&mut self) {
        let header = self.focused_detail_header_line();
        let Some(focus) = self.detail_focus.take() else {
            return;
        };
        let Some(state) = self.expanded_details.remove(&focus.key) else {
            return;
        };
        if let Some(header) = header {
            self.nav_cursor = header;
        }
        self.invalidate_detail_layout(state.turn);
    }

    /// Snap the viewport to the bottom (latest) transcript line. Used when
    /// leaving Navigate/Select so the user lands on the newest output.
    pub(super) fn pin_to_latest(&mut self) {
        self.pinned = true;
        self.top_line = self.last_base;
    }

    pub(super) fn enter_select(&mut self) {
        self.mode = Mode::Select;
        self.sync_detail_nav_position();
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

    fn detail_target(&self, key: &DetailKey) -> Option<view::DetailTarget> {
        self.log_details
            .iter()
            .flatten()
            .find(|target| target.key == *key)
            .cloned()
    }

    fn focused_detail_target(&self) -> Option<view::DetailTarget> {
        let focus = self.detail_focus.as_ref()?;
        self.detail_target(&focus.key)
    }

    pub(super) fn focused_detail_row_index(&self, row: usize) -> Option<usize> {
        let focus = self.detail_focus.as_ref()?;
        self.log_details.iter().position(|target| {
            target
                .as_ref()
                .is_some_and(|target| target.key == focus.key && target.row == Some(row))
        })
    }

    fn focused_detail_header_line(&self) -> Option<usize> {
        let focus = self.detail_focus.as_ref()?;
        self.log_details
            .iter()
            .position(|target| {
                target
                    .as_ref()
                    .is_some_and(|target| target.key == focus.key && target.row.is_none())
            })
            .map(|rel| self.log_off.saturating_add(rel))
    }

    fn sync_detail_nav_position(&mut self) {
        let Some((key, cursor, col)) = self
            .detail_focus
            .as_ref()
            .map(|focus| (focus.key.clone(), focus.cursor, focus.col))
        else {
            return;
        };
        let Some(rel) = self.log_details.iter().position(|target| {
            target
                .as_ref()
                .is_some_and(|target| target.key == key && target.row == Some(cursor))
        }) else {
            return;
        };
        self.nav_cursor = self.log_off.saturating_add(rel);
        self.nav_col = col;
    }

    pub(super) fn focused_detail_content_range(&self) -> Option<(usize, usize)> {
        let focus = self.detail_focus.as_ref()?;
        let rel = self.focused_detail_row_index(focus.cursor)?;
        self.log_vis.get(rel).map(|line| line.content)
    }

    fn rendered_content_range(&self, line: usize) -> (usize, usize) {
        let rel = line.saturating_sub(self.log_off);
        self.log_vis.get(rel).map_or((0, 0), |v| v.content)
    }

    /// Content char range [cstart, cend] of the cursor line (absolute char
    /// indices), from the last render's visible window.
    pub(super) fn cursor_content_range(&self) -> (usize, usize) {
        if let Some(range) = self.focused_detail_content_range() {
            return range;
        }
        self.rendered_content_range(self.nav_cursor)
    }

    fn cursor_detail(&self) -> Option<view::DetailTarget> {
        let rel = self.nav_cursor.checked_sub(self.log_off)?;
        self.log_details.get(rel)?.clone()
    }

    fn cursor_turn(&self) -> usize {
        let mut turn = 0;
        for idx in 0..self.turns.len() {
            if self.turn_start_line(idx) <= self.nav_cursor {
                turn = idx;
            } else {
                break;
            }
        }
        turn
    }

    fn invalidate_detail_layout(&mut self, turn: usize) {
        self.collapsed_turns.get_mut().remove(turn);
        self.frozen_render.clear();
        if let Some(height) = self.frozen_heights_estimated.get_mut(turn) {
            *height = true;
            self.height_remeasure_from = Some(
                self.height_remeasure_from
                    .unwrap_or_default()
                    .max(turn.saturating_add(1)),
            );
        }
    }

    pub(super) fn set_cursor_detail_expanded(&mut self, expanded: bool) -> bool {
        let Some(target) = self.cursor_detail() else {
            return false;
        };
        let is_expanded = self.expanded_details.contains_key(&target.key);
        if expanded == is_expanded {
            return false;
        }
        let turn = self.cursor_turn();
        if expanded {
            self.expanded_details
                .insert(target.key, DetailState { turn, scroll: None });
        } else {
            self.expanded_details.remove(&target.key);
        }
        self.invalidate_detail_layout(turn);
        true
    }

    pub(super) fn toggle_cursor_detail(&mut self) -> bool {
        let header = self.focused_detail_header_line();
        if let Some(focus) = self.detail_focus.take() {
            let Some(state) = self.expanded_details.remove(&focus.key) else {
                self.mode = Mode::Navigate;
                self.sel = None;
                return false;
            };
            if let Some(header) = header {
                self.nav_cursor = header;
            }
            self.mode = Mode::Navigate;
            self.sel = None;
            self.invalidate_detail_layout(state.turn);
            return true;
        }
        let Some(target) = self.cursor_detail() else {
            return false;
        };
        let expanded = !self.expanded_details.contains_key(&target.key);
        if !self.set_cursor_detail_expanded(expanded) {
            return false;
        }
        if expanded {
            self.detail_focus = Some(DetailFocus::on(&target, 0));
        }
        true
    }

    pub(super) fn scroll_cursor_detail(&mut self, delta: i32) -> bool {
        let Some(target) = self.cursor_detail() else {
            return false;
        };
        let turn = {
            let Some(state) = self.expanded_details.get_mut(&target.key) else {
                return false;
            };
            let max = target.total.saturating_sub(DETAIL_VIEW_ROWS);
            let current = state.scroll.unwrap_or(if target.tail { max } else { 0 });
            state.scroll = Some(if delta > 0 {
                current.saturating_add(delta as usize).min(max)
            } else {
                current.saturating_sub(delta.unsigned_abs() as usize)
            });
            state.turn
        };
        self.invalidate_detail_layout(turn);
        true
    }

    pub(super) fn nav_col_delta(&mut self, delta: i32) {
        let (cstart, cend) = self.cursor_content_range();
        if let Some(focus) = &mut self.detail_focus {
            focus.col = if delta > 0 {
                focus.col.saturating_add(1).min(cend)
            } else {
                focus.col.saturating_sub(1).max(cstart)
            };
            self.sync_detail_nav_position();
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
            return;
        }
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

    pub(super) fn nav_set_col(&mut self, target: usize) {
        let (cstart, cend) = self.cursor_content_range();
        let col = if cend > cstart {
            target.clamp(cstart, cend - 1)
        } else {
            cstart
        };
        if let Some(focus) = &mut self.detail_focus {
            focus.col = col;
            self.sync_detail_nav_position();
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
            return;
        }
        self.nav_col = col;
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
    }

    fn first_nonblank_col_in(text: &str, cstart: usize, cend: usize) -> usize {
        if cend <= cstart {
            return cstart;
        }
        for (i, c) in text.chars().enumerate() {
            if i >= cend {
                break;
            }
            if i >= cstart && !c.is_whitespace() {
                return i;
            }
        }
        cstart
    }

    pub(super) fn first_nonblank_col(&self) -> usize {
        let (cstart, cend) = self.cursor_content_range();
        if let Some(focus) = &self.detail_focus {
            let Some(rel) = self.focused_detail_row_index(focus.cursor) else {
                return cstart;
            };
            let Some(line) = self.log_vis.get(rel) else {
                return cstart;
            };
            return Self::first_nonblank_col_in(&line.rendered, cstart, cend);
        }
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let Some(line) = self.log_vis.get(rel) else {
            return cstart;
        };
        Self::first_nonblank_col_in(&line.rendered, cstart, cend)
    }

    pub(super) fn nav_word_target(&self, motion: WordMotion) -> usize {
        let (cstart, cend) = self.cursor_content_range();
        if cend <= cstart {
            return cstart;
        }
        let Some((vl, current_col)) = (if let Some(focus) = &self.detail_focus {
            self.focused_detail_row_index(focus.cursor)
                .and_then(|rel| self.log_vis.get(rel))
                .map(|line| (line, focus.col))
        } else {
            let rel = self.nav_cursor.saturating_sub(self.log_off);
            self.log_vis.get(rel).map(|line| (line, self.nav_col))
        }) else {
            return cstart;
        };
        let chars: Vec<char> = vl.rendered.chars().collect();
        let content = &chars[cstart..cend];
        let n = content.len();
        let p = current_col.clamp(cstart, cend - 1) - cstart;
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

    fn move_focused_detail(&mut self, delta: i32) -> bool {
        let Some(target) = self.focused_detail_target() else {
            return false;
        };
        let key = target.key.clone();
        let last = target.total.saturating_sub(1);
        let next = {
            let Some(focus) = &mut self.detail_focus else {
                return false;
            };
            let step = delta.unsigned_abs() as usize;
            focus.cursor = if delta > 0 {
                focus.cursor.saturating_add(step).min(last)
            } else {
                focus.cursor.saturating_sub(step)
            };
            focus.cursor
        };
        let (scroll_changed, turn) = {
            let Some(state) = self.expanded_details.get_mut(&key) else {
                return false;
            };
            let max = target.total.saturating_sub(DETAIL_VIEW_ROWS);
            let start = state.scroll.unwrap_or(if target.tail { max } else { 0 });
            let desired = if next < start {
                next
            } else if next >= start + DETAIL_VIEW_ROWS {
                next.saturating_add(1).saturating_sub(DETAIL_VIEW_ROWS)
            } else {
                start
            }
            .min(max);
            let changed = state.scroll != Some(desired) && start != desired;
            if changed {
                state.scroll = Some(desired);
            }
            (changed, state.turn)
        };
        if scroll_changed {
            self.invalidate_detail_layout(turn);
        }
        self.sync_detail_nav_position();
        true
    }

    pub(super) fn nav_move(&mut self, delta: i32) {
        if self.detail_focus.is_some() && self.move_focused_detail(delta) {
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
            return;
        }
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
        if self.detail_focus.is_some() && self.move_focused_detail(i32::MIN) {
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
            return;
        }
        self.nav_cursor = 0;
        if self.mode == Mode::Select {
            self.sel = Some(self.select_sel());
        }
        self.nav_show_cursor();
    }

    pub(super) fn nav_bottom(&mut self) {
        if self.detail_focus.is_some() && self.move_focused_detail(i32::MAX) {
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
            return;
        }
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
        self.top_line = new_top.min(base);
        // Reaching the latest transcript line is an explicit request to
        // follow the tail. Record that intent immediately rather than waiting
        // for a render to infer it from offsets: a settling event can append
        // or reflow rows before that render and otherwise make the viewport
        // fall back to the previous bottom.
        self.pinned = cur == total.saturating_sub(1) && self.top_line >= base;
    }

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

    /// Capture a content anchor for an arbitrary transcript position
    /// `(cursor, col)` — its turn index and the cumulative selectable-content
    /// char offset of its line's start within that turn — so the position can
    /// be re-seated on the same content character after a re-wrap. The content
    /// text is unchanged by a width change, so a content offset is a stable
    /// anchor where an absolute line index is not.
    fn content_anchor_for(&self, cursor: usize, col: usize) -> Option<(usize, usize)> {
        let n = self.turns.len();
        if n == 0 {
            return None;
        }
        let mut k = 0;
        for i in 0..n {
            if self.turn_start_line(i) <= cursor {
                k = i;
            } else {
                break;
            }
        }
        let intra = cursor.saturating_sub(self.turn_start_line(k));
        let last = k + 1 == n;
        let live = last && self.frozen_heights.len() < n;
        let char_pos = if live {
            let v = self.render_turn_at(k, self.frozen_width, self.run_active());
            cursor_char_pos(&v, intra, col)
        } else if let Some(v) = self.frozen_render.get(k) {
            cursor_char_pos(v, intra, col)
        } else {
            let v = self.render_turn_at(k, self.frozen_width, false);
            cursor_char_pos(&v, intra, col)
        };
        Some((k, char_pos))
    }

    pub(super) fn nav_content_anchor(&self) -> Option<(usize, usize)> {
        self.content_anchor_for(self.nav_cursor, self.nav_col)
    }

    pub(super) fn sel_content_anchor(&self) -> Option<(usize, usize)> {
        self.content_anchor_for(self.select_anchor.0, self.select_anchor.1)
    }

    fn reseat_position(
        &self,
        anchor: (usize, usize),
        last_lines: &[view::RenderLine],
        width: usize,
    ) -> Option<(usize, usize)> {
        let (k, char_pos) = anchor;
        let n = self.turns.len();
        if k >= n {
            return None;
        }
        let live = k + 1 == n && self.frozen_heights.len() < n;
        // Find the (new-width) line containing `char_pos` and the display
        // column that lands on that character, so the cell stays on the same
        // content char instead of drifting to the new line's start.
        let seated = if live {
            reseat_at(last_lines, char_pos)
        } else if let Some(v) = self.frozen_render.get(k) {
            reseat_at(v, char_pos)
        } else {
            let v = self.render_turn_at(k, width, false);
            reseat_at(&v, char_pos)
        }?;
        Some((self.turn_start_line(k).saturating_add(seated.0), seated.1))
    }

    pub(super) fn reseat_nav_cursor(
        &mut self,
        anchor: (usize, usize),
        last_lines: &[view::RenderLine],
        width: usize,
    ) {
        if let Some((cursor, col)) = self.reseat_position(anchor, last_lines, width) {
            self.nav_cursor = cursor;
            self.nav_col = col;
            if self.mode == Mode::Select {
                self.sel = Some(self.select_sel());
            }
        }
    }

    pub(super) fn reseat_sel_anchor(
        &mut self,
        anchor: (usize, usize),
        last_lines: &[view::RenderLine],
        width: usize,
    ) {
        if let Some((cursor, col)) = self.reseat_position(anchor, last_lines, width) {
            self.select_anchor = (cursor, col);
            self.sel = Some(self.select_sel());
        }
    }

    pub(super) fn turn_start_line(&self, i: usize) -> usize {
        let n = self.turns.len();
        if i == 0 || n == 0 {
            return 0;
        }
        let mut start = 0usize;
        for j in 0..i.min(n) {
            let h = self
                .frozen_heights
                .get(j)
                .copied()
                .unwrap_or(self.last_turn_height);
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
            Mode::Navigate | Mode::Select if self.detail_focus.is_some() => {
                self.nav_move(-i32::try_from(DETAIL_VIEW_ROWS).unwrap_or(i32::MAX));
            }
            Mode::Navigate | Mode::Select => self.nav_move(-step),
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
            Mode::Navigate | Mode::Select if self.detail_focus.is_some() => {
                self.nav_move(i32::try_from(DETAIL_VIEW_ROWS).unwrap_or(i32::MAX));
            }
            Mode::Navigate | Mode::Select => self.nav_move(step),
        }
    }

    pub(super) fn yank_selection(&mut self) {
        if let Some(text) = self.selection_text() {
            self.save_yank_cursor();
            self.yank_text(&text);
        }
    }

    pub(super) fn yank_line(&mut self) {
        self.sync_detail_nav_position();
        if let Some(text) = self.current_line_text() {
            self.save_yank_cursor();
            self.yank_text(&text);
        }
    }

    /// Save the current cursor position so the next `enter_nav` can jump
    /// back to it. Not saved when the cursor is on the last line (the
    /// transcript-follow case) — in that case `enter_nav` follows as usual.
    fn save_yank_cursor(&mut self) {
        let last = self.log_total.saturating_sub(1);
        if self.nav_cursor < last {
            self.yank_cursor = Some((self.nav_cursor, self.nav_col));
        } else {
            self.yank_cursor = None;
        }
    }

    pub(super) fn yank_text(&mut self, text: &str) {
        Self::osc52(text);
        self.yank_notify = Some(Instant::now());
    }

    pub(super) fn osc52(text: &str) {
        let b64 = base64::engine::general_purpose::STANDARD.encode(text);
        let _ = write!(io::stdout(), "\x1b]52;c;{b64}\x07");
        let _ = io::stdout().flush();
    }

    pub(super) fn scroll_by(&mut self, delta: i32) {
        if delta == 0 {
            return;
        }
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
        if self.pinned {
            base
        } else {
            self.top_line.min(base)
        }
    }

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

    /// Lines carrying a raw markdown position map ([`log_raw`]) are copied
    /// from the source — markers intact — by mapping the display selection
    /// `[cs, ce)` through the map to a source slice `source[map[cs]..map[ce]]`.
    /// Soft-wrap continuation rows share their source line with the preceding
    /// row and their maps are contiguous, so their slices concatenate without
    /// a separator; a `\n` is inserted only at hard breaks (a new source
    /// line) or at the boundary to a decoration-only line. Decoration-only
    /// lines (tool glyphs, borders) fall back to their rendered content slice.
    #[allow(clippy::needless_range_loop)]
    #[allow(clippy::too_many_lines)]
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
        let vis = &self.log_vis;
        let off = self.log_off;
        let vis_len = vis.len();
        if vis_len == 0 || el < off || sl >= off + vis_len {
            return None;
        }
        let lo = sl.max(off) - off;
        let hi = el.min(off + vis_len - 1) - off;
        let mut out = String::new();
        let mut prev_src: Option<std::sync::Arc<str>> = None;
        for rel in lo..=hi {
            let li_abs = off + rel;
            let vl = &vis[rel];
            let s = &vl.rendered;
            let n = s.chars().count();
            let (cstart, cend) = vl.content;
            let cs = (if li_abs == sl { sc } else { 0 }).clamp(cstart, cend);
            let ce = (if li_abs == el { ec } else { n }).clamp(cstart, cend);
            if let Some(rl) = vl.raw.as_ref() {
                if rl.map.len() >= 2 {
                    let start_rel = cs.saturating_sub(cstart).min(rl.map.len() - 1);
                    let end_rel = ce.saturating_sub(cstart).min(rl.map.len() - 1);
                    if start_rel < end_rel {
                        let start = if start_rel == 0 && rl.hard_break {
                            0
                        } else {
                            rl.map[start_rel]
                        };
                        let end = rl.map[end_rel];
                        let cont = !rl.hard_break
                            && prev_src
                                .as_ref()
                                .is_some_and(|p| std::sync::Arc::ptr_eq(p, &rl.source));
                        if !cont && !out.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&rl.source[start..end]);
                        prev_src = Some(rl.source.clone());
                        continue;
                    }
                    if rl.hard_break && !out.is_empty() {
                        out.push('\n');
                    }
                    prev_src = if rl.hard_break {
                        None
                    } else {
                        Some(rl.source.clone())
                    };
                    continue;
                }
                if !rl.source.is_empty() && cs < ce {
                    let cont = !rl.hard_break
                        && prev_src
                            .as_ref()
                            .is_some_and(|p| std::sync::Arc::ptr_eq(p, &rl.source));
                    if !cont && !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&rl.source);
                    prev_src = Some(rl.source.clone());
                } else {
                    // Empty source and either no content selected (a
                    // blank paragraph separator) or decoration-only grid
                    // chars (a table border).  The former must emit a
                    // separator so the blank line survives; the latter
                    // is suppressed.
                    if cs >= ce && rl.hard_break && !out.is_empty() {
                        out.push('\n');
                    }
                    prev_src = None;
                }
                continue;
            }
            let chars: Vec<(usize, char)> = s.char_indices().collect();
            let b0 = if cs == 0 || cs >= ce {
                0
            } else {
                chars[cs - 1].0 + chars[cs - 1].1.len_utf8()
            };
            let b1 = if ce == 0 || cs >= ce {
                0
            } else {
                chars[ce - 1].0 + chars[ce - 1].1.len_utf8()
            };
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&s[b0..b1]);
            prev_src = None;
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    pub(super) fn clear_log(&mut self) {
        self.turns.clear();
        self.expanded_details.clear();
        self.detail_focus = None;
        self.collapsed_turns.get_mut().clear();
        self.turn_byte_ranges.clear();
        self.turn_event_offsets.clear();
        self.pinned = true;
        self.top_line = 0;
        self.bump_render_epoch();
    }
}

fn content_offset(lines: &[view::RenderLine], intra: usize) -> usize {
    lines
        .iter()
        .take(intra)
        .map(view::RenderLine::content_len)
        .sum()
}

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

fn reseat_at(lines: &[view::RenderLine], char_pos: usize) -> Option<(usize, usize)> {
    let j = line_at_content_offset(lines, char_pos)?;
    let start = content_offset(lines, j);
    let rl = lines.get(j)?;
    let clen = rl.content_len();
    let col = char_pos.saturating_sub(start).min(clen);
    Some((j, rl.content.0 + col))
}

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
