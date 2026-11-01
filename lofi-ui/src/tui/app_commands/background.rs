use super::*;

impl App {
    /// Open the background dialog on `tab`. Every way in lands here: the
    /// `/job` and `/subagent` commands pick their tab, and ctrl+b picks the
    /// tab of the most recent work.
    pub(in crate::tui) fn open_background_modal(&mut self, tab: BackgroundTab) {
        let mut subagents = SubagentsModalState {
            selected: 0,
            rows: Vec::new(),
        };
        self.reload_subagent_rows(&mut subagents);
        let timers = TimersModalState {
            selected: 0,
            rows: self.timers_snapshot(),
        };
        self.background_modal = Some(BackgroundModalState {
            tab,
            jobs: JobsModalState { selected: 0 },
            subagents,
            timers,
            view: None,
            confirm: None,
        });
    }

    /// The tab a bare keybinding should land on: the one whose youngest
    /// running work started last, so ctrl+b lands beside what the user just
    /// set going. A timer only waits, so it never takes this key.
    pub(in crate::tui) fn newest_background_tab(&self) -> BackgroundTab {
        let job = self
            .running_jobs()
            .iter()
            .filter(|job| job.running)
            .map(|job| job.duration_ms)
            .min();
        let subagent = self
            .running_subagents()
            .iter()
            .map(|run| run.elapsed_ms)
            .min();
        match (job, subagent) {
            (Some(job), Some(subagent)) => {
                if subagent < job {
                    BackgroundTab::Subagents
                } else {
                    BackgroundTab::Jobs
                }
            }
            (Some(_), None) => BackgroundTab::Jobs,
            (None, _) => BackgroundTab::Subagents,
        }
    }

    /// Keep the open dialog current: the drill-in reloads, the rows take the
    /// live registry fields, and a listing the frame re-reads (jobs, timers)
    /// also clamps the selection it may have shrunk under.
    pub(in crate::tui) fn refresh_background_modal(&mut self) {
        let Some(modal) = self.background_modal.as_ref() else {
            return;
        };
        let tab = modal.tab;
        let viewing = modal.view.is_some();
        match tab {
            BackgroundTab::Subagents => {
                if viewing {
                    let Some(mut modal) = self.background_modal.take() else {
                        return;
                    };
                    if let Some(BackgroundView::Subagent(view)) = &mut modal.view {
                        self.refresh_subagent_view(view);
                    }
                    self.background_modal = Some(modal);
                } else {
                    let Some(registry) = self.subagents.as_ref() else {
                        return;
                    };
                    let runs = registry.live();
                    if let Some(modal) = self.background_modal.as_mut() {
                        modal.subagents.overlay_live(&runs);
                        clamp_selection(&mut modal.subagents.selected, modal.subagents.rows.len());
                    }
                }
            }
            BackgroundTab::Jobs => {
                let jobs = self.jobs_rows();
                let Some(mut modal) = self.background_modal.take() else {
                    return;
                };
                if let Some(BackgroundView::Job(view)) = &mut modal.view {
                    self.refresh_job_output(view);
                } else {
                    clamp_selection(&mut modal.jobs.selected, jobs.len());
                }
                self.background_modal = Some(modal);
            }
            BackgroundTab::Timers => {
                if !viewing {
                    let rows = self.timers_snapshot();
                    let Some(modal) = self.background_modal.as_mut() else {
                        return;
                    };
                    modal.timers.rows = rows;
                    clamp_selection(&mut modal.timers.selected, modal.timers.rows.len());
                }
            }
        }
    }

