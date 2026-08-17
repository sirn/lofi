#![allow(clippy::wildcard_imports)]

use super::*;

#[derive(Clone, Copy)]
enum ModalSlot {
    Picker,
    Tree,
    Model,
    Thinking,
    Service,
    Theme,
}

fn run_resume_load(
    generation: u64,
    generation_clock: &AtomicU64,
    files: &[store::SessionFile],
    tx: &tokio::sync::mpsc::UnboundedSender<PickerLoad>,
) {
    for (index, file) in files.iter().enumerate() {
        if generation_clock.load(Ordering::Relaxed) != generation {
            return;
        }
        if let Some(preview) = file.quick_preview() {
            if tx
                .send(PickerLoad::ResumePreviews {
                    generation,
                    rows: vec![(index, preview)],
                })
                .is_err()
            {
                return;
            }
        }
    }
    for (index, file) in files.iter().enumerate() {
        if generation_clock.load(Ordering::Relaxed) != generation {
            return;
        }
        let Some(entry) = file.inspect() else {
            continue;
        };
        if tx
            .send(PickerLoad::ResumeRows {
                generation,
                rows: vec![(index, entry)],
            })
            .is_err()
        {
            return;
        }
    }
}

fn run_tree_snapshot(
    generation: u64,
    generation_clock: &AtomicU64,
    cursor: &store::SessionCursor,
    tx: &tokio::sync::mpsc::UnboundedSender<PickerLoad>,
    state: &mut lofi_core::session::io::WorkerState,
) {
    match cursor.tree_snapshot() {
        Ok(snapshot) => {
            if generation_clock.load(Ordering::Relaxed) != generation {
                return;
            }
            let skeletons =
                build_tree_entry_skeletons(&snapshot.index, snapshot.leaf_id.as_deref(), cursor);
            // Pin the snapshot on this worker; the UI gets only an id. All
            // subsequent hydration looks the snapshot up by id, and picker
            // close drops it here — so the multi-MB index Vec is allocated
            // and freed on the same thread.
            let snapshot_id = state.insert_tree_snapshot(snapshot);
            if tx
                .send(PickerLoad::TreeReady {
                    generation,
                    entries: skeletons.clone(),
                    snapshot_id,
                })
                .is_err()
            {
                state.remove_tree_snapshot(snapshot_id);
                return;
            }
            // Look the snapshot back up rather than holding a borrow across
            // the channel send above.
            let Some(snapshot) = state.tree_snapshot(snapshot_id) else {
                return;
            };
            let start = skeletons.len().saturating_sub(20);
            hydrate_tree_entry_window(
                &snapshot.index,
                cursor,
                &skeletons,
                start..skeletons.len(),
                |rows| tx.send(PickerLoad::TreeRows { generation, rows }).is_ok(),
                || generation_clock.load(Ordering::Relaxed) != generation,
            );
        }
        Err(error) => {
            let _ = tx.send(PickerLoad::TreeFailed {
                generation,
                error: error.to_string(),
            });
        }
    }
}

impl App {
    pub(super) fn toggle_verbose(&mut self) {
        self.debug_sample("verbose");
        self.verbose = !self.verbose;
        if self.verbose {
            self.restore_last_committed_exec_results();
        } else {
            self.release_last_committed_exec_results();
        }
        // Frozen styled rows are mode-specific, but retain and swap the tiny
        // per-mode height indexes so toggling back does not reparse every turn.
        // The state itself surfaces as the `[VERBOSE]` tag on the rule line
        // rather than a chat turn, so toggling stays out of the transcript.
        self.switch_verbose_layout();
        self.debug_after_draw = Some("verbose");
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
            "/verbose" => {
                self.toggle_verbose();
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
            "/job" | "/jobs" => {
                self.open_jobs_modal();
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

    pub(super) fn refresh_slash_complete(&mut self) {
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
        lines.push(info_kv(t, "Alt+Up", "restore queued prompt"));
        lines.push(info_kv(t, "↑ / ↓", "move line; recall at edge"));
        lines.push(info_kv(
            t,
            "PgUp/PgDn",
            "scroll page (Input); move cursor page (Nav)",
        ));
        lines.push(info_kv(t, "Tab", "switch mode: Input ↔ Navigate"));
        lines.push(info_kv(t, "Esc", "clear input"));
        lines.push(info_kv(
            t,
            "Ctrl+C",
            "cancel · clear · 2× quit (Input); back (Nav)",
        ));
        lines.push(info_kv(t, "Ctrl+D", "delete char; quit on empty"));
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
        lines.push(info_kv(t, "/thinking", "switch the thinking level"));
        lines.push(info_kv(t, "/service", "switch the service tier"));
        lines.push(info_kv(
            t,
            "/job",
            "list background jobs, view output, stop",
        ));
        lines.push(info_kv(t, "/verbose", "toggle tool detail"));
        lines.push(info_kv(t, "/quit", "exit"));
        lines.push(Line::from(""));
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

    pub(super) fn show_session_info(&mut self) {
        let t = self.theme;
        let mut lines: Vec<Line<'static>> = vec![info_section(t, "Session")];
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
        lines.push(Line::from(""));
        lines.push(info_section(t, "Model"));
        lines.push(info_kv(t, "model", &self.session_model()));
        lines.push(info_kv(
            t,
            "messages",
            &self.lifecycle.history_stats().messages.to_string(),
        ));
        lines.push(info_kv(t, "turns", &self.turns.len().to_string()));
        lines.push(Line::from(""));
        lines.push(info_section(t, "Workspace"));
        lines.push(info_kv(t, "root", &self.session.cwd.display().to_string()));
        self.info = Some(InfoModal {
            title: "Session".to_string(),
            lines,
            scroll: 0,
            total: 0,
            view_h: 0,
        });
    }

    pub(super) fn start_new_session(&mut self) {
        self.reset_session_jobs();
        if let Err(error) = self.lifecycle.clear_history() {
            self.notify(NotifyKind::Error, format!("clear agent history: {error}"));
        }
        self.turns.clear();
        self.collapsed_turns.get_mut().clear();
        self.turn_byte_ranges.clear();
        self.turn_event_offsets.clear();
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

    fn reset_session_jobs(&mut self) {
        if let Some(jobs) = &self.jobs {
            jobs.reset();
            self.jobs_receiver_stale = true;
        }
        self.jobs_modal = None;
        self.prompt_queue
            .retain(|queued| queued.kind == lofi_types::PromptKind::User);
        self.startup_notices.clear();
    }

    pub(super) fn open_picker(&mut self) {
        // Read-only listing via the core sink — the UI holds no store handle.
        let files = self
            .session
            .sink
            .as_ref()
            .map(lofi_core::session::sink::SessionSink::workspace_sessions);
        let Some(files) = files else {
            self.notify(NotifyKind::Warn, "sessions are disabled (--no-session)");
            return;
        };
        match files {
            Ok(files) if files.is_empty() => {
                self.notify(NotifyKind::Info, "no saved sessions for this workspace");
            }
            Ok(files) => {
                let generation = self
                    .picker_generation
                    .fetch_add(1, Ordering::Relaxed)
                    .wrapping_add(1);
                self.picker = Some(PickerState {
                    entries: files
                        .iter()
                        .cloned()
                        .map(|file| PickerEntry {
                            file,
                            preview: None,
                            details: None,
                        })
                        .collect(),
                    selected: 0,
                    generation,
                });
                if let (Some(tx), Some(sink)) =
                    (self.picker_load_tx.clone(), self.session.sink.as_ref())
                {
                    let generation_clock = Arc::clone(&self.picker_generation);
                    sink.submit_io(Box::new(move |_state| {
                        run_resume_load(generation, &generation_clock, &files, &tx);
                    }));
                } else if let Some(picker) = self.picker.as_mut() {
                    for row in &mut picker.entries {
                        row.preview = row.file.quick_preview();
                        row.details = row.file.inspect();
                    }
                }
            }
            Err(e) => {
                self.notify(NotifyKind::Error, format!("list sessions: {e}"));
            }
        }
    }

    pub(super) fn apply_picker_load(&mut self, load: PickerLoad) {
        match load {
            PickerLoad::ResumePreviews { generation, rows } => {
                let Some(picker) = self
                    .picker
                    .as_mut()
                    .filter(|picker| picker.generation == generation)
                else {
                    return;
                };
                for (index, preview) in rows {
                    if let Some(row) = picker.entries.get_mut(index) {
                        row.preview = Some(preview);
                    }
                }
            }
            PickerLoad::ResumeRows { generation, rows } => {
                let Some(picker) = self
                    .picker
                    .as_mut()
                    .filter(|picker| picker.generation == generation)
                else {
                    return;
                };
                for (index, entry) in rows {
                    if let Some(row) = picker.entries.get_mut(index) {
                        row.details = Some(entry);
                    }
                }
            }
            PickerLoad::TreeReady {
                generation,
                entries,
                snapshot_id,
            } => {
                let Some(picker) = self
                    .tree_picker
                    .as_mut()
                    .filter(|picker| picker.generation == generation)
                else {
                    // Picker was closed before the snapshot was ready. Tell
                    // the worker to drop the snapshot now so it doesn't leak
                    // until process exit.
                    if let Some(sink) = self.session.sink.as_ref() {
                        sink.submit_io(Box::new(move |state| {
                            state.remove_tree_snapshot(snapshot_id);
                        }));
                    }
                    return;
                };
                if entries.is_empty() {
                    self.tree_picker = None;
                    self.notify(NotifyKind::Info, "no branch points in this session yet");
                    if let Some(sink) = self.session.sink.as_ref() {
                        sink.submit_io(Box::new(move |state| {
                            state.remove_tree_snapshot(snapshot_id);
                        }));
                    }
                } else {
                    picker.selected = entries.len().saturating_sub(1);
                    picker.entries = entries;
                    picker.loading = false;
                    self.tree_picker_snapshot = Some(snapshot_id);
                    self.tree_picker_pending.clear();
                    self.tree_picker_pending
                        .extend(picker.entries.len().saturating_sub(20)..picker.entries.len());
                }
            }
            PickerLoad::TreeRows { generation, rows } => {
                let Some(picker) = self
                    .tree_picker
                    .as_mut()
                    .filter(|picker| picker.generation == generation)
                else {
                    return;
                };
                for (index, mut entry) in rows {
                    self.tree_picker_pending.remove(&index);
                    entry.prefill.clear();
                    if let Some(row) = picker.entries.get_mut(index) {
                        *row = entry;
                    }
                }
            }
            PickerLoad::TreeFailed { generation, error } => {
                if self
                    .tree_picker
                    .as_ref()
                    .is_some_and(|picker| picker.generation == generation)
                {
                    self.tree_picker = None;
                    self.notify(
                        NotifyKind::Error,
                        format!("load session for /tree: {error}"),
                    );
                }
            }
        }
    }

    pub(super) fn picker_confirm_inner(&mut self, picker: PickerState) {
        self.picker_generation.fetch_add(1, Ordering::Relaxed);
        let entry = picker.entries.into_iter().nth(picker.selected);
        let Some(entry) = entry else {
            return;
        };
        match entry.file.open_snapshot() {
            Ok((cursor, snapshot)) => {
                let index = snapshot.index;
                let file_size = snapshot.file_size;
                let loaded = self.restore_indexed_session(&cursor, &index, file_size);
                if let Err(e) = loaded {
                    self.push_turn(Turn {
                        prompt: "/resume".to_string(),
                        kind: lofi_types::PromptKind::User,
                        blocks: vec![Block::Error(format!("load session: {e}"))],
                    });
                    return;
                }
                self.reset_session_jobs();
                if self.turns.len() > 1 {
                    let n = self.turns.len();
                    for turn in &mut self.turns[..n - 1] {
                        turn.blocks.clear();
                    }
                }
                if !self.model_choices.is_empty() {
                    if let Some(m) = last_run_model_from_index(&cursor, &index) {
                        let restored = format!("{}/{}:{}", m.provider, m.id, m.thinking.as_str());
                        let current = format!("{}:{}", self.model_label, self.thinking.as_str());
                        if restored != current {
                            self.pending_model_switch = Some(restored);
                        }
                    }
                }
                self.bump_render_epoch();
                self.session.attach_cursor(cursor);
                self.pinned = true;
                self.top_line = 0;
            }
            Err(e) => self.push_turn(Turn {
                prompt: "/resume".to_string(),
                kind: lofi_types::PromptKind::User,
                blocks: vec![Block::Error(format!("load session: {e}"))],
            }),
        }
    }

    #[cfg(test)]
    pub(super) fn resume_model_switch(&self, events: &[SessionEvent]) -> Option<String> {
        if self.model_choices.is_empty() {
            return None;
        }
        let m = store::last_run_model(events)?;
        let restored = format!("{}/{}:{}", m.provider, m.id, m.thinking.as_str());
        let current = format!("{}:{}", self.model_label, self.thinking.as_str());
        (restored != current).then_some(restored)
    }

    /// `/model`: open the model-picker overlay populated from the retained
    /// `model_choices`. The current model is pre-selected so the user sees
    /// where they are. No-op (with a notice) when no model is available.
    pub(super) fn open_model_picker(&mut self) {
        if self.model_choices.is_empty() {
            self.notify(
                NotifyKind::Warn,
                "no models available (set a provider API key)",
            );
            return;
        }
        let selected = self
            .model_choices
            .iter()
            .position(|c| format!("{}/{}", c.provider, c.id) == self.model_label)
            .unwrap_or(0);
        self.model_picker = Some(ModelPickerState {
            choices: self.model_choices.clone(),
            selected,
        });
    }

    pub(super) fn model_picker_confirm(&mut self) {
        if let Some(picker) = self.model_picker.take() {
            if let Some(choice) = picker.choices.get(picker.selected) {
                self.pending_model_switch = Some(format!("{}/{}", choice.provider, choice.id));
            }
        }
    }

    /// `/job`: open the background-jobs modal. Lists every session job with
    /// live status; `Enter` drills into a job's output, `x` arms a kill
    /// confirm.
    /// No-op (with a notice) when no agent is configured.
    pub(super) fn open_jobs_modal(&mut self) {
        if self.jobs.is_none() {
            self.notify(NotifyKind::Info, "no background jobs (no active session)");
            return;
        }
        self.jobs_modal = Some(JobsModalState {
            selected: 0,
            viewing: None,
            confirm_kill: None,
        });
    }

    /// Fresh job snapshot for the modal, newest first. Empty when no agent.
    pub(super) fn jobs_snapshot(&self) -> Vec<lofi_core::JobInfo> {
        self.jobs
            .as_ref()
            .map_or_else(Vec::new, lofi_core::JobRegistry::snapshot)
    }

    /// Refresh the open output view. PTY jobs copy their latest parsed screen;
    /// plain jobs page a tail window capped at `JOB_LOG_WINDOW_BYTES`.
    pub(super) fn refresh_job_output(&mut self) {
        let (Some(jobs), Some(modal)) = (&self.jobs, &mut self.jobs_modal) else {
            return;
        };
        let Some(view) = &mut modal.viewing else {
            return;
        };
        match &mut view.content {
            JobViewContent::Terminal(screen) => {
                if let Some(current) = jobs.screen(view.id) {
                    *screen = current;
                }
            }
            JobViewContent::Log {
                lines,
                cursor,
                total,
                ..
            } => {
                let Some((bytes, next, new_total)) = jobs.read_log(view.id, *cursor, 64 * 1024)
                else {
                    return;
                };
                if next == *cursor {
                    return;
                }
                *cursor = next;
                *total = new_total;
                let text = String::from_utf8_lossy(&bytes);
                lines.extend(text.lines().map(str::to_owned));
                let mut held: usize = lines.iter().map(|line| line.len() + 1).sum();
                while held > JOB_LOG_WINDOW_BYTES {
                    let Some(front) = lines.pop_front() else {
                        break;
                    };
                    held -= front.len() + 1;
                }
            }
        }
    }

    /// Keys for the `/job` modal. Two levels: the job list, and the drill-in
    /// output view. `x` on a running job arms a kill confirm (`y` confirms,
    /// anything else cancels). Always returns true while the modal is open.
    fn handle_jobs_key(&mut self, k: &KeyEvent) -> bool {
        // Take the modal out so the body can call `&self`/`&mut self` helpers
        // (snapshot, refresh) without overlapping the modal borrow; put it
        // back (or not, on close) before returning.
        let Some(mut modal) = self.jobs_modal.take() else {
            return true;
        };

        // A pending kill confirm consumes the next key regardless of level.
        if let Some(id) = modal.confirm_kill.take() {
            if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                if let Some(jobs) = &self.jobs {
                    jobs.kill(id);
                }
            }
            self.jobs_modal = Some(modal);
            return true;
        }

        // Drill-in output view.
        if let Some(view) = &mut modal.viewing {
            if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                modal.viewing = None;
            } else if let JobViewContent::Log { lines, scroll, .. } = &mut view.content {
                let max_scroll = lines.len().saturating_sub(1);
                match k.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        *scroll = (*scroll + 1).min(max_scroll);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_sub(1);
                    }
                    KeyCode::Char('g') => *scroll = max_scroll,
                    KeyCode::Char('G') => *scroll = 0,
                    _ => {}
                }
            }
            self.jobs_modal = Some(modal);
            return true;
        }

        // Job list.
        let snapshot = self.jobs_snapshot();
        let len = snapshot.len();
        let selected_id = snapshot.get(modal.selected).map(|j| j.id);
        let selected_running = snapshot.get(modal.selected).is_some_and(|j| j.running);
        let mut open_log = false;
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                // Closed: do not restore the modal.
                return true;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                modal.selected = modal.selected.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if len > 0 {
                    modal.selected = (modal.selected + 1).min(len - 1);
                }
            }
            KeyCode::Enter => {
                if let Some(id) = selected_id {
                    let content = if snapshot[modal.selected].tty {
                        self.jobs
                            .as_ref()
                            .and_then(|jobs| jobs.screen(id))
                            .map_or_else(JobViewContent::empty_log, JobViewContent::Terminal)
                    } else {
                        JobViewContent::empty_log()
                    };
                    modal.viewing = Some(JobOutputView { id, content });
                    open_log = true;
                }
            }
            KeyCode::Char('x') if selected_running => {
                modal.confirm_kill = selected_id;
            }
            _ => {}
        }
        self.jobs_modal = Some(modal);
        if open_log {
            self.refresh_job_output();
        }
        true
    }
    /// `/thinking`: open the thinking-level picker for the current model.
    /// Offers `off` plus the model's declared `thinking_levels` (deduped),
    /// pre-selected at the current level. Shows a notice instead of opening
    /// when the current model supports no thinking levels (only `off`).
    pub(super) fn open_thinking_picker(&mut self) {
        let levels = self.current_thinking_choices();
        if levels.len() <= 1 {
            self.notify(
                NotifyKind::Info,
                "current model does not support thinking levels",
            );
            return;
        }
        let selected = levels.iter().position(|l| l == &self.thinking).unwrap_or(0);
        self.thinking_picker = Some(ThinkingPickerState { levels, selected });
    }

    fn current_thinking_choices(&self) -> Vec<ThinkingLevel> {
        let mut out = vec![ThinkingLevel::Off];
        if let Some(c) = self
            .model_choices
            .iter()
            .find(|c| format!("{}/{}", c.provider, c.id) == self.model_label)
        {
            for level in &c.thinking_levels {
                if level != &ThinkingLevel::Off && !out.contains(level) {
                    out.push(level.clone());
                }
            }
        }
        out
    }

    /// `/service`: open the service-tier picker for the current model.
    /// Offers `auto` plus the model's declared `service_tiers` (deduped),
    /// pre-selected at the current tier. Shows a notice instead of opening
    /// when the current model declares no service tiers beyond `auto`.
    pub(super) fn open_service_picker(&mut self) {
        let tiers = self.current_service_choices();
        if tiers.len() <= 1 {
            self.notify(
                NotifyKind::Info,
                "current model does not declare service tiers",
            );
            return;
        }
        let selected = tiers
            .iter()
            .position(|t| t == &self.service_tier)
            .unwrap_or(0);
        self.service_picker = Some(ServicePickerState { tiers, selected });
    }

    fn current_service_choices(&self) -> Vec<ServiceTier> {
        let mut out = vec![ServiceTier::Auto];
        if let Some(c) = self
            .model_choices
            .iter()
            .find(|c| format!("{}/{}", c.provider, c.id) == self.model_label)
        {
            for tier in &c.service_tiers {
                if tier != &ServiceTier::Auto && !out.contains(tier) {
                    out.push(tier.clone());
                }
            }
        }
        out
    }

    pub(super) fn service_picker_confirm(&mut self) {
        if let Some(picker) = self.service_picker.take() {
            if let Some(tier) = picker.tiers.get(picker.selected) {
                let tier_suffix = if tier == &ServiceTier::Auto {
                    String::new()
                } else {
                    format!("@{}", tier.as_str())
                };
                self.pending_model_switch = Some(format!(
                    "{}:{}{}",
                    self.model_label,
                    self.thinking.as_str(),
                    tier_suffix
                ));
            }
        }
    }

    /// `/theme`: open the color-scheme picker (Auto / Light / Dark),
    /// pre-selected on the currently active mode.
    pub(super) fn open_theme_picker(&mut self) {
        let modes = ThemePickerState::MODES;
        let selected = modes
            .iter()
            .position(|m| m == &self.theme_mode)
            .unwrap_or(0);
        self.theme_picker = Some(ThemePickerState { modes, selected });
    }

    pub(super) fn theme_picker_confirm(&mut self) {
        if let Some(picker) = self.theme_picker.take() {
            if let Some(&mode) = picker.modes.get(picker.selected) {
                self.theme_mode = mode;
                self.sync_color_scheme_reports();
                match mode {
                    lofi_types::ThemeMode::Auto => {
                        crate::tui::tty_events::request_color_scheme();
                    }
                    lofi_types::ThemeMode::Light => {
                        self.apply_resolved_theme(Theme::light());
                    }
                    lofi_types::ThemeMode::Dark => {
                        self.apply_resolved_theme(Theme::dark());
                    }
                }
            }
        }
    }

    pub(super) fn apply_color_scheme(
        &mut self,
        scheme: crate::tui::tty_events::ColorScheme,
    ) -> bool {
        if self.theme_mode != lofi_types::ThemeMode::Auto {
            return false;
        }
        self.apply_resolved_theme(Theme::from_scheme(scheme))
    }

    pub(super) fn sync_color_scheme_reports(&self) {
        crate::tui::tty_events::set_reports_enabled(self.theme_mode == lofi_types::ThemeMode::Auto);
    }

    pub(super) fn apply_resolved_theme(&mut self, next: Theme) -> bool {
        if self.theme == next {
            return false;
        }
        self.theme = next;
        self.frozen_render.clear();
        true
    }

    pub(super) fn thinking_picker_confirm(&mut self) {
        if let Some(picker) = self.thinking_picker.take() {
            if let Some(level) = picker.levels.get(picker.selected) {
                self.pending_model_switch =
                    Some(format!("{}:{}", self.model_label, level.as_str()));
            }
        }
    }

    pub(super) fn apply_model_switch(
        &mut self,
        model: &lofi_types::Model,
        level: ThinkingLevel,
        tier: ServiceTier,
    ) {
        self.model_label = format!("{}/{}", model.provider, model.id);
        self.thinking_label = (level != ThinkingLevel::Off).then(|| format!(":{}", level.as_str()));
        self.thinking = level;
        self.service_label = (tier != ServiceTier::Auto).then(|| format!("@{}", tier.as_str()));
        self.service_tier = tier;
        self.ctx_limit = model
            .context_window
            .filter(|&l| l > 0)
            .unwrap_or(DEFAULT_CTX_LIMIT);
        self.lifecycle.set_context_window(self.ctx_limit);
        self.notify(
            NotifyKind::Info,
            format!(
                "switched to {}/{}{}{}",
                model.provider,
                model.id,
                self.thinking_label.as_deref().unwrap_or(""),
                self.service_label.as_deref().unwrap_or("")
            ),
        );
        self.bump_render_epoch();
    }

    /// Drop the IO-thread-held tree snapshot, if any. Sends the release to
    /// the same worker that built the snapshot so the multi-MB
    /// `Vec<EventIndex>` is freed on the thread that allocated it; glibc can
    /// then reuse the pages for the next IO job instead of leaving them in
    /// the worker's arena because the free ran on the main thread.
    fn release_tree_snapshot(&mut self) {
        let (Some(id), Some(sink)) = (self.tree_picker_snapshot.take(), self.session.sink.as_ref())
        else {
            return;
        };
        sink.submit_io(Box::new(move |state| {
            state.remove_tree_snapshot(id);
        }));
    }

    /// '/tree': open the branch-picker overlay over the active session's
    /// event log. Lists every user-prompt event (the natural branch points)
    /// with its preview. Confirmed entry feeds the prompt text back into the
    /// input (for editing) and moves the shared cursor so the next run starts
    /// as a sibling of that prompt rather than appending to the active leaf.
    pub(super) fn open_tree_picker(&mut self) {
        let Some(cursor) = self.session.cursor.clone() else {
            self.notify(
                NotifyKind::Error,
                "no session file (ephemeral or --no-session)",
            );
            return;
        };
        let generation = self
            .picker_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        // Release any prior tree snapshot on the IO thread before opening a
        // new one; otherwise a stale snapshot would leak until process exit.
        self.release_tree_snapshot();
        self.tree_picker_pending.clear();
        self.tree_picker = Some(TreePickerState {
            entries: Vec::new(),
            selected: 0,
            generation,
            loading: true,
        });
        if let (Some(tx), Some(sink)) = (self.picker_load_tx.clone(), self.session.sink.as_ref()) {
            let generation_clock = Arc::clone(&self.picker_generation);
            let cursor = cursor.clone();
            sink.submit_io(Box::new(move |state| {
                run_tree_snapshot(generation, &generation_clock, &cursor, &tx, state);
            }));
        } else {
            match cursor.tree_snapshot() {
                Ok(snapshot) => {
                    let entries =
                        build_tree_entries(&snapshot.index, snapshot.leaf_id.as_deref(), &cursor);
                    if entries.is_empty() {
                        self.tree_picker = None;
                        self.notify(NotifyKind::Info, "no branch points in this session yet");
                    } else {
                        let selected = entries.len().saturating_sub(1);
                        self.tree_picker = Some(TreePickerState {
                            entries,
                            selected,
                            generation,
                            loading: false,
                        });
                    }
                }
                Err(e) => {
                    self.tree_picker = None;
                    self.notify(NotifyKind::Error, format!("load session for /tree: {e}"));
                }
            }
        }
    }

    fn request_tree_viewport(&mut self) {
        const VIEW_ROWS: usize = 20;
        let Some(picker) = self.tree_picker.as_ref() else {
            return;
        };
        let Some(snapshot_id) = self.tree_picker_snapshot else {
            return;
        };
        let Some(tx) = self.picker_load_tx.clone() else {
            return;
        };
        let generation = picker.generation;
        let start = picker
            .selected
            .saturating_sub(VIEW_ROWS.saturating_sub(1))
            .min(picker.entries.len().saturating_sub(VIEW_ROWS));
        let end = (start + VIEW_ROWS).min(picker.entries.len());
        // Include one page before the visible viewport. Since the tree starts
        // at its tail and users normally move upward, this amortizes topology
        // setup to roughly once per 20 rows instead of once per keypress.
        let prefetch_start = start.saturating_sub(VIEW_ROWS);
        let requested: Vec<_> = (prefetch_start..end)
            .filter(|row| !picker.entries[*row].hydrated && !self.tree_picker_pending.contains(row))
            .map(|row| (row, picker.entries[row].clone()))
            .collect();
        if requested.is_empty() {
            return;
        }
        self.tree_picker_pending
            .extend(requested.iter().map(|(row, _)| *row));
        let Some(cursor) = self.session.cursor.clone() else {
            return;
        };
        let Some(sink) = self.session.sink.as_ref() else {
            return;
        };
        let generation_clock = Arc::clone(&self.picker_generation);
        sink.submit_io(Box::new(move |state| {
            let Some(snapshot) = state.tree_snapshot(snapshot_id) else {
                // Picker was closed between request and execution; nothing to
                // hydrate. The pending-set entry will be cleared on close.
                return;
            };
            hydrate_tree_entry_rows(
                &snapshot.index,
                &cursor,
                &requested,
                |rows| tx.send(PickerLoad::TreeRows { generation, rows }).is_ok(),
                || generation_clock.load(Ordering::Relaxed) != generation,
            );
        }));
    }

    fn present_lineage_job_reconciliation(
        &mut self,
        reconciliation: &lofi_core::LineageJobReconciliation,
    ) {
        if !reconciliation.killed.is_empty() {
            let ids = reconciliation
                .killed
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            self.notify(
                NotifyKind::Info,
                format!(
                    "killed {} off-lineage job(s): {ids}",
                    reconciliation.killed.len()
                ),
            );
        }
        if !reconciliation.stale.is_empty() {
            let ids = reconciliation
                .stale
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            self.prompt_queue.push(super::QueuedPrompt {
                text: format!(
                    "branch switch: jobs [{ids}] from the prior lineage are no longer running; their ids are stale. Use jobSpawn for new background work."
                ),
                kind: lofi_types::PromptKind::Notice,
            });
        }
    }

    pub(super) fn tree_picker_confirm_inner(&mut self, picker: &TreePickerState) {
        self.picker_generation.fetch_add(1, Ordering::Relaxed);
        self.release_tree_snapshot();
        self.tree_picker_pending.clear();
        // See Escape path for why we trim after dropping picker state.
        lofi_core::malloc_trim::release_freed_memory();
        let Some(entry) = picker.entries.get(picker.selected).cloned() else {
            return;
        };
        if !entry.hydrated {
            self.notify(NotifyKind::Info, "selected tree row is still loading");
            return;
        }
        // The branch-head move is a core-owned session write. The sink moves
        // the head and returns the new lineage snapshot in one call; the UI
        // then mirrors the cursor for reads and rolls the transcript back.
        let Some(cursor) = self.session.cursor.clone() else {
            return;
        };
        let old_leaf = cursor.leaf_id();
        // Move the head (core-owned) and grab the snapshot in a short borrow
        // scope so the mutable UI work below doesn't overlap the sink borrow.
        let snapshot = {
            let Some(sink) = self.session.sink.as_ref() else {
                return;
            };
            match sink.switch_branch(entry.branch_point.clone()) {
                Ok(snapshot) => snapshot,
                Err(e) => {
                    self.notify(NotifyKind::Error, format!("persist session cursor: {e}"));
                    return;
                }
            }
        };
        self.session.refresh_cursor();
        let loaded = self.rollback_indexed(&cursor, &snapshot.index, snapshot.file_size);
        if let Err(e) = loaded {
            // Undo the head move (core-owned), then re-roll to the old lineage.
            let restored_snapshot = {
                self.session
                    .sink
                    .as_ref()
                    .and_then(|sink| sink.restore_branch(old_leaf.clone()).ok())
            };
            self.session.refresh_cursor();
            let restored = restored_snapshot.map(|snapshot| {
                self.rollback_indexed(&cursor, &snapshot.index, snapshot.file_size)
                    .ok()
            });
            let suffix = if restored.is_some() {
                String::new()
            } else {
                "; restore failed".to_string()
            };
            self.notify(
                NotifyKind::Error,
                format!("load session for /tree: {e}{suffix}"),
            );
            return;
        }
        if let Some(jobs) = self.jobs.as_ref() {
            match self
                .lifecycle
                .reconcile_jobs_after_lineage_switch(&cursor, &snapshot.index, jobs)
            {
                Ok(reconciliation) => self.present_lineage_job_reconciliation(&reconciliation),
                Err(error) => {
                    self.notify(NotifyKind::Error, format!("reconcile branch jobs: {error}"));
                }
            }
        }
        let prefill = if !entry.prefill.is_empty() {
            entry.prefill
        } else if entry.source_kind == store::IndexKind::UserPrompt {
            load_prompt_text(&cursor, entry.source_offset)
        } else {
            String::new()
        };
        if !prefill.is_empty() {
            self.input = prefill;
            self.input_cursor = self.input.len();
        }
    }

    pub(super) fn queue_confirmation(&mut self, req: lofi_core::ConfirmRequest) {
        if Self::confirmation_ready(&req) {
            self.pending_confirms.push(req);
        } else {
            self.deferred_confirms.push(req);
        }
    }

    /// Retire completed requests and promote auto-mode evaluations once they
    /// need user attention. The grace period is a presentation policy: core
    /// and tool layers emit the evaluating request immediately.
    pub(super) fn refresh_confirmations(&mut self) -> bool {
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
    pub(super) fn modal_open(&self) -> bool {
        self.info.is_some()
            || self.picker.is_some()
            || self.tree_picker.is_some()
            || self.model_picker.is_some()
            || self.thinking_picker.is_some()
            || self.service_picker.is_some()
            || self.theme_picker.is_some()
            || self.jobs_modal.is_some()
            || !self.pending_confirms.is_empty()
    }

    /// Dispatch a key to the topmost visible popup. This order is the reverse
    /// of the popup render order in `view::render`.
    pub(super) fn handle_modal_stack_key(&mut self, k: &KeyEvent) -> bool {
        if !self.pending_confirms.is_empty() {
            return self.handle_confirm_key(k);
        }
        if self.info.is_some() {
            return self.handle_info_key(k);
        }
        if self.jobs_modal.is_some() || self.active_modal_slot().is_some() {
            return self.handle_modal_key(k);
        }
        self.handle_popover_key(k)
    }

    pub(super) fn handle_confirm_key(&mut self, k: &KeyEvent) -> bool {
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

    fn active_modal_slot(&self) -> Option<ModalSlot> {
        if self.theme_picker.is_some() {
            Some(ModalSlot::Theme)
        } else if self.service_picker.is_some() {
            Some(ModalSlot::Service)
        } else if self.thinking_picker.is_some() {
            Some(ModalSlot::Thinking)
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
    pub(super) fn handle_modal_key(&mut self, k: &KeyEvent) -> bool {
        if self.jobs_modal.is_some() {
            return self.handle_jobs_key(k);
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
                        self.tree_picker_confirm_inner(&picker);
                    }
                }
                ModalSlot::Model => self.model_picker_confirm(),
                ModalSlot::Thinking => self.thinking_picker_confirm(),
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
                        self.tree_picker_confirm_inner(&picker);
                    }
                }
                ModalSlot::Model => self.model_picker_confirm(),
                ModalSlot::Thinking => self.thinking_picker_confirm(),
                ModalSlot::Service => self.service_picker_confirm(),
                ModalSlot::Theme => self.theme_picker_confirm(),
            },
            KeyCode::Esc | KeyCode::Char('q') => match slot {
                ModalSlot::Picker => {
                    self.picker = None;
                    self.picker_generation.fetch_add(1, Ordering::Relaxed);
                }
                ModalSlot::Tree => {
                    self.tree_picker = None;
                    self.release_tree_snapshot();
                    self.tree_picker_pending.clear();
                    self.picker_generation.fetch_add(1, Ordering::Relaxed);
                    // The picker entries Vec buffer was cloned on the IO
                    // thread before being sent here; dropping it on this
                    // thread leaves the freed pages stranded in this arena.
                    // Trim after drop so RSS returns to the pre-/tree level.
                    lofi_core::malloc_trim::release_freed_memory();
                }
                ModalSlot::Model => self.model_picker = None,
                ModalSlot::Thinking => self.thinking_picker = None,
                ModalSlot::Service => self.service_picker = None,
                ModalSlot::Theme => self.theme_picker = None,
            },
            _ => {}
        }
        if (matches!(slot, ModalSlot::Picker) && self.picker.is_none())
            || (matches!(slot, ModalSlot::Tree) && self.tree_picker.is_none())
            || (matches!(slot, ModalSlot::Model) && self.model_picker.is_none())
            || (matches!(slot, ModalSlot::Thinking) && self.thinking_picker.is_none())
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

    pub(super) fn active_modal_mut(&mut self) -> Option<&mut dyn Modal> {
        if self.theme_picker.is_some() {
            self.theme_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else if self.service_picker.is_some() {
            self.service_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else if self.thinking_picker.is_some() {
            self.thinking_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else if self.model_picker.is_some() {
            self.model_picker.as_mut().map(|m| m as &mut dyn Modal)
        } else if self.tree_picker.is_some() {
            self.tree_picker.as_mut().map(|t| t as &mut dyn Modal)
        } else {
            self.picker.as_mut().map(|p| p as &mut dyn Modal)
        }
    }

    pub(super) fn handle_popover_key(&mut self, k: &KeyEvent) -> bool {
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

    #[cfg(test)]
    pub(super) fn tree_picker_confirm(&mut self) {
        if let Some(picker) = self.tree_picker.take() {
            self.tree_picker_confirm_inner(&picker);
        }
    }

    fn rollback_indexed(
        &mut self,
        cursor: &store::SessionCursor,
        index: &[store::EventIndex],
        file_size: u64,
    ) -> Result<()> {
        self.restore_indexed_session(cursor, index, file_size)?;
        // Rollback leaves no live run. Unlike resume, even the selected final
        // turn is immutable and file-backed, so retaining its potentially huge
        // blocks would recreate the RSS spike this path is meant to prevent.
        for turn in &mut self.turns {
            turn.blocks.clear();
        }
        self.bump_render_epoch();
        self.pinned = true;
        self.top_line = 0;
        Ok(())
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
