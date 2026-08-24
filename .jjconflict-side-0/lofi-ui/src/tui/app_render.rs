#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(crate) fn mode_badge(&self) -> (&'static str, Color) {
        let t = self.theme;
        match self.mode {
            Mode::Input => ("INPUT", t.muted),
            Mode::Navigate => ("NAV", t.primary),
            Mode::Select => ("SELECT", t.warn),
        }
    }

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
        segments.push(format!(
            "context {}/{}{}",
            used_str,
            compact_count(self.ctx_limit),
            cached_suffix.unwrap_or_default()
        ));
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

    /// Persistent count of running background jobs, e.g. "1 job" / "2 jobs".
    /// Always visible while any job runs, unlike the transient badges.
    pub(crate) fn jobs_badge(&self) -> Option<String> {
        let jobs = self.jobs.as_ref()?;
        let n = jobs.running_count();
        if n == 0 {
            None
        } else if n == 1 {
            Some("1 job".to_string())
        } else {
            Some(format!("{n} jobs"))
        }
    }

    pub(crate) fn queue_badge(&self) -> Option<String> {
        if self.prompt_queue.is_empty() {
            return None;
        }
        let n = self.prompt_queue.len();
        let head = &self.prompt_queue[0];
        // Match the transcript marker so a queued Notice reads the same
        // everywhere: a hollow bullet signals "system-injected" the way the
        // user's solid mark signals "you typed this".
        let marker = match head.kind {
            lofi_types::PromptKind::User => "",
            lofi_types::PromptKind::Notice => "▷ ",
        };
        let preview = head.text.as_str();
        let truncated = if preview.chars().count() > 40 {
            let mut s: String = preview.chars().take(39).collect();
            s.push('…');
            s
        } else {
            preview.to_string()
        };
        Some(if n == 1 {
            format!("Queue: {marker}{truncated}")
        } else {
            format!("Queue: {marker}{truncated} (+{})", n - 1)
        })
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

    /// Persistent chip while `/policy` set a non-default approval mode.
    pub(crate) fn policy_badge(&self) -> Option<String> {
        let mode = self.policy_override.as_ref()?.current()?;
        if mode == lofi_core::default_approval_mode(self.auto_mode_configured) {
            return None;
        }
        Some(format!("policy: {}", policy_mode_label(mode)))
    }

    /// Rows the notification area occupies at the given terminal width.
    /// Only the transient notify badge can overflow one line — quit, yank,
    /// retry, and queue badges are short by construction and always take a
    /// single row. A long notify wraps to at most [`NOTIFY_MAX_LINES`] rows.
    pub(crate) fn notify_lines(&self, w: usize) -> u16 {
        let Some((msg, _)) = self.notify_badge() else {
            return 1;
        };
        let (label, _) = self.mode_badge();
        let mode_w = super::view::width(label) + 2;
        let avail = w.saturating_sub(4).saturating_sub(mode_w);
        if avail == 0 {
            return 1;
        }
        super::view::wrap(msg, avail).len().min(NOTIFY_MAX_LINES) as u16
    }

    /// Footer cost, shown on the right edge of the usage line. Includes the
    /// current turn's running cost (`turn_cost`) so a multi-round turn shows
    /// a live total before `TurnEnd` folds it into `cost`.
    pub(crate) fn render_footer_cost(&self) -> Line<'static> {
        Line::from(vec![Span::styled(
            fmt_cost(self.cost + self.turn_cost),
            Style::new().fg(self.theme.muted),
        )])
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
