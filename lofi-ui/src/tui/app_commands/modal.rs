use super::*;

#[derive(Clone, Copy)]
enum ModalSlot {
    Picker,
    Tree,
    Model,
    Thinking,
    Policy,
    Service,
    Theme,
}

impl App {
    pub(in crate::tui) fn queue_confirmation(&mut self, req: lofi_core::ConfirmRequest) {
        if Self::confirmation_ready(&req) {
            self.pending_confirms.push(req);
        } else {
            self.deferred_confirms.push(req);
        }
    }

    /// Retire completed requests and promote auto-mode evaluations once they
    /// need user attention. The grace period is a presentation policy: core
    /// and tool layers emit the evaluating request immediately.
    pub(in crate::tui) fn refresh_confirmations(&mut self) -> bool {
        let old_front = self.pending_confirms.first().map(|req| req.id);
        let old_pending = self.pending_confirms.len();
        let old_deferred = self.deferred_confirms.len();

        self.pending_confirms
            .retain(|req| req.active.load(std::sync::atomic::Ordering::Relaxed));

        let mut waiting = Vec::with_capacity(self.deferred_confirms.len());
        for req in self.deferred_confirms.drain(..) {
            if !req.active.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            if Self::confirmation_ready(&req) {
                self.pending_confirms.push(req);
            } else {
                waiting.push(req);
            }
        }
        self.deferred_confirms = waiting;

        let new_front = self.pending_confirms.first().map(|req| req.id);
        if old_front != new_front {
            self.confirm_selected = 0;
            self.confirm_scroll = 0;
            self.confirm_total = 0;
            self.confirm_view_h = 0;
        }

        old_pending != self.pending_confirms.len()
            || old_deferred != self.deferred_confirms.len()
            || old_front != new_front
    }

