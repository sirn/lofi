use super::*;

fn run_tree_snapshot(
    generation: u64,
    generation_clock: &AtomicU64,
    cursor: &store::SessionCursor,
    tx: &tokio::sync::mpsc::UnboundedSender<PickerLoad>,
    state: &mut lofi_core::session::io::WorkerState,
) {
    match cursor.tree_snapshot() {
        Ok(snapshot) => {
            // Pin the snapshot before the first cancellation check. Every
            // return path can then release it through the same worker-owned
            // drop-and-trim operation.
            let snapshot_id = state.insert_tree_snapshot(snapshot);
            if generation_clock.load(Ordering::Relaxed) != generation {
                state.remove_tree_snapshot(snapshot_id);
                return;
            }
            let Some(snapshot) = state.tree_snapshot(snapshot_id) else {
                return;
            };
            let skeletons =
                build_tree_entry_skeletons(&snapshot.index, snapshot.leaf_id.as_deref(), cursor);
            if generation_clock.load(Ordering::Relaxed) != generation {
                drop(skeletons);
                state.remove_tree_snapshot(snapshot_id);
                return;
            }
            // The UI gets only an id. All subsequent hydration looks the
            // snapshot up by id, and picker close drops it here so its whole
            // lifecycle stays on the allocating worker.
            let start = skeletons.len().saturating_sub(20);
            let requested: Vec<_> = skeletons[start..]
                .iter()
                .cloned()
                .enumerate()
                .map(|(offset, row)| (start + offset, row))
                .collect();
            if tx
                .send(PickerLoad::TreeReady {
                    generation,
                    entries: skeletons,
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
            hydrate_tree_entry_rows(
                &snapshot.index,
                cursor,
                &requested,
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
    /// Return tree allocations to the IO worker that created them. The
    /// snapshot index and picker rows can each use multiple MiB. Dropping both
    /// on that worker before trimming lets glibc release its arena pages.
    pub(super) fn release_tree_rows(&self, rows: Vec<(usize, TreeEntry)>) {
        if let Some(sink) = self.session.sink.as_ref() {
            sink.submit_io(Box::new(move |_state| {
                drop(rows);
                lofi_core::release_freed_memory();
            }));
        } else {
            drop(rows);
            lofi_core::release_freed_memory();
        }
    }

    pub(super) fn release_tree_resources(&mut self, entries: Vec<TreeEntry>) {
        let snapshot_id = self.tree_picker_snapshot.take();
        if entries.is_empty() && snapshot_id.is_none() {
            return;
        }
        if let Some(sink) = self.session.sink.as_ref() {
            sink.submit_io(Box::new(move |state| {
                drop(entries);
                if let Some(id) = snapshot_id {
                    state.remove_tree_snapshot(id);
                } else {
                    lofi_core::release_freed_memory();
                }
            }));
        } else {
            drop(entries);
            lofi_core::release_freed_memory();
        }
    }

    fn release_tree_snapshot(&mut self) {
        self.release_tree_resources(Vec::new());
    }

    /// '/tree': open the branch-picker overlay over the active session's
    /// event log. Lists every user-prompt event (the natural branch points)
    /// with its preview. Confirmed entry feeds the prompt text back into the
    /// input (for editing) and moves the shared cursor so the next run starts
    /// as a sibling of that prompt rather than appending to the active leaf.
    pub(in crate::tui) fn open_tree_picker(&mut self) {
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

    pub(super) fn request_tree_viewport(&mut self) {
        const VIEW_ROWS: usize = 20;
        let Some(picker) = self.tree_picker.as_ref() else {
            return;
        };
        let Some(snapshot_id) = self.tree_picker_snapshot else {
            return;
        };
        if !self.tree_picker_pending.is_empty() {
            return;
        }
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

    pub(in crate::tui) fn tree_picker_confirm_inner(&mut self, picker: TreePickerState) {
        let entry = picker.entries.get(picker.selected).cloned();
        self.picker_generation.fetch_add(1, Ordering::Relaxed);
        self.release_tree_resources(picker.entries);
        self.tree_picker_pending.clear();
        if self.busy() {
            self.notify(
                NotifyKind::Warn,
                "wait for the running turn to finish before rolling back",
            );
            return;
        }
        let Some(entry) = entry else {
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
        let loaded = self.rollback_indexed(
            &cursor,
            &snapshot.index,
            snapshot.file_size,
            snapshot.history_start,
            snapshot.contiguous,
        );
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
                self.rollback_indexed(
                    &cursor,
                    &snapshot.index,
                    snapshot.file_size,
                    snapshot.history_start,
                    snapshot.contiguous,
                )
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

    #[cfg(test)]
    pub(in crate::tui) fn tree_picker_confirm(&mut self) {
        if let Some(picker) = self.tree_picker.take() {
            self.tree_picker_confirm_inner(picker);
        }
    }

    fn rollback_indexed(
        &mut self,
        cursor: &store::SessionCursor,
        index: &[store::SessionIndexEntry],
        file_size: u64,
        history_start: usize,
        contiguous: bool,
    ) -> Result<()> {
        self.restore_indexed_session(cursor, index, file_size, history_start, contiguous)?;
        // Rollback leaves no live run. Unlike resume, even the selected final
        // turn is immutable and file-backed, so retaining its potentially huge
        // blocks would recreate the RSS spike this path is meant to prevent.
        for turn in &mut self.transcript.turns {
            turn.blocks.clear();
        }
        self.bump_render_epoch();
        self.pinned = true;
        self.top_line = 0;
        Ok(())
    }
}
