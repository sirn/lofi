use super::*;

impl App {
    /// `/job`: open the background-jobs modal. Lists every session job with
    /// live status; `Enter` drills into a job's output, `x` arms a kill
    /// confirm.
    /// No-op (with a notice) when no agent is configured.
    pub(in crate::tui) fn open_jobs_modal(&mut self) {
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

    /// Take the notices the registry queued since the last tick, oldest
    /// first. Taking a notice is also what drops its job from the
    /// notification area, so the caller must hand every one of these over
    /// before the next frame.
    pub(in crate::tui) fn take_job_notices(&mut self) -> Vec<QueuedPrompt> {
        self.jobs.as_ref().map_or_else(Vec::new, |jobs| {
            jobs.take_notices()
                .into_iter()
                .map(|notice| QueuedPrompt {
                    text: notice.text,
                    kind: lofi_types::PromptKind::Notice,
                })
                .collect()
        })
    }

    /// Fresh job snapshot for the modal, newest first. Empty when no agent.
    pub(in crate::tui) fn jobs_snapshot(&self) -> Vec<lofi_core::JobInfo> {
        self.jobs
            .as_ref()
            .map_or_else(Vec::new, lofi_core::JobRegistry::snapshot)
    }

    /// Refresh the open output view. PTY jobs copy their latest parsed screen;
    /// plain jobs page a tail window capped at `JOB_LOG_WINDOW_BYTES`.
    pub(in crate::tui) fn refresh_job_output(&mut self) {
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
    pub(super) fn handle_jobs_key(&mut self, k: &KeyEvent) -> bool {
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
}