    fn confirmation_ready(req: &lofi_core::ConfirmRequest) -> bool {
        match *req
            .reason
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            lofi_core::ConfirmReason::AutoEvaluating { started_at } => {
                started_at.elapsed() >= AUTO_MODE_UI_GRACE
            }
            _ => true,
        }
    }

    /// Whether a centered modal is open. The inline slash-complete popover is
    /// excluded because input remains active beneath it.
    pub(in crate::tui) fn modal_open(&self) -> bool {
        self.info.is_some()
            || self.picker.is_some()
            || self.tree_picker.is_some()
            || self.model_picker.is_some()
            || self.thinking_picker.is_some()
            || self.policy_picker.is_some()
            || self.service_picker.is_some()
            || self.theme_picker.is_some()
            || self.jobs_modal.is_some()
            || self.subagents_modal.is_some()
            || !self.pending_confirms.is_empty()
    }

    /// Dispatch a key to the topmost visible popup. This order is the reverse
    /// of the popup render order in `view::render`.
    pub(in crate::tui) fn handle_modal_stack_key(&mut self, k: &KeyEvent) -> bool {
        if !self.pending_confirms.is_empty() {
            return self.handle_confirm_key(k);
        }
        if self.info.is_some() {
            return self.handle_info_key(k);
        }
        if self.jobs_modal.is_some()
            || self.subagents_modal.is_some()
            || self.active_modal_slot().is_some()
        {
            return self.handle_modal_key(k);
        }
        self.handle_popover_key(k)
    }

    pub(in crate::tui) fn handle_confirm_key(&mut self, k: &KeyEvent) -> bool {
        if self.pending_confirms.is_empty() {
            return false;
        }
        // Auto-mode may have approved between the last draw and this key.
        // Consume the key while dismissing that stale modal rather than
        // letting it leak through to the prompt or a queued confirmation.
        if !self.pending_confirms[0]
            .active
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            self.pending_confirms.remove(0);
            self.confirm_selected = 0;
            self.confirm_scroll = 0;
            self.confirm_total = 0;
            self.confirm_view_h = 0;
            return true;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let max_scroll = self.confirm_total.saturating_sub(self.confirm_view_h);
        let response = match k.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.confirm_scroll = self.confirm_scroll.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.confirm_scroll = self.confirm_scroll.saturating_add(1).min(max_scroll);
                None
            }
            KeyCode::PageUp => {
                self.confirm_scroll = self
                    .confirm_scroll
                    .saturating_sub(self.confirm_view_h.max(1));
                None
            }
            KeyCode::PageDown => {
                self.confirm_scroll = self
                    .confirm_scroll
                    .saturating_add(self.confirm_view_h.max(1))
                    .min(max_scroll);
                None
            }
            KeyCode::Home => {
                self.confirm_scroll = 0;
                None
            }
            KeyCode::End => {
                self.confirm_scroll = max_scroll;
                None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.confirm_selected = 0;
                None
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.confirm_selected = 1;
                None
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.confirm_selected = 1 - self.confirm_selected.min(1);
                None
            }
            KeyCode::Enter => Some(self.confirm_selected == 0),
            KeyCode::Char('a' | 'A' | 'y' | 'Y') => Some(true),
            KeyCode::Char('d' | 'D' | 'n' | 'N') | KeyCode::Esc => Some(false),
            KeyCode::Char('c') if ctrl => Some(false),
            _ => None,
        };
        if let Some(approved) = response {
            let req = self.pending_confirms.remove(0);
            req.active
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let _ = req.respond.send(approved);
            self.confirm_selected = 0;
            self.confirm_scroll = 0;
            self.confirm_total = 0;
            self.confirm_view_h = 0;
        }
        true
    }

    /// Handle keys for the read-only information modal. Copy leaves the modal
    /// open; dismiss and navigation keys are consumed instead of reaching the
    /// prompt.
    pub(in crate::tui) fn handle_info_key(&mut self, k: &KeyEvent) -> bool {
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
                        .map(|line| match line {
                            InfoLine::Section(label) => label.clone(),
                            InfoLine::Text(line) => line
                                .spans
                                .iter()
                                .map(|span| span.content.as_ref())
                                .collect::<String>(),
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

    fn active_modal_slot(&self) -> Option<ModalSlot> {
        if self.theme_picker.is_some() {
            Some(ModalSlot::Theme)
        } else if self.service_picker.is_some() {
            Some(ModalSlot::Service)
        } else if self.thinking_picker.is_some() {
            Some(ModalSlot::Thinking)
        } else if self.policy_picker.is_some() {
            Some(ModalSlot::Policy)
        } else if self.model_picker.is_some() {
            Some(ModalSlot::Model)
        } else if self.tree_picker.is_some() {
            Some(ModalSlot::Tree)
        } else if self.picker.is_some() {
            Some(ModalSlot::Picker)
        } else {
            None
        }
    }

    /// Unified key dispatch for list-style modal overlays (`/resume` and
    /// `/tree`). `↑/↓` or `j`/`k` or `Ctrl+N`/`Ctrl+P` move the selection
    /// (clamped); `Tab`/`Shift+Tab` cycle with wrap-around; `Enter`
    /// confirms; `Esc`/`q` cancels. Returns `true` if a modal handled the
    /// key (so the caller skips normal Input-mode processing).
    #[allow(clippy::too_many_lines)]
    pub(in crate::tui) fn handle_modal_key(&mut self, k: &KeyEvent) -> bool {
        if self.jobs_modal.is_some() {
            return self.handle_jobs_key(k);
        }
        if self.subagents_modal.is_some() {
            return self.handle_subagents_key(k);
        }
        let Some(slot) = self.active_modal_slot() else {
            return false;
        };
        let len = self.active_modal_mut().map_or(0, |m| m.len());
        // A progressive picker draws before it has rows. Escape still closes
        // it, but Enter cannot confirm an absent selection.
        if len == 0 && matches!(k.code, KeyCode::Enter | KeyCode::Tab | KeyCode::BackTab) {
            return true;
        }
        match k.code {
            KeyCode::Enter => match slot {
                ModalSlot::Picker => {
                    if let Some(picker) = self.picker.take() {
                        self.picker_confirm_inner(picker);
                    }
                }
                ModalSlot::Tree => {
                    if let Some(picker) = self.tree_picker.take() {
                        self.tree_picker_confirm_inner(picker);
                    }
                }
                ModalSlot::Model => self.model_picker_confirm(),
                ModalSlot::Thinking => self.thinking_picker_confirm(),
                ModalSlot::Policy => self.policy_picker_confirm(),
                ModalSlot::Service => self.service_picker_confirm(),
                ModalSlot::Theme => self.theme_picker_confirm(),
            },
            // With a single entry, Tab/Shift+Tab confirm outright instead of
            // cycling (a no-op) — same as pressing Enter.
            KeyCode::Tab | KeyCode::BackTab if len == 1 => match slot {
                ModalSlot::Picker => {
                    if let Some(picker) = self.picker.take() {
                        self.picker_confirm_inner(picker);
                    }
                }
                ModalSlot::Tree => {
                    if let Some(picker) = self.tree_picker.take() {
                        self.tree_picker_confirm_inner(picker);
                    }
                }
                ModalSlot::Model => self.model_picker_confirm(),
                ModalSlot::Thinking => self.thinking_picker_confirm(),
                ModalSlot::Policy => self.policy_picker_confirm(),
                ModalSlot::Service => self.service_picker_confirm(),
                ModalSlot::Theme => self.theme_picker_confirm(),
            },
            KeyCode::Esc | KeyCode::Char('q') => match slot {
                ModalSlot::Picker => {
                    self.picker = None;
                    self.picker_generation.fetch_add(1, Ordering::Relaxed);
                }
                ModalSlot::Tree => {
                    let entries = self
                        .tree_picker
                        .take()
                        .map(|picker| picker.entries)
                        .unwrap_or_default();
                    self.picker_generation.fetch_add(1, Ordering::Relaxed);
                    self.release_tree_resources(entries);
                    self.tree_picker_pending.clear();
                }
                ModalSlot::Model => self.model_picker = None,
                ModalSlot::Thinking => self.thinking_picker = None,
                ModalSlot::Policy => self.policy_picker = None,
                ModalSlot::Service => self.service_picker = None,
                ModalSlot::Theme => self.theme_picker = None,
            },
            _ => {}
        }
        if (matches!(slot, ModalSlot::Picker) && self.picker.is_none())
            || (matches!(slot, ModalSlot::Tree) && self.tree_picker.is_none())
            || (matches!(slot, ModalSlot::Model) && self.model_picker.is_none())
            || (matches!(slot, ModalSlot::Thinking) && self.thinking_picker.is_none())
            || (matches!(slot, ModalSlot::Policy) && self.policy_picker.is_none())
            || (matches!(slot, ModalSlot::Service) && self.service_picker.is_none())
            || (matches!(slot, ModalSlot::Theme) && self.theme_picker.is_none())
        {
            return true;
        }
        let Some(m) = self.active_modal_mut() else {
            return true;
        };
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
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                m.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                m.set_selected(if s > 0 { s - 1 } else { 0 });
            }
            KeyCode::Tab => {
                m.set_selected((s + 1) % len);
            }
            KeyCode::BackTab => {
                m.set_selected(if s == 0 { len - 1 } else { s - 1 });
            }
            _ => {}
        }
        if self.tree_picker.is_some() {
            self.request_tree_viewport();
        }
        true
    }

    pub(in crate::tui) fn active_modal_mut(&mut self) -> Option<&mut dyn Modal> {
        if self.theme_picker.is_some() {
            self.theme_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else if self.service_picker.is_some() {
            self.service_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else if self.thinking_picker.is_some() {
            self.thinking_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else if self.policy_picker.is_some() {
            self.policy_picker.as_mut().map(|p| p as &mut dyn Modal)
        } else if self.model_picker.is_some() {
            self.model_picker.as_mut().map(|m| m as &mut dyn Modal)
        } else if self.tree_picker.is_some() {
            self.tree_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else {
            self.picker.as_mut().map(|p| p as &mut dyn Modal)
        }
    }

    pub(in crate::tui) fn handle_popover_key(&mut self, k: &KeyEvent) -> bool {
        if self.slash_complete.is_none() {
            return false;
        }
        let len = self.slash_complete.as_ref().map_or(0, super::Popover::len);
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
        let Some(popover) = self.slash_complete.as_mut().map(|p| p as &mut dyn Popover) else {
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
            KeyCode::Char('n') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                popover.set_selected(if s + 1 < len { s + 1 } else { s });
            }
            KeyCode::Char('p') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                popover.set_selected(if s > 0 { s - 1 } else { 0 });
            }
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
}
