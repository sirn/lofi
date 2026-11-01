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

    /// Fresh job snapshot for the dialog, newest first. Empty when no agent.
    pub(in crate::tui) fn jobs_snapshot(&self) -> Vec<lofi_core::JobInfo> {
        self.jobs
            .as_ref()
            .map_or_else(Vec::new, lofi_core::JobRegistry::snapshot)
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
