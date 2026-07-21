#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(super) fn toggle_verbose(&mut self) {
        self.verbose = !self.verbose;
        // Folded tool bodies are baked into the frozen-render cache at freeze
        // time, so a toggle must invalidate it — otherwise only the live
        // (last) turn would react and earlier turns would keep the preview.
        // The state itself surfaces as the `[VERBOSE]` tag on the rule line
        // rather than a chat turn, so toggling stays out of the transcript.
        self.bump_render_epoch();
    }

    /// Handle a submitted line starting with '/'. Returns true if it was a
    /// recognized command (so the caller does not start a run).
    pub(super) fn slash_command(&mut self, line: &str) -> bool {
        let cmd = line.trim();
        match cmd {
            "/clear" => {
                self.clear_log();
                true
            }
            "/compact" => {
                self.compact_now();
                true
            }
            "/recall" => {
                self.recall_now(cmd);
                true
            }
            "/quit" | "/exit" => {
                self.should_quit = true;
                true
            }
            "/help" => {
                self.show_help();
                true
            }
            "/new" => {
                self.start_new_session();
                true
            }
            "/session" => {
                self.show_session_info();
                true
            }
            "/resume" => {
                self.open_picker();
                true
            }
            "/tree" => {
                self.open_tree_picker();
                true
            }
            "/verbose" => {
                self.toggle_verbose();
                true
            }
            _ if cmd.starts_with('/') => {
                self.notify(
                    NotifyKind::Error,
                    format!("unknown command: {cmd} (try /help)"),
                );
                true
            }
            _ => false,
        }
    }

    /// Recompute the slash-command autocomplete popover from the current
    /// input. The popover is active while the input is a non-empty prefix
    /// of one or more [`SLASH_COMMANDS`] entries (e.g. `/`, `/tr`). A bare
    /// `/` matches everything; once the full command is typed exactly, the
    /// popover dismisses (nothing left to complete). Preserves the selected
    /// candidate when it's still in the new match set.
    pub(super) fn refresh_slash_complete(&mut self) {
        let input = self.input.as_str();
        if !input.starts_with('/') || input.is_empty() {
            self.slash_complete = None;
            return;
        }
        // Don't offer completion once the user has typed a full command plus
        // trailing text (e.g. `/help foo`) — there's nothing to complete.
        let candidates: Vec<usize> = SLASH_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (cmd, _))| cmd.starts_with(input))
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty() || (candidates.len() == 1 && SLASH_COMMANDS[candidates[0]].0 == input) {
            self.slash_complete = None;
            return;
        }
        // Preserve the selection if the previously-selected command is still
        // a candidate; otherwise reset to the first match.
        let prev = self.slash_complete.as_ref().and_then(|sc| {
            sc.candidates
                .get(sc.selected)
                .and_then(|&idx| candidates.iter().position(|&c| c == idx))
        });
        let selected = prev.unwrap_or(0);
        self.slash_complete = Some(SlashComplete { candidates, selected });
    }

    /// Accept the selected autocomplete candidate: replace the input with
    /// the command, position the cursor at the end, and dismiss the popover.
    pub(super) fn slash_complete_accept(&mut self) {
        if let Some(sc) = self.slash_complete.take() {
            if let Some(&idx) = sc.candidates.get(sc.selected) {
                self.input = SLASH_COMMANDS[idx].0.to_string();
                self.input_cursor = self.input.chars().count();
            }
        }
        self.slash_complete = None;
    }

    pub(super) fn slash_complete_up(&mut self) {
        if let Some(sc) = self.slash_complete.as_mut() {
            if sc.selected > 0 {
                sc.selected -= 1;
            }
        }
    }

    pub(super) fn slash_complete_down(&mut self) {
        if let Some(sc) = self.slash_complete.as_mut() {
            if sc.selected + 1 < sc.candidates.len() {
                sc.selected += 1;
            }
        }
    }

    pub(super) fn show_help(&mut self) {
        let t = self.theme;
        let mut lines: Vec<Line<'static>> = vec![info_section(t, "Keys")];
        lines.push(info_kv(t, "Enter", "send"));
        lines.push(info_kv(t, "Alt+Enter", "newline (Ctrl+J)"));
        lines.push(info_kv(t, "↑ / ↓", "move line; recall at edge"));
        lines.push(info_kv(t, "PgUp/PgDn", "scroll page (Input); move cursor page (Nav)"));
        lines.push(info_kv(t, "Tab", "switch mode: Input ↔ Navigate"));
        lines.push(info_kv(t, "Esc", "clear input"));
        lines.push(info_kv(t, "Ctrl+C", "cancel · clear · 2× quit (Input); back (Nav)"));
        lines.push(info_kv(t, "Ctrl+D", "delete char; quit on empty"));
        lines.push(Line::from(""));
        lines.push(info_section(t, "Navigate"));
        lines.push(info_kv(t, "j/k ↑↓", "scroll"));
        lines.push(info_kv(t, "h/l ←→", "move column"));
        lines.push(info_kv(t, "0 ^ $", "start / first non-blank / end"));
        lines.push(info_kv(t, "w b e", "next / prev word"));
        lines.push(info_kv(t, "g G", "top / bottom"));
        lines.push(info_kv(t, "[ ]", "jump turns"));
        lines.push(info_kv(t, "v", "select"));
        lines.push(info_kv(t, "y", "yank line"));
        lines.push(info_kv(t, "i", "back to Input"));
        lines.push(Line::from(""));
        lines.push(info_section(t, "Select"));
        lines.push(info_kv(t, "move", "extends selection"));
        lines.push(info_kv(t, "y / Enter", "yank → Input"));
        lines.push(info_kv(t, "Tab / Esc", "back"));
        lines.push(Line::from(""));
        lines.push(info_section(t, "Commands"));
        lines.push(info_kv(t, "/help", "this help"));
        lines.push(info_kv(t, "/clear", "clear log"));
        lines.push(info_kv(t, "/compact", "fold older history into a summary"));
        lines.push(info_kv(t, "/recall [query]", "search session history (incl. compacted)"));
        lines.push(info_kv(t, "/new", "start a fresh session"));
        lines.push(info_kv(t, "/resume", "pick a past session"));
        lines.push(info_kv(t, "/tree", "roll back to a past turn"));
        lines.push(info_kv(t, "/session", "show session info"));
        lines.push(info_kv(t, "/verbose", "toggle tool detail"));
        lines.push(info_kv(t, "/quit", "exit"));
        lines.push(Line::from(""));
        lines.push(info_note(t, "Type / for slash-command autocomplete (↑/↓ and Tab)."));
        self.info = Some(InfoModal {
            title: "Help".to_string(),
            lines,
            scroll: 0,
            total: 0,
            view_h: 0,
        });
    }

    pub(super) fn show_session_info(&mut self) {
        match &self.session.path {
            Some(p) => {
                let id = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?");
                self.notify(
                    NotifyKind::Info,
                    format!(
                        "{} · {} msgs · {}",
                        id,
                        self.history.lock().map_or(0, |m| m.len()),
                        self.session_model(),
                    ),
                );
            }
            None => self.notify(
                NotifyKind::Warn,
                "no session file (ephemeral or not yet started)",
            ),
        }
    }

    /// '/new': drop the transcript and start a fresh session file on the next
    /// prompt. The store is retained; only the path/name/log are reset.
    pub(super) fn start_new_session(&mut self) {
        if let Ok(mut m) = self.history.lock() {
            m.clear();
        }
        self.turns.clear();
        self.turn_byte_ranges.clear();
        self.session.path = None;
        self.pinned = true;
        self.top_line = 0;
        // Reset the footer usage/cost stats so a fresh session doesn't
        // carry over the previous one's context gauge and accumulated cost.
        self.status_usage = None;
        self.prev_ctx_tokens = None;
        self.cost = 0.0;
        self.turn_cost = 0.0;
        self.turn_has_round_usage = false;
        self.bump_render_epoch();
    }

    /// Populate the '/resume' picker with sessions for this workspace.
    pub(super) fn open_picker(&mut self) {
        let Some(store) = &self.session.store else {
            self.notify(
                NotifyKind::Warn,
                "sessions are disabled (--no-session)",
            );
            return;
        };
        match store.list_for_cwd(&self.session.cwd) {
            Ok(entries) if entries.is_empty() => {
                self.notify(
                    NotifyKind::Info,
                    "no saved sessions for this workspace",
                );
            }
            Ok(entries) => {
                self.picker = Some(PickerState {
                    entries,
                    selected: 0,
                });
            }
            Err(e) => {
                self.notify(NotifyKind::Error, format!("list sessions: {e}"));
            }
        }
    }

    /// Load the selected session into the transcript and close the picker.
    pub(super) fn picker_confirm_inner(&mut self, picker: PickerState) {
        let entry = picker.entries.into_iter().nth(picker.selected);
        let Some(entry) = entry else {
            return;
        };
        match store::load(&entry.path) {
            Ok((_meta, events, offsets, file_size)) => {
                let messages = messages_from_events(&events, &self.compaction.edit);
                if let Ok(mut m) = self.history.lock() {
                    *m = messages;
                }
                // Replay the durable event log through `apply_event` so the
                // resume path and the live path share one builder. Totals
                // (cost/usage) are restored by the replayed `TurnEnd` events,
                // not by a separate accumulator — reset them first.
                self.turns = Vec::new();
                self.turn_byte_ranges = Vec::new();
                self.cost = 0.0;
                self.total_in = 0;
                self.total_out = 0;
                self.status_usage = None;
                self.prev_ctx_tokens = None;
                for ev in replay_session_events(&events) {
                    self.apply_event(ev);
                }
                self.turn_byte_ranges =
                    turn_byte_ranges_from_events(&events, &offsets, file_size);
                // Freeze all but the last turn (file-backed; see `run_loop`).
                if self.turns.len() > 1 {
                    let n = self.turns.len();
                    for turn in &mut self.turns[..n - 1] {
                        turn.blocks.clear();
                    }
                }
                self.bump_render_epoch();
                self.session.path = Some(entry.path);
                self.pinned = true;
                self.top_line = 0;
            }
            Err(e) => {
                self.push_turn(Turn {
                    prompt: "/resume".to_string(),
                    blocks: vec![Block::Error(format!("load session: {e}"))],
                });
            }
        }
    }

    /// '/tree': open the branch-picker overlay over the active session's
    /// event log. Lists every user-prompt event (the natural branch points)
    /// with its preview. Confirmed entry feeds the prompt text back into the
    /// input (for editing) and sets the branch hint so the next run starts as
    /// a sibling of that prompt rather than appending to the active leaf.
    pub(super) fn open_tree_picker(&mut self) {
        let Some(path) = &self.session.path else {
            self.notify(
                NotifyKind::Error,
                "no session file (ephemeral or --no-session)",
            );
            return;
        };
        // Lightweight index scan (id + parent_id + kind only — no
        // ContentBlock deserialization) so /tree stays fast on large
        // sessions. Labels are loaded on demand by offset.
        let indices = match store::load_index(path) {
            Ok((_meta, indices, _size)) => indices,
            Err(e) => {
                self.notify(NotifyKind::Error, format!("load session for /tree: {e}"));
                return;
            }
        };
        let entries = build_tree_entries(&indices, self.branch_hint.as_deref(), path);
        if entries.is_empty() {
            self.notify(
                NotifyKind::Info,
                "no branch points in this session yet",
            );
            return;
        }
        let selected = entries.len().saturating_sub(1);
        self.tree_picker = Some(TreePickerState { entries, selected });
    }

    /// Confirm the hovered entry: roll the transcript back to the chosen
    /// branch point, set the branch hint so the next run chains off it, and
    /// (for "edit and resend" entries) load the original prompt into the
    /// input box. The visual rollback replaces the old "branch ready" badge —
    /// the user sees the conversation up to the branch point immediately.
    pub(super) fn tree_picker_confirm_inner(&mut self, picker: &TreePickerState) {
        let Some(entry) = picker.entries.get(picker.selected).cloned() else {
            return;
        };
        let Some(path) = self.session.path.clone() else {
            return;
        };
        // Reload events from disk (the picker was built from a snapshot; the
        // file is the source of truth for the active-path walk).
        let events = match store::load(&path) {
            Ok((_meta, events, _offsets, _size)) => events,
            Err(e) => {
                self.notify(NotifyKind::Error, format!("load session for /tree: {e}"));
                return;
            }
        };
        self.rollback_to(&events, &entry.branch_point);
        self.branch_from(entry.branch_point);
        if !entry.prefill.is_empty() {
            self.input = entry.prefill;
            self.input_cursor = self.input.chars().count();
        }
    }

    /// Key dispatch for the read-only information modal ([`InfoModal`]).
    /// `↑/↓` or `j`/`k` (and `Ctrl+N`/`Ctrl+P`, `PgUp`/`PgDn`) scroll the
    /// body; `y` copies the body to the clipboard (the modal stays open so
    /// Whether a centered modal (info, `/resume` picker, `/tree` picker) is
    /// open. While true the prompt cursor is hidden and paste is ignored.
    /// The slash-complete popover is intentionally excluded — it's inline
    /// and you're still typing into the prompt.
    pub(super) fn modal_open(&self) -> bool {
        self.info.is_some() || self.picker.is_some() || self.tree_picker.is_some()
    }

    /// you can keep reading); `Esc`/`q`/`Enter` dismiss. Other keys are
    /// swallowed. Returns `true` while the modal is open so keys don't fall
    /// through to the prompt.
    pub(super) fn handle_info_key(&mut self, k: &KeyEvent) -> bool {
        if self.info.is_none() {
            return false;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                self.info = None;
            }
            KeyCode::Char('y') => {
                if let Some(info) = self.info.as_ref() {
                    let text = info
                        .lines
                        .iter()
                        .map(|l| {
                            l.spans
                                .iter()
                                .map(|s| s.content.as_ref())
                                .collect::<String>()
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.yank_text(&text);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(i) = self.info.as_mut() {
                    i.scroll_down();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = self.info.as_mut() {
                    i.scroll_up();
                }
            }
            KeyCode::Char('n') if ctrl => {
                if let Some(i) = self.info.as_mut() {
                    i.scroll_down();
                }
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(i) = self.info.as_mut() {
                    i.scroll_up();
                }
            }
            KeyCode::PageDown => {
                if let Some(i) = self.info.as_mut() {
                    i.scroll_page_down();
                }
            }
            KeyCode::PageUp => {
                if let Some(i) = self.info.as_mut() {
                    i.scroll_page_up();
                }
            }
            _ => {}
        }
        true
    }

    /// Unified key dispatch for list-style modal overlays (`/resume` and
    /// `/tree`). `↑/↓` or `j`/`k` or `Ctrl+N`/`Ctrl+P` move the selection
    /// (clamped); `Tab`/`Shift+Tab` cycle with wrap-around; `Enter`
    /// confirms; `Esc`/`q` cancels. Returns `true` if a modal handled the
    /// key (so the caller skips normal Input-mode processing).
    pub(super) fn handle_modal_key(&mut self, k: &KeyEvent) -> bool {
        /// Which overlay slot is active, for per-slot confirm/cancel.
        enum Slot { Picker, Tree }
        let slot = if self.picker.is_some() {
            Slot::Picker
        } else if self.tree_picker.is_some() {
            Slot::Tree
        } else {
            return false;
        };
        let len = self.active_modal_mut().map_or(0, |m| m.len());
        // Confirm/cancel (and the single-item Tab shortcut) take `&mut self`
        // (or take the picker) and are handled before borrowing the modal for
        // navigation.
        match k.code {
            KeyCode::Enter => match slot {
                Slot::Picker => {
                    if let Some(picker) = self.picker.take() {
                        self.picker_confirm_inner(picker);
                    }
                }
                Slot::Tree => {
                    if let Some(picker) = self.tree_picker.take() {
                        self.tree_picker_confirm_inner(&picker);
                    }
                }
            },
            // With a single entry, Tab/Shift+Tab confirm outright instead of
            // cycling (a no-op) — same as pressing Enter.
            KeyCode::Tab | KeyCode::BackTab if len == 1 => match slot {
                Slot::Picker => {
                    if let Some(picker) = self.picker.take() {
                        self.picker_confirm_inner(picker);
                    }
                }
                Slot::Tree => {
                    if let Some(picker) = self.tree_picker.take() {
                        self.tree_picker_confirm_inner(&picker);
                    }
                }
            },
            KeyCode::Esc | KeyCode::Char('q') => match slot {
                Slot::Picker => self.picker = None,
                Slot::Tree => self.tree_picker = None,
            },
            _ => {}
        }
        if (matches!(slot, Slot::Picker) && self.picker.is_none())
            || (matches!(slot, Slot::Tree) && self.tree_picker.is_none())
        {
            // Confirm/cancel consumed the overlay; nothing left to navigate.
            return true;
        }
        let Some(m) = self.active_modal_mut() else { return true };
        if len == 0 {
            return true;
        }
        let s = m.selected();
        match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                m.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            KeyCode::Down | KeyCode::Char('j') => {
                m.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            // Ctrl+N / Ctrl+P — readline-style next/previous, matching the
            // popover and the Input-mode cursor keys.
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                m.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                m.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            // Tab/Shift+Tab cycle with wrap-around (last ↔ first).
            KeyCode::Tab => {
                m.set_selected((s + 1) % len);
            }
            KeyCode::BackTab => {
                m.set_selected(if s == 0 { len - 1 } else { s - 1 });
            }
            _ => {}
        }
        true
    }

    /// Borrow whichever modal overlay is currently active, for shared
    /// navigation. Only one slot is ever non-`None` at a time.
    pub(super) fn active_modal_mut(&mut self) -> Option<&mut dyn Modal> {
        if self.picker.is_some() {
            self.picker.as_mut().map(|p| p as &mut dyn Modal)
        } else {
            self.tree_picker.as_mut().map(|t| t as &mut dyn Modal)
        }
    }

    /// Unified key dispatch for the slash-command autocomplete popover.
    /// `↑/↓` or `Ctrl+N`/`Ctrl+P` move the selection (clamped); `Tab`/
    /// `Shift+Tab` cycle with wrap-around (last ↔ first); `Enter` accepts
    /// the selection (auto-completes); `Esc` dismisses. Unlike
    /// [`handle_modal_key`], `j`/`k`/`q` are not intercepted — the popover
    /// floats over a text input, so those must stay printable. Returns
    /// `true` if the popover handled the key.
    pub(super) fn handle_popover_key(&mut self, k: &KeyEvent) -> bool {
        if self.slash_complete.is_none() {
            return false;
        }
        let len = self.slash_complete.as_ref().map_or(0, super::Popover::len);
        // Accept/dismiss (and the single-item Tab shortcut) take `&mut self`
        // and are handled before borrowing the popover for navigation.
        match k.code {
            KeyCode::Enter => {
                self.slash_complete_accept();
                return true;
            }
            // With a single candidate, Tab/Shift+Tab accept it outright
            // instead of cycling (a no-op) — same as pressing Enter.
            KeyCode::Tab | KeyCode::BackTab if len == 1 => {
                self.slash_complete_accept();
                return true;
            }
            KeyCode::Esc => {
                self.slash_complete = None;
                return true;
            }
            _ => {}
        }
        let Some(popover) = self.slash_complete.as_mut().map(|p| p as &mut dyn Popover)
        else {
            return false;
        };
        if len == 0 {
            return false;
        }
        let s = popover.selected();
        match k.code {
            KeyCode::Up => {
                popover.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            KeyCode::Down => {
                popover.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            // Ctrl+N / Ctrl+P — readline-style next/previous, matching the
            // Input-mode cursor keys.
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                popover.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                popover.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            // Tab/Shift+Tab cycle with wrap-around (last ↔ first).
            KeyCode::Tab => {
                popover.set_selected((s + 1) % len);
            }
            KeyCode::BackTab => {
                popover.set_selected(if s == 0 { len - 1 } else { s - 1 });
            }
            _ => return false,
        }
        true
    }

    /// Test helper: confirm the active tree picker. In production,
    /// [`handle_modal_key`] dispatches Enter through the per-slot
    /// `tree_picker_confirm_inner`.
    #[cfg(test)]
    pub(super) fn tree_picker_confirm(&mut self) {
        if let Some(picker) = self.tree_picker.take() {
            self.tree_picker_confirm_inner(&picker);
        }
    }

    /// Rebuild the visible turns and the agent's message history from the
    /// active path root → `leaf_id` (inclusive), discarding everything after
    /// that point from the in-memory view. Cost/usage are reset and
    /// re-accumulated from the replayed `turn_end` events. Byte ranges are
    /// dropped: after a rollback the visible turns are rendered from
    /// in-memory blocks, not the file-backed frozen-turn cache (the cache is
    /// invalidated by `bump_render_epoch`). The on-disk file is untouched —
    /// the rolled-back branches remain and are reachable via `/tree` again.
    pub(super) fn rollback_to(&mut self, events: &[SessionEvent], leaf_id: &str) {
        let path = store::active_path(events, leaf_id);
        let rolled_back: Vec<SessionEvent> =
            path.iter().map(|&i| events[i].clone()).collect();
        let messages = messages_from_events(&rolled_back, &self.compaction.edit);
        if let Ok(mut m) = self.history.lock() {
            *m = messages;
        }
        self.turns = Vec::new();
        self.turn_byte_ranges = Vec::new();
        self.cost = 0.0;
        self.total_in = 0;
        self.total_out = 0;
        self.status_usage = None;
        self.prev_ctx_tokens = None;
        for ev in replay_session_events(&rolled_back) {
            self.apply_event(ev);
        }
        self.bump_render_epoch();
        self.pinned = true;
        self.top_line = 0;
    }

    /// Post a transient slash-command notification on the rule line's left
    /// edge. Replaces any prior notification. Use instead of pushing a chat
    /// turn for short status/error feedback so the transcript stays clean.
    pub(super) fn notify(&mut self, kind: NotifyKind, msg: impl Into<String>) {
        self.notify = Some(Notify {
            msg: msg.into(),
            kind,
            at: Instant::now(),
        });
    }
}