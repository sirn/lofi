#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(crate) fn render_header_line(&self, width: usize) -> Line<'static> {
        let t = self.theme;
        let wordmark = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
        let muted = Style::new().fg(t.muted);
        let lofi_w = unicode_width::UnicodeWidthStr::width("lofi");
        let model = self.render_footer_right();
        let model_w: usize = model
            .spans
            .iter()
            .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
            .sum();
        let budget = width
            .saturating_sub(lofi_w)
            .saturating_sub(1)
            .saturating_sub(model_w)
            .saturating_sub(2);
        let cwd = abbreviate_path(&self.session.cwd, budget);
        HStack::new(width)
            .left([
                Span::styled("lofi", wordmark),
                Span::raw(" "),
                Span::styled(cwd, muted),
            ])
            .right(model.spans)
            .build()
    }

    pub(crate) fn render_footer_left(&self, _width: usize) -> Line<'static> {
        let t = self.theme;
        let sep = " · ";
        let mut segments: Vec<String> = Vec::new();
        if self.total_in > 0 || self.total_out > 0 {
            segments.push(format!(
                "↑{} ↓{}",
                compact_count(self.total_in),
                compact_count(self.total_out)
            ));
        }
        // Cache tokens are included so the gauge reflects the real window
        // usage rather than only the non-cached slice; the soft-compaction
        // policy reads the same `context_tokens` value. When the provider
        // reports cache activity, append the hit rate as `N% cached`.
        let used = self.status_usage.map_or(0, |u| u.context_tokens());
        let cached_suffix = self.status_usage.and_then(|u| {
            let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
            if prompt > 0 && (u.cache_read_tokens > 0 || u.cache_write_tokens > 0) {
                let rate = u.cache_read_tokens as f64 / prompt as f64 * 100.0;
                Some(format!(" · {rate:.0}% cached"))
            } else {
                None
            }
        });
        let used_str = if self.compacted {
            "c".to_string()
        } else {
            compact_count(used)
        };
        if self.external_backend {
            segments.push(self.backend_context.map_or_else(
                || "context unknown".to_string(),
                |context| {
                    format!(
                        "context {}/{}",
                        compact_count(context.used),
                        context
                            .size
                            .map_or_else(|| "unknown".to_string(), compact_count)
                    )
                },
            ));
        } else {
            segments.push(format!(
                "context {}/{}{}",
                used_str,
                compact_count(self.ctx_limit),
                cached_suffix.unwrap_or_default()
            ));
        }
        Line::from(vec![Span::styled(
            segments.join(sep),
            Style::new().fg(t.muted),
        )])
    }

    pub(crate) fn retry_badge(&self) -> Option<String> {
        self.retry
            .as_ref()
            .map(|retry| format!("Retry: {} of {}", retry.attempt, retry.max_attempts))
    }

    pub(crate) fn running_jobs(&self) -> Vec<lofi_core::JobInfo> {
        tail_jobs(self.jobs_snapshot(), &self.notified_jobs)
    }

    pub(crate) fn has_running_jobs(&self) -> bool {
        self.jobs
            .as_ref()
            .is_some_and(|jobs| jobs.running_count() > 0)
    }

    pub(crate) fn yank_badge(&self) -> Option<&'static str> {
        match self.yank_notify {
            Some(t) if t.elapsed() < YANK_NOTIFY => Some("Copied to clipboard"),
            _ => None,
        }
    }

    pub(crate) fn quit_badge(&self) -> Option<&'static str> {
        match self.ctrl_c_at {
            Some(t) if t.elapsed() < QUIT_DOUBLE_PRESS => Some("Press Ctrl-C again to quit"),
            _ => None,
        }
    }

    pub(crate) fn notify_badge(&self) -> Option<(&str, NotifyKind)> {
        match &self.notify {
            Some(n) if n.at.elapsed() < NOTIFY_TTL => Some((&n.msg, n.kind)),
            _ => None,
        }
    }

    pub(crate) fn active_toast(&self) -> Option<(String, Color)> {
        let t = self.theme;
        if let Some(badge) = self.quit_badge() {
            Some((badge.to_string(), t.warn))
        } else if let Some(badge) = self.yank_badge() {
            Some((badge.to_string(), t.primary))
        } else if let Some(retry) = self.retry_badge() {
            Some((retry, t.warn))
        } else if let Some((msg, kind)) = self.notify_badge() {
            let color = match kind {
                NotifyKind::Info => t.primary,
                NotifyKind::Warn => t.warn,
                NotifyKind::Error => t.error,
            };
            Some((msg.to_string(), color))
        } else {
            None
        }
    }

    pub(crate) fn toast_lines(&self, w: usize) -> Option<(Vec<String>, Color)> {
        let (msg, color) = self.active_toast()?;
        let avail = w.saturating_sub(4);
        if avail == 0 {
            return None;
        }
        let mut lines = super::view::wrap(&msg, avail);
        let overflow = lines.len() > NOTIFY_MAX_LINES;
        lines.truncate(NOTIFY_MAX_LINES);
        if overflow {
            if let Some(last) = lines.last_mut() {
                *last = super::view::truncate(last, avail.saturating_sub(1));
                last.push('…');
            }
        }
        Some((lines, color))
    }

    /// Persistent chip while `/policy` set a non-default approval mode.
    pub(crate) fn policy_badge(&self) -> Option<String> {
        let mode = self.policy_override.as_ref()?.current()?;
        if mode == lofi_core::default_approval_mode(self.auto_mode_configured) {
            return None;
        }
        Some(policy_mode_label(mode).to_string())
    }

    /// Rows the toast body occupies inside its border at the given terminal width.
    #[cfg(test)]
    pub(crate) fn notify_lines(&self, w: usize) -> u16 {
        self.toast_lines(w)
            .map_or(0, |(lines, _)| lines.len() as u16)
    }

    /// Footer cost and active policy override, shown on the right edge of the
    /// usage line. Includes the current turn's running cost (`turn_cost`) so a
    /// multi-round turn shows a live total before `TurnEnd` folds it into `cost`.
    pub(crate) fn render_footer_cost(&self) -> Line<'static> {
        let t = self.theme;
        let mut spans = Vec::new();
        if let Some(badge) = self.policy_badge() {
            spans.push(Span::styled(badge, Style::new().fg(t.warn)));
            spans.push(Span::styled(" · ", Style::new().fg(t.muted)));
        }
        spans.push(Span::styled(
            fmt_cost(self.cost + self.turn_cost),
            Style::new().fg(t.muted),
        ));
        Line::from(spans)
    }

    pub(crate) fn render_footer_right(&self) -> Line<'static> {
        let mut label = self.model_label.clone();
        if let Some(tl) = &self.thinking_label {
            label.push_str(tl);
        }
        if let Some(st) = &self.service_label {
            label.push_str(st);
        }
        Line::from(vec![Span::styled(label, Style::new().fg(self.theme.muted))])
    }
}

