use super::*;

impl App {
    /// Number of select rows the input occupies after soft-wrapping to the
    /// prompt width, capped at [`MAX_INPUT_LINES`]. `width` is the full
    /// terminal width; 2 cells are reserved for the `❯ `/`  ` prefix.
    fn input_lines(&self, width: usize) -> usize {
        let content_w = width.saturating_sub(2);
        self.input_select_rows(content_w)
            .len()
            .min(MAX_INPUT_LINES)
    }

    /// Keep [`input_scroll`] within bounds and clamp it so the cursor's
    /// select row stays inside the visible `[scroll, scroll + vis_h)` window.
    /// Called each frame before the prompt is rendered.
    fn sync_input_scroll(&mut self, content_w: usize, vis_h: usize) {
        let total = self.input_select_rows(content_w).len();
        let max_top = total.saturating_sub(vis_h);
        let mut scroll = self.input_scroll.min(max_top);
        let vrow = self.input_cursor_pos(content_w).0;
        if vrow < scroll {
            scroll = vrow;
        } else if vis_h > 0 && vrow >= scroll + vis_h {
            scroll = vrow.saturating_sub(vis_h) + 1;
        }
        self.input_scroll = scroll.min(max_top);
    }

    /// Soft-wrap the input to `content_w` display cells, breaking on
    /// wide-char boundaries (not word boundaries, so the cursor maps
    /// predictably). Hard `\n` splits always start a new row. Empty input
    /// yields a single empty row so the prompt always renders one line.
    fn input_select_rows(&self, content_w: usize) -> Vec<String> {
        let mut rows = Vec::new();
        for line in self.input.split('\n') {
            if content_w == 0 {
                rows.push(line.to_string());
                continue;
            }
            let mut cur = String::new();
            let mut cur_w = 0usize;
            for c in line.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if cur_w + cw > content_w && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push(c);
                cur_w += cw;
            }
            rows.push(cur);
        }
        if rows.is_empty() {
            rows.push(String::new());
        }
        rows
    }

    /// Map the cursor to a (select row, x-within-content) pair for
    /// [`set_cursor_position`], accounting for soft-wrap. `x` is relative to
    /// the content area; the caller adds the 2-cell prefix.
    fn input_cursor_pos(&self, content_w: usize) -> (usize, usize) {
        let (lrow, lcol) = self.cursor_row_col();
        let mut vrow = 0usize;
        for (i, line) in self.input.split('\n').enumerate() {
            if i == lrow {
                break;
            }
            vrow += count_wrapped_rows(line, content_w);
        }
        let line = self.input.split('\n').nth(lrow).unwrap_or("");
        let prefix: String = line.chars().take(lcol).collect();
        let (sub, x) = wrap_prefix_pos(&prefix, content_w);
        (vrow + sub, x)
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.input_cursor, c);
        self.input_cursor += c.len_utf8();
        self.history_idx = None;
    }

    /// Insert a (possibly multi-line) string at the cursor in one operation.
    /// Used by bracketed-paste so a paste of any size lands atomically
    /// instead of character-by-character. Line endings are normalized to
    /// `\n` so a paste carrying `\r\n` or bare `\r` (which varies by terminal)
    /// renders and submits as real newlines, not a single garbled line.
    fn insert_str(&mut self, s: &str) {
        let normalized: String = s.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.input_cursor, &normalized);
        self.input_cursor += normalized.len();
        self.history_idx = None;
    }

    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn backspace(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        let i = self.input[..self.input_cursor]
            .char_indices()
            .last()
            .map_or(0, |(i, _)| i);
        self.input.replace_range(i..self.input_cursor, "");
        self.input_cursor = i;
        self.history_idx = None;
    }

    fn move_left(&mut self) {
        if let Some((i, _)) = self.input[..self.input_cursor].char_indices().last() {
            self.input_cursor = i;
        }
    }

    fn move_right(&mut self) {
        if let Some((_, c)) = self.input[self.input_cursor..].char_indices().next() {
            self.input_cursor += c.len_utf8();
        }
    }

    fn move_up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row == 0 {
            return;
        }
        self.input_cursor =
            line_start_byte(&self.input, row - 1) + char_index_to_byte(&self.input, row - 1, col);
    }

    fn move_down(&mut self) {
        let (row, col) = self.cursor_row_col();
        let last_row = self.input.matches('\n').count();
        if row >= last_row {
            return;
        }
        self.input_cursor =
            line_start_byte(&self.input, row + 1) + char_index_to_byte(&self.input, row + 1, col);
    }

    fn cursor_row_col(&self) -> (usize, usize) {
        let before = &self.input[..self.input_cursor];
        let row = before.matches('\n').count();
        let col = before
            .rsplit_once('\n')
            .map_or(before.chars().count(), |(_, last)| last.chars().count());
        (row, col)
    }

    fn move_line_start(&mut self) {
        let before = &self.input[..self.input_cursor];
        self.input_cursor = before.rsplit_once('\n').map_or(0, |(b, _)| b.len() + 1);
    }

    fn move_line_end(&mut self) {
        let after = &self.input[self.input_cursor..];
        self.input_cursor += after.find('\n').unwrap_or(after.len());
    }

    fn move_word_back(&mut self) {
        self.input_cursor = prev_word_start(&self.input, self.input_cursor);
    }

    fn move_word_fwd(&mut self) {
        self.input_cursor = next_word_end(&self.input, self.input_cursor);
    }

    fn delete_forward_char(&mut self) {
        if let Some((i, c)) = self.input[self.input_cursor..].char_indices().next() {
            let end = self.input_cursor + i + c.len_utf8();
            self.input.replace_range(self.input_cursor..end, "");
        }
        self.history_idx = None;
    }

    /// Remove `input[start..end]` into the kill ring. When `append` is true the
    /// removed text is appended (consecutive `C-k`); otherwise it replaces.
    fn kill_range(&mut self, start: usize, end: usize, append: bool) {
        let (start, end) = if start <= end { (start, end) } else { (end, start) };
        if start >= end {
            return;
        }
        let removed = self.input[start..end].to_string();
        if append {
            self.kill_ring.push_str(&removed);
        } else {
            self.kill_ring = removed;
        }
        self.input.replace_range(start..end, "");
        self.input_cursor = start;
        self.history_idx = None;
    }

    fn kill_line_end(&mut self, append: bool) {
        let after = &self.input[self.input_cursor..];
        // End of buffer with no trailing newline: nothing to kill. Emacs
        // leaves the buffer untouched here, and the naive `cursor + 1` would
        // slice one past the end and panic.
        if after.is_empty() {
            return;
        }
        let nl = after.find('\n').unwrap_or(after.len());
        // Emacs kills the newline itself when invoked on an empty remainder,
        // so repeated `C-k` on blank lines pulls them in one at a time.
        let end = if nl == 0 {
            self.input_cursor + 1
        } else {
            self.input_cursor + nl
        };
        self.kill_range(self.input_cursor, end, append);
        self.last_kill_was_kill = true;
    }

    fn kill_line_start(&mut self) {
        let before = &self.input[..self.input_cursor];
        let start = before.rsplit_once('\n').map_or(0, |(b, _)| b.len() + 1);
        self.kill_range(start, self.input_cursor, false);
    }

    fn kill_word_back(&mut self) {
        let start = prev_word_start(&self.input, self.input_cursor);
        self.kill_range(start, self.input_cursor, false);
    }

    fn kill_word_fwd(&mut self) {
        let end = next_word_end(&self.input, self.input_cursor);
        self.kill_range(self.input_cursor, end, false);
    }

    fn yank(&mut self) {
        if self.kill_ring.is_empty() {
            return;
        }
        let s = self.kill_ring.clone();
        self.input.insert_str(self.input_cursor, &s);
        self.input_cursor += s.len();
        self.history_idx = None;
    }

    /// Up arrow / `Ctrl+P`: move to the previous line, or — when already on
    /// the first line — jump to its start, and once at the very first cell
    /// recall the previous history entry. Mirrors zsh `up-line-or-history`
    /// with a start-of-line intermediate step.
    fn cursor_up(&mut self) {
        let (row, col) = self.cursor_row_col();
        if row > 0 {
            self.move_up();
        } else if col > 0 {
            self.move_line_start();
        } else {
            self.recall_prev();
        }
    }

    /// Down arrow / `Ctrl+N`: the symmetric counterpart — next line, then end
    /// of the last line, then recall the next history entry.
    fn cursor_down(&mut self) {
        let (row, _col) = self.cursor_row_col();
        let last_row = self.input.matches('\n').count();
        if row < last_row {
            self.move_down();
        } else if self.input_cursor < self.input.len() {
            self.move_line_end();
        } else {
            self.recall_next();
        }
    }

    /// Content slice of the cursor line (char range `[cstart, cend)` mapped to
    /// bytes), excluding the decorative gutter and trailing padding.
    fn current_line_text(&self) -> Option<String> {
        let rel = self.nav_cursor.saturating_sub(self.log_off);
        let s = self.log_lines.get(rel)?;
        let n = s.chars().count();
        let (cstart, cend) = self.log_content.get(rel).copied().unwrap_or((0, n));
        let cstart = cstart.min(n);
        let cend = cend.min(n);
        if cstart >= cend {
            return None;
        }
        let b0 = s.char_indices().nth(cstart).map_or(s.len(), |(b, _)| b);
        let b1 = s.char_indices().nth(cend).map_or(s.len(), |(b, _)| b);
        Some(s[b0..b1].to_string())
    }

    fn recall_prev(&mut self) {
        if self.history_nav.is_empty() {
            return;
        }
        match self.history_idx {
            None => {
                self.input_stash = self.input.clone();
                let idx = self.history_nav.len() - 1;
                self.history_idx = Some(idx);
                self.set_input(self.history_nav[idx].clone());
                self.input_cursor = 0;
            }
            Some(0) => {}
            Some(i) => {
                let idx = i - 1;
                self.history_idx = Some(idx);
                self.set_input(self.history_nav[idx].clone());
                self.input_cursor = 0;
            }
        }
    }

    fn recall_next(&mut self) {
        match self.history_idx {
            None => {}
            Some(i) => {
                if i + 1 >= self.history_nav.len() {
                    self.history_idx = None;
                    let stash = std::mem::take(&mut self.input_stash);
                    self.set_input(stash);
                } else {
                    let idx = i + 1;
                    self.history_idx = Some(idx);
                    self.set_input(self.history_nav[idx].clone());
                }
            }
        }
    }

    fn set_input(&mut self, s: String) {
        self.input = s;
        self.input_cursor = self.input.len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
        self.history_idx = None;
        self.slash_complete = None;
    }
}
