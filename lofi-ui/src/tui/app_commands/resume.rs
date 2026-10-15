use super::*;

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
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by_key(|&index| files[index].byte_len());
    for index in order {
        if generation_clock.load(Ordering::Relaxed) != generation {
            return;
        }
        let Some(entry) = files[index].inspect() else {
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

impl App {
    pub(in crate::tui) fn open_picker(&mut self) {
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

    pub(in crate::tui) fn apply_picker_load(&mut self, load: PickerLoad) {
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
                    // Picker was closed before the snapshot was ready. Move
                    // its rows back to the allocating worker so both the rows
                    // and snapshot are dropped before that worker trims.
                    if let Some(sink) = self.session.sink.as_ref() {
                        sink.submit_io(Box::new(move |state| {
                            drop(entries);
                            state.remove_tree_snapshot(snapshot_id);
                        }));
                    } else {
                        drop(entries);
                        lofi_core::release_freed_memory();
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
                    self.release_tree_rows(rows);
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

    pub(in crate::tui) fn picker_confirm_inner(&mut self, picker: PickerState) {
        self.picker_generation.fetch_add(1, Ordering::Relaxed);
        // Switching under an active run would book its records into the
        // wrong transcript.
        if self.busy() {
            self.notify(
                NotifyKind::Warn,
                "wait for the running turn to finish before switching sessions",
            );
            return;
        }
        let entry = picker.entries.into_iter().nth(picker.selected);
        let Some(entry) = entry else {
            return;
        };
        // Record into the still-open session before the new cursor exists.
        // After open_snapshot the leaf would move on, and a late record
        // through the old cursor would fork off the visible path. The
        // registry clear waits for the success path: on every failure path
        // the old session stays open and keeps its subagents.
        self.record_subagent_spend();
        match entry.file.open_snapshot() {
            Ok((cursor, snapshot)) => {
                match lofi_core::backend::saved_backend(&cursor) {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        self.notify(
                            NotifyKind::Warn,
                            "resume external backend sessions from the CLI",
                        );
                        return;
                    }
                    Err(error) => {
                        self.notify(NotifyKind::Error, error.to_string());
                        return;
                    }
                }
                let index = snapshot.index;
                let file_size = snapshot.file_size;
                let loaded = self.restore_indexed_session(
                    &cursor,
                    &index,
                    file_size,
                    snapshot.history_start,
                    snapshot.contiguous,
                );
                if let Err(e) = loaded {
                    self.transcript.push_turn(Turn {
                        prompt: "/resume".to_string(),
                        kind: lofi_types::PromptKind::User,
                        blocks: vec![Block::Error(format!("load session: {e}"))],
                    });
                    return;
                }
                self.reset_session_jobs();
                self.clear_subagents();
                if self.transcript.turns.len() > 1 {
                    let n = self.transcript.turns.len();
                    for turn in &mut self.transcript.turns[..n - 1] {
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
            Err(e) => self.transcript.push_turn(Turn {
                prompt: "/resume".to_string(),
                kind: lofi_types::PromptKind::User,
                blocks: vec![Block::Error(format!("load session: {e}"))],
            }),
        }
    }

    #[cfg(test)]
    pub(in crate::tui) fn resume_model_switch(&self, events: &[SessionEvent]) -> Option<String> {
        if self.model_choices.is_empty() {
            return None;
        }
        let m = store::last_run_model(events)?;
        let restored = format!("{}/{}:{}", m.provider, m.id, m.thinking.as_str());
        let current = format!("{}:{}", self.model_label, self.thinking.as_str());
        (restored != current).then_some(restored)
    }
}