/// Jobs the transcript tail still shows as running. The registry flips a
/// job to terminal before its completion notice reaches the run loop, so a
/// terminal job with notices enabled stays listed until its notice has been
/// queued — the running line and the notice swap in one frame instead of
/// the tail collapsing and regrowing a tick apart. Jobs spawned with
/// \`notify: false\` never produce a notice and drop out immediately.
fn tail_jobs(
    snapshot: Vec<lofi_core::JobInfo>,
    notified: &std::collections::HashSet<u64>,
) -> Vec<lofi_core::JobInfo> {
    snapshot
        .into_iter()
        .filter(|job| job.running || (job.notify && !notified.contains(&job.id)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::tail_jobs;

    fn job(id: u64, running: bool, notify: bool) -> lofi_core::JobInfo {
        lofi_core::JobInfo {
            id,
            cmd: format!("cmd {id}"),
            state: if running { "running" } else { "completed" }.to_string(),
            running,
            pid: None,
            exit_code: None,
            duration_ms: 1_000,
            log_path: String::new(),
            tty: false,
            notify,
        }
    }

    fn ids(jobs: &[lofi_core::JobInfo]) -> Vec<u64> {
        jobs.iter().map(|j| j.id).collect()
    }

    #[test]
    fn terminal_job_stays_listed_until_its_notice_lands() {
        let mut notified = std::collections::HashSet::new();
        let snapshot = || vec![job(1, false, true)];
        assert_eq!(ids(&tail_jobs(snapshot(), &notified)), vec![1]);
        notified.insert(1);
        assert!(tail_jobs(snapshot(), &notified).is_empty());
    }

    #[test]
    fn terminal_job_without_notify_drops_out_immediately() {
        let notified = std::collections::HashSet::new();
        let snapshot = vec![job(1, true, true), job(2, false, false)];
        assert_eq!(ids(&tail_jobs(snapshot, &notified)), vec![1]);
    }
}
