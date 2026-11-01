use super::*;

impl App {
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

    /// Take the wake-up notices fired by `notifyAt`/`notifyEvery` timers
    /// since the last tick, oldest first. Unlike job notices these carry no
    /// notification-area line, so nothing else changes when one is taken.
    pub(in crate::tui) fn take_timer_notices(&mut self) -> Vec<QueuedPrompt> {
        self.timers.as_ref().map_or_else(Vec::new, |timers| {
            timers
                .take_notices()
                .into_iter()
                .map(|text| QueuedPrompt {
                    text,
                    kind: lofi_types::PromptKind::Notice,
                })
                .collect()
        })
    }

    /// Append the pending wake-up set when it changed since the last
    /// record. One snapshot per change is enough: the last one on the
    /// lineage is what a resume re-arms.
    pub(in crate::tui) fn record_timer_state(&mut self) {
        let Some(timers) = &self.timers else {
            return;
        };
        let records = timers.records();
        if !timer_shape_changed(&records, &self.timers_recorded) {
            return;
        }
        let Some(cursor) = &self.session.cursor else {
            return;
        };
        // Only a snapshot the transcript holds counts as recorded: a failed
        // append must come back on the next tick.
        if cursor.record_timer_state(&records).is_ok() {
            self.timers_recorded.clone_from(&records);
        }
    }

    /// `/new` sheds the pending timers, so the still-open transcript records
    /// the empty set: resuming that session must not re-arm them. The caller
    /// drops the timers either way and reports the error, because nothing
    /// retries the append once the cursor is gone.
    pub(in crate::tui) fn record_dropped_timers(&self) -> lofi_error::Result<()> {
        if self.timers_recorded.is_empty() {
            return Ok(());
        }
        match &self.session.cursor {
            Some(cursor) => cursor.record_timer_state(&[]),
            None => Ok(()),
        }
    }

    /// Fresh job snapshot for the dialog, newest first. Empty when no agent.
    pub(in crate::tui) fn jobs_snapshot(&self) -> Vec<lofi_core::JobInfo> {
        self.jobs
            .as_ref()
            .map_or_else(Vec::new, lofi_core::JobRegistry::snapshot)
    }

    /// Jobs-tab rows: the live registry, then the jobs a resumed transcript
    /// still lists as started. Those jobs died with their process, so the row
    /// shows only what the record kept.
    pub(in crate::tui) fn jobs_rows(&self) -> Vec<JobRow> {
        let mut rows: Vec<JobRow> = self.jobs_snapshot().into_iter().map(JobRow::Live).collect();
        rows.extend(self.stale_jobs.iter().cloned().map(JobRow::Stale));
        rows
    }

    /// Fresh timer listing for the dialog, soonest first. Empty when no
    /// agent.
    pub(in crate::tui) fn timers_snapshot(&self) -> Vec<lofi_core::TimerInfo> {
        self.timers
            .as_ref()
            .map_or_else(Vec::new, lofi_core::TimerRegistry::listing)
    }

    /// Refresh the open output view. PTY jobs copy their latest parsed screen;
    /// plain jobs page a tail window capped at `JOB_LOG_WINDOW_BYTES`.
    pub(in crate::tui) fn refresh_job_output(&mut self, view: &mut JobOutputView) {
        let Some(jobs) = &self.jobs else {
            return;
        };
        let id = view.id;
        match &mut view.content {
            JobViewContent::Terminal(screen) => {
                if let Some(current) = jobs.screen(id) {
                    *screen = current;
                }
            }
            JobViewContent::Log {
                lines,
                cursor,
                total,
                ..
            } => {
                let Some((bytes, next, new_total)) = jobs.read_log(id, *cursor, 64 * 1024) else {
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
}

/// Whether a resume needs the new set: its members and what each one fires.
/// `due` is left out on purpose. A repeating timer only moves its next fire
/// when it fires, and a resume recomputes that from the interval: one catch-up
/// notice, then the old cadence. Recording every fire instead would add a
/// transcript line per interval for a timer that repeats each second.
fn timer_shape_changed(now: &[lofi_types::TimerRecord], was: &[lofi_types::TimerRecord]) -> bool {
    now.len() != was.len()
        || now.iter().zip(was).any(|(new, old)| {
            new.id != old.id
                || new.every_ms != old.every_ms
                || new.message != old.message
                || new.owner != old.owner
        })
}