    /// Keys for the background dialog: the tab bar, the row list of the
    /// active tab, and the drill-in of one row. Always returns true while
    /// the dialog is open.
    pub(in crate::tui) fn handle_background_key(&mut self, k: &KeyEvent) -> bool {
        // Take the modal out so the body can call `&mut self` helpers without
        // overlapping the borrow; put it back (or not, on close).
        let Some(mut modal) = self.background_modal.take() else {
            return true;
        };

        if let Some(target) = modal.confirm.take() {
            if matches!(k.code, KeyCode::Char('y' | 'Y')) {
                self.kill_background(target);
            }
            self.background_modal = Some(modal);
            return true;
        }

        // Drill-in of one row.
        if let Some(view) = &mut modal.view {
            if Self::background_view_key(view, k) {
                modal.view = None;
            }
            self.background_modal = Some(modal);
            return true;
        }

        // Tab bar.
        let switch = match k.code {
            KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => Some(cycle_tab(modal.tab, 1)),
            KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => {
                Some(cycle_tab(modal.tab, BackgroundTab::ALL.len() - 1))
            }
            KeyCode::Char(c) => BackgroundTab::from_key(c),
            _ => None,
        };
        if let Some(tab) = switch {
            modal.show(tab);
            if tab == BackgroundTab::Timers {
                modal.timers.rows = self.timers_snapshot();
            }
            self.background_modal = Some(modal);
            return true;
        }

        // Row list of the active tab. Letters belong to the tab bar, so a
        // row moves only on the arrow keys.
        let jobs = self.jobs_rows();
        let len = modal.len(&jobs);
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return true,
            KeyCode::Up => {
                let row = modal.selected();
                modal.select(row.saturating_sub(1));
            }
            KeyCode::Down => {
                if len > 0 {
                    let row = modal.selected().saturating_add(1).min(len - 1);
                    modal.select(row);
                }
            }
            KeyCode::Char('r') => {
                self.reload_background_tab(&mut modal);
            }
            KeyCode::Enter => {
                modal.view = self.background_view(&modal, &jobs);
            }
            KeyCode::Char('x') => {
                modal.confirm = Self::background_kill_target(&modal, &jobs);
            }
            KeyCode::Char('X') if len > 0 => {
                modal.confirm = Some(BackgroundKill::All(modal.tab));
            }
            _ => {}
        }
        self.background_modal = Some(modal);
        true
    }

    /// Scroll keys inside a drill-in. Returns whether the key leaves it.
    fn background_view_key(view: &mut BackgroundView, k: &KeyEvent) -> bool {
        if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
            return true;
        }
        match view {
            BackgroundView::Job(job) => {
                if let JobViewContent::Log { lines, scroll, .. } = &mut job.content {
                    let max_scroll = lines.len().saturating_sub(1);
                    match k.code {
                        KeyCode::Up => *scroll = (*scroll + 1).min(max_scroll),
                        KeyCode::Down => *scroll = scroll.saturating_sub(1),
                        KeyCode::Char('g') => *scroll = max_scroll,
                        KeyCode::Char('G') => *scroll = 0,
                        _ => {}
                    }
                }
            }
            BackgroundView::Subagent(view) => {
                let max_scroll = view.total.saturating_sub(view.view_h.max(1));
                match k.code {
                    KeyCode::Up => view.scroll = (view.scroll + 1).min(max_scroll),
                    KeyCode::Down => view.scroll = view.scroll.saturating_sub(1),
                    KeyCode::Char('g') => view.scroll = max_scroll,
                    KeyCode::Char('G') => view.scroll = 0,
                    _ => {}
                }
            }
        }
        false
    }

    /// `r` in the dialog: re-read what the active tab lists. Jobs are read
    /// per frame, so only the subagent store listing and the timer rows are
    /// worth a reload.
    fn reload_background_tab(&mut self, modal: &mut BackgroundModalState) {
        match modal.tab {
            BackgroundTab::Subagents => self.reload_subagent_rows(&mut modal.subagents),
            BackgroundTab::Jobs => {}
            BackgroundTab::Timers => {
                modal.timers.rows = self.timers_snapshot();
                clamp_selection(&mut modal.timers.selected, modal.timers.rows.len());
            }
        }
    }

    /// Drill-in for the selected row of the active tab. A timer has no
    /// output to show.
    fn background_view(
        &self,
        modal: &BackgroundModalState,
        jobs: &[JobRow],
    ) -> Option<BackgroundView> {
        let selected = modal.selected();
        match modal.tab {
            BackgroundTab::Subagents => modal
                .subagents
                .rows
                .get(selected)
                .map(|row| BackgroundView::Subagent(self.subagent_transcript_view(row))),
            // A stale job has no log and no screen left to show.
            BackgroundTab::Jobs => {
                let JobRow::Live(job) = jobs.get(selected)? else {
                    return None;
                };
                let content = if job.tty {
                    self.jobs
                        .as_ref()
                        .and_then(|jobs| jobs.screen(job.id))
                        .map_or_else(JobViewContent::empty_log, JobViewContent::Terminal)
                } else {
                    JobViewContent::empty_log()
                };
                Some(BackgroundView::Job(JobOutputView {
                    id: job.id,
                    content,
                }))
            }
            BackgroundTab::Timers => None,
        }
    }

    /// Kill target for the selected row. A settled row has nothing to stop,
    /// so `x` on one arms nothing.
    fn background_kill_target(
        modal: &BackgroundModalState,
        jobs: &[JobRow],
    ) -> Option<BackgroundKill> {
        let selected = modal.selected();
        match modal.tab {
            BackgroundTab::Subagents => modal.subagents.rows.get(selected).and_then(|row| {
                row.running
                    .then(|| BackgroundKill::Subagent(row.id.clone()))
            }),
            BackgroundTab::Jobs => jobs
                .get(selected)
                .filter(|job| job.running())
                .map(|job| BackgroundKill::Job(job.id())),
            BackgroundTab::Timers => modal
                .timers
                .rows
                .get(selected)
                .map(|timer| BackgroundKill::Timer(timer.id)),
        }
    }

    /// Carry out an armed kill. Kill-all walks the registry, so a row the
    /// list has not refreshed yet still gets stopped.
    pub(in crate::tui) fn kill_background(&mut self, target: BackgroundKill) {
        match target {
            BackgroundKill::Job(id) => {
                if let Some(jobs) = &self.jobs {
                    jobs.kill(id);
                }
            }
            BackgroundKill::Subagent(id) => {
                if let Some(registry) = &self.subagents {
                    let _ = registry.kill(&id, Some("stopped from the background dialog"));
                }
            }
            BackgroundKill::Timer(id) => {
                if let Some(timers) = &self.timers {
                    let _ = timers.cancel(id);
                }
            }
            BackgroundKill::All(tab) => match tab {
                BackgroundTab::Jobs => {
                    if let Some(jobs) = &self.jobs {
                        for job in jobs.active_items().iter().filter(|job| job.running) {
                            jobs.kill(job.id);
                        }
                    }
                }
                BackgroundTab::Subagents => {
                    if let Some(registry) = &self.subagents {
                        for run in registry.running() {
                            let _ =
                                registry.kill(&run.id, Some("stopped from the background dialog"));
                        }
                    }
                }
                BackgroundTab::Timers => {
                    if let Some(timers) = &self.timers {
                        timers.reset();
                    }
                }
            },
        }
    }
}

/// Next tab in `step` direction, wrapping.
fn cycle_tab(tab: BackgroundTab, step: usize) -> BackgroundTab {
    let tabs = BackgroundTab::ALL;
    let at = tabs.iter().position(|&t| t == tab).unwrap_or(0);
    tabs[(at + step) % tabs.len()]
}

fn clamp_selection(selected: &mut usize, len: usize) {
    *selected = (*selected).min(len.saturating_sub(1));
}
