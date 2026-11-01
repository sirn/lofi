#![allow(clippy::wildcard_imports)]

use super::*;

impl App {
    pub(crate) fn render_header_line(&self, width: usize) -> Line<'static> {
        let t = self.theme;
        let wordmark = t.accent_heading_style();
        let muted = t.muted_style();
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

    /// Running work as one always-visible counter: `2 jobs · 1 subagent`,
    /// shrinking to `2j · 1s` when the footer is narrow. A terminal with
    /// nothing running gets no segment at all. A timer only waits, so it is
    /// listed in the dialog and kept out of this count.
    pub(crate) fn background_counter(&self, labels: bool) -> Option<String> {
        let jobs = self
            .jobs
            .as_ref()
            .map_or(0, lofi_core::JobRegistry::running_count);
        let subagents = self.running_subagents().len();
        counter_text(jobs, subagents, self.background_needs_you(), labels)
    }

    /// Work waiting on the user: a confirmation to answer, or a job whose
    /// completion notice has not been handed to the agent yet.
    pub(crate) fn background_needs_you(&self) -> bool {
        !self.pending_confirms.is_empty()
            || self
                .jobs
                .as_ref()
                .is_some_and(|jobs| jobs.active_items().iter().any(|job| !job.running))
    }

    /// Usage side of the footer, compacted in steps when it exceeds the
    /// width budget: first the `context` and `cached` labels, then the
    /// token-io counts, then everything. The counter outranks the usage
    /// detail: its counts survive the degradation that drops them.
    pub(crate) fn render_footer_left(&self, width: usize) -> Line<'static> {
        let t = self.theme;
        let usage = [
            self.footer_left_segments(true, true),
            self.footer_left_segments(false, true),
            self.footer_left_segments(false, false),
        ];
        // Widest first: the usage detail drops, then the counter's words,
        // and only then the counter itself.
        let mut candidates: Vec<(Option<String>, Vec<String>)> = Vec::new();
        if let Some(counter) = self.background_counter(true) {
            for usage in &usage {
                candidates.push((Some(counter.clone()), usage.clone()));
            }
        }
        if let Some(counter) = self.background_counter(false) {
            candidates.push((Some(counter.clone()), usage[2].clone()));
            candidates.push((Some(counter), Vec::new()));
        }
        for usage in &usage {
            candidates.push((None, usage.clone()));
        }
        candidates.push((None, Vec::new()));
        let (counter, segments) = candidates
            .into_iter()
            .find(|(counter, segments)| counter_row_width(counter.as_deref(), segments) <= width)
            .unwrap_or_default();
        let mut spans = Vec::new();
        if let Some(counter) = counter {
            let style = if self.background_needs_you() {
                Style::new().fg(t.warn)
            } else {
                t.muted_style()
            };
            spans.push(Span::styled(counter, style));
            if !segments.is_empty() {
                spans.push(Span::styled(" · ", t.muted_style()));
            }
        }
        spans.push(Span::styled(segments.join(" · "), t.muted_style()));
        Line::from(spans)
    }

    /// `labels` keeps the `context` word and the `N% cached` suffix;
    /// `io` keeps the in/out token counts.
    fn footer_left_segments(&self, labels: bool, io: bool) -> Vec<String> {
        let mut segments: Vec<String> = Vec::new();
        if io && (self.total_in > 0 || self.total_out > 0) {
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
        // The transport's own context report wins; otherwise the last
        // round's usage is the fill. Both transports read this one rule.
        let used = self.reported_context.map_or_else(
            || self.status_usage.map_or(0, |u| u.context_tokens()),
            |context| context.used,
        );
        let cached_suffix = if labels {
            self.status_usage.and_then(|u| {
                let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
                if prompt > 0 && (u.cache_read_tokens > 0 || u.cache_write_tokens > 0) {
                    let rate = u.cache_read_tokens as f64 / prompt as f64 * 100.0;
                    Some(format!(" · {rate:.0}% cached"))
                } else {
                    None
                }
            })
        } else {
            None
        };
        let used_str = if self.compacted {
            "c".to_string()
        } else {
            compact_count(used)
        };
        let context_label = if labels { "context " } else { "" };
        // A transport that reports its own window (the CLI child) is the
        // authority; otherwise the configured model window is.
        let size = self
            .reported_context
            .and_then(|context| context.size)
            .unwrap_or(self.ctx_limit);
        segments.push(format!(
            "{}{}/{}{}",
            context_label,
            used_str,
            compact_count(size),
            cached_suffix.unwrap_or_default()
        ));
        segments
    }

    pub(crate) fn retry_badge(&self) -> Option<String> {
        self.retry
            .as_ref()
            .map(|retry| format!("Retry: {} of {}", retry.attempt, retry.max_attempts))
    }

    /// The notification area's job lines. The registry owns the rule: a
    /// finished job stays listed while its terminal notice waits to be
    /// taken, and drops in the call that takes it.
    pub(crate) fn running_jobs(&self) -> Vec<lofi_core::JobInfo> {
        self.jobs
            .as_ref()
            .map_or_else(Vec::new, lofi_core::JobRegistry::active_items)
    }

    pub(crate) fn running_subagents(&self) -> Vec<lofi_core::SubagentRunning> {
        self.subagents
            .as_ref()
            .map(lofi_core::SubagentRegistry::running)
            .unwrap_or_default()
    }

    pub(crate) fn has_running_subagents(&self) -> bool {
        self.subagents
            .as_ref()
            .is_some_and(lofi_core::SubagentRegistry::has_running_background)
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

    /// The one notice the footer row shows, highest priority first.
    pub(crate) fn active_notice(&self) -> Option<(String, Color)> {
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

    /// The newest transient notice as one row, clipped to `width`. The
    /// notice area never grows the layout, so a long message loses its tail
    /// rather than pushing the transcript up.
    pub(crate) fn notice_line(&self, width: usize) -> Option<(String, Color)> {
        let (msg, color) = self.active_notice()?;
        if width == 0 {
            return None;
        }
        let flat = msg.split_whitespace().collect::<Vec<_>>().join(" ");
        if unicode_width::UnicodeWidthStr::width(flat.as_str()) <= width {
            return Some((flat, color));
        }
        let mut clipped = super::view::truncate(&flat, width.saturating_sub(1));
        clipped.push('…');
        Some((clipped, color))
    }

    /// Persistent chip while `/policy` set a non-default approval mode.
    pub(crate) fn policy_badge(&self) -> Option<String> {
        let mode = self.policy_override.as_ref()?.current()?;
        if mode == lofi_core::default_approval_mode(self.auto_mode_configured) {
            return None;
        }
        Some(policy_mode_label(mode).to_string())
    }

    /// Footer cost and active policy override, shown on the right edge of the
    /// usage line. Includes the current turn's running cost (`turn_cost`) so a
    /// multi-round turn shows a live total before `TurnEnd` folds it into `cost`.
    pub(crate) fn render_footer_cost(&self) -> Line<'static> {
        let t = self.theme;
        let mut spans = Vec::new();
        if let Some(badge) = self.policy_badge() {
            spans.push(Span::styled(badge, Style::new().fg(t.warn)));
            spans.push(Span::styled(" · ", t.muted_style()));
        }
        spans.push(Span::styled(
            fmt_cost(self.cost + self.turn_cost),
            t.muted_style(),
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
        Line::from(vec![Span::styled(label, self.theme.muted_style())])
    }
}

/// The counter body: `2 jobs · 1 subagent`, or `2j · 1s` when `labels` is
/// false, with ` ●` appended while work waits on the user. Nothing running
/// renders nothing.
pub(crate) fn counter_text(
    jobs: usize,
    subagents: usize,
    needs_you: bool,
    labels: bool,
) -> Option<String> {
    let mut parts = Vec::new();
    for (count, one, many, short) in [
        (jobs, "job", "jobs", "j"),
        (subagents, "subagent", "subagents", "s"),
    ] {
        if count == 0 {
            continue;
        }
        parts.push(if labels {
            if count == 1 {
                format!("{count} {one}")
            } else {
                format!("{count} {many}")
            }
        } else {
            format!("{count}{short}")
        });
    }
    if parts.is_empty() {
        return None;
    }
    let mut counter = parts.join(" · ");
    if needs_you {
        counter.push_str(" ●");
    }
    Some(counter)
}

/// Width of the footer's left side: the counter, then the usage segments
/// joined by ` · `.
fn counter_row_width(counter: Option<&str>, segments: &[String]) -> usize {
    let usage = footer_segments_width(segments);
    counter.map_or(usage, |counter| {
        let counter = unicode_width::UnicodeWidthStr::width(counter);
        if segments.is_empty() {
            counter
        } else {
            counter + 3 + usage
        }
    })
}

/// Display width of footer segments joined by ` · `.
fn footer_segments_width(segments: &[String]) -> usize {
    let body: usize = segments
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.as_str()))
        .sum();
    body + 3 * segments.len().saturating_sub(1)
}
