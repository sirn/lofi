use super::*;

impl App {
    /// Handle a submitted line starting with '/'. Returns true if it was a
    /// recognized command (so the caller does not start a run).
    pub(in crate::tui) fn slash_command(&mut self, line: &str) -> bool {
        let cmd = line.trim();
        if self.external_backend
            && matches!(
                cmd,
                "/clear" | "/compact" | "/new" | "/resume" | "/tree" | "/thinking" | "/service"
            )
        {
            self.notify(NotifyKind::Warn, "this operation is not supported by the external backend; start or resume a session from the CLI");
            return true;
        }
        match cmd {
            "/clear" => {
                self.clear_log();
                true
            }
            "/compact" => {
                self.compact_now();
                true
            }
            "/debug" => {
                self.toggle_debug();
                true
            }
            _ if cmd == "/recall" || cmd.starts_with("/recall ") => {
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
            "/model" => {
                self.open_model_picker();
                true
            }
            "/thinking" => {
                self.open_thinking_picker();
                true
            }
            "/service" => {
                self.open_service_picker();
                true
            }
            "/theme" => {
                self.open_theme_picker();
                true
            }
            "/policy" => {
                self.open_policy_picker();
                true
            }
            "/job" | "/jobs" => {
                self.open_jobs_modal();
                true
            }
            "/subagent" | "/subagents" => {
                self.open_subagents_modal();
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

    pub(in crate::tui) fn refresh_slash_complete(&mut self) {
        let input = self.input.as_str();
        if !input.starts_with('/') || input.is_empty() {
            self.slash_complete = None;
            return;
        }
        let candidates: Vec<usize> = SLASH_COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, (cmd, _))| cmd.starts_with(input))
            .map(|(i, _)| i)
            .collect();
        if candidates.is_empty()
            || (candidates.len() == 1 && SLASH_COMMANDS[candidates[0]].0 == input)
        {
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
        self.slash_complete = Some(SlashComplete {
            candidates,
            selected,
        });
    }

    pub(in crate::tui) fn slash_complete_accept(&mut self) {
        if let Some(sc) = self.slash_complete.take() {
            if let Some(&idx) = sc.candidates.get(sc.selected) {
                self.input = SLASH_COMMANDS[idx].0.to_string();
                self.input_cursor = self.input.chars().count();
            }
        }
        self.slash_complete = None;
    }

    pub(in crate::tui) fn slash_complete_up(&mut self) {
        if let Some(sc) = self.slash_complete.as_mut() {
            if sc.selected > 0 {
                sc.selected -= 1;
            }
        }
    }

    pub(in crate::tui) fn slash_complete_down(&mut self) {
        if let Some(sc) = self.slash_complete.as_mut() {
            if sc.selected + 1 < sc.candidates.len() {
                sc.selected += 1;
            }
        }
    }

    pub(in crate::tui) fn show_help(&mut self) {
        let t = self.theme;
        let mut lines = vec![info_section("Keys")];
        lines.push(info_kv(t, "Enter", "send"));
        lines.push(info_kv(t, "Alt+Enter", "newline (Ctrl+J)"));
        lines.push(info_kv(t, "Alt+Up", "restore queued prompt"));
        lines.push(info_kv(t, "↑ / ↓", "move line; recall at edge"));
        lines.push(info_kv(
            t,
            "PgUp/PgDn",
            "scroll page (Input); move cursor page (Nav)",
        ));
        lines.push(info_kv(t, "Tab", "switch mode: Input ↔ Navigate"));
        lines.push(info_kv(t, "Esc", "cancel run; otherwise clear input"));
        lines.push(info_kv(
            t,
            "Ctrl+C",
            "clear · cancel · 2× quit (Input); back (Nav)",
        ));
        lines.push(info_kv(
            t,
            "Ctrl+D",
            "delete char; cancel run or quit on empty",
        ));
        lines.push(info_kv(
            t,
            "!command",
            "run shell command; include in context",
        ));
        lines.push(info_kv(
            t,
            "!!command",
            "run shell command; omit from context",
        ));
        lines.push(InfoLine::Text(Line::from("")));
        lines.push(info_section("Navigate"));
        lines.push(info_kv(t, "j/k ↑↓", "scroll"));
        lines.push(info_kv(t, "h/l ←→", "move column"));
        lines.push(info_kv(t, "Enter/Space", "open detail / enter scroll"));
        lines.push(info_kv(t, "j/k PgUp/PgDn", "move in focused detail"));
        lines.push(info_kv(t, "0 ^ $", "start / first non-blank / end"));
        lines.push(info_kv(t, "w b e", "next / prev word"));
        lines.push(info_kv(t, "g G", "top / bottom"));
        lines.push(info_kv(t, "[ ]", "jump turns"));
        lines.push(info_kv(t, "v", "select"));
        lines.push(info_kv(t, "y", "yank line"));
        lines.push(info_kv(t, "i", "back to Input"));
        lines.push(InfoLine::Text(Line::from("")));
        lines.push(info_section("Select"));
        lines.push(info_kv(t, "move", "extends selection"));
        lines.push(info_kv(t, "y", "yank → Input"));
        lines.push(info_kv(t, "Enter/Space", "leave focused detail"));
        lines.push(info_kv(t, "Tab / Esc", "back"));
        lines.push(InfoLine::Text(Line::from("")));
        lines.push(info_section("Commands"));
        lines.push(info_kv(t, "/help", "this help"));
        lines.push(info_kv(t, "/clear", "clear log"));
        lines.push(info_kv(t, "/compact", "fold older history into a summary"));
        lines.push(info_kv(t, "/debug", "toggle resource diagnostics"));
        lines.push(info_kv(
            t,
            "/recall [query]",
            "search session history (incl. compacted)",
        ));
        lines.push(info_kv(t, "/new", "start a fresh session"));
        lines.push(info_kv(t, "/resume", "pick a past session"));
        lines.push(info_kv(t, "/tree", "roll back to a past turn"));
        lines.push(info_kv(t, "/session", "show session info"));
        lines.push(info_kv(t, "/model", "switch the active model"));
        lines.push(info_kv(t, "/theme", "switch color scheme for this session"));
        lines.push(info_kv(
            t,
            "/policy",
            "change bash approval mode for this session",
        ));
        lines.push(info_kv(t, "/thinking", "switch the thinking level"));
        lines.push(info_kv(t, "/service", "switch the service tier"));
        lines.push(info_kv(
            t,
            "/job",
            "list background jobs, view output, stop",
        ));
        lines.push(info_kv(t, "/subagent", "list subagents, view a transcript"));
        lines.push(info_kv(t, "/quit", "exit"));
        lines.push(InfoLine::Text(Line::from("")));
        lines.push(info_note(
            t,
            "Type / for slash-command autocomplete (↑/↓ and Tab).",
        ));
        self.info = Some(InfoModal {
            title: "Help".to_string(),
            lines,
            scroll: 0,
            total: 0,
            view_h: 0,
        });
    }

    pub(in crate::tui) fn show_session_info(&mut self) {
        let t = self.theme;
        let mut lines = vec![info_section("Session")];
        #[allow(clippy::single_match_else)]
        match self.session.cursor.as_ref() {
            Some(cursor) => {
                lines.push(info_kv(t, "id", &cursor.id()));
                lines.push(info_kv(t, "file", &cursor.path().display().to_string()));
                lines.push(info_kv(t, "size", &format_bytes(cursor.len())));
            }
            None => {
                lines.push(info_kv(t, "id", "(none)"));
                lines.push(info_note(
                    t,
                    "No session file — ephemeral or not yet started.",
                ));
            }
        }
        lines.push(InfoLine::Text(Line::from("")));
        lines.push(info_section("Model"));
        lines.push(info_kv(t, "model", &self.session_model()));
        lines.push(info_kv(
            t,
            "messages",
            &self.lifecycle.history_stats().messages.to_string(),
        ));
        lines.push(info_kv(
            t,
            "turns",
            &self.transcript.turns.len().to_string(),
        ));
        lines.push(InfoLine::Text(Line::from("")));
        lines.push(info_section("Workspace"));
        lines.push(info_kv(t, "root", &self.session.cwd.display().to_string()));
        self.info = Some(InfoModal {
            title: "Session".to_string(),
            lines,
            scroll: 0,
            total: 0,
            view_h: 0,
        });
    }

    pub(in crate::tui) fn start_new_session(&mut self) {
        if self.busy() {
            self.notify(
                NotifyKind::Warn,
                "wait for the running turn to finish before starting a new session",
            );
            return;
        }
        self.reset_session_jobs();
        self.record_subagent_spend();
        self.clear_subagents();
        if let Err(error) = self.lifecycle.clear_history() {
            self.notify(NotifyKind::Error, format!("clear agent history: {error}"));
        }
        self.transcript.clear();
        self.expanded_details.clear();
        self.detail_focus = None;
        if let Some(sink) = self.session.sink.as_mut() {
            sink.clear_cursor();
        }
        self.session.cursor = None;
        self.pinned = true;
        self.top_line = 0;
        // Reset the footer usage/cost stats so a fresh session doesn't
        // carry over the previous one's context gauge and accumulated cost.
        self.reset_compaction_gauges();
        self.cost = 0.0;
        self.turn_cost = 0.0;
        self.turn_has_round_usage = false;
        self.bump_render_epoch();
    }

    pub(super) fn reset_session_jobs(&mut self) {
        if let Some(jobs) = &self.jobs {
            jobs.reset();
            self.jobs_receiver_stale = true;
        }
        self.notified_jobs.clear();
        self.jobs_modal = None;
        self.prompt_queue
            .retain(|queued| queued.kind == lofi_types::PromptKind::User);
        self.startup_notices.clear();
    }
}
