use super::*;

impl App {
    /// Footer mode tag shown at the far left of the status line.
    pub(crate) fn mode_badge(&self) -> (&'static str, Color) {
        let t = self.theme;
        match self.mode {
            Mode::Input => ("INPUT", t.muted),
            Mode::Navigate => ("NAV", t.primary),
            Mode::Select => ("SELECT", t.warn),
        }
    }

    /// Header line: `lofi` wordmark at the left, the working directory
    /// (abbreviated to fit) right-aligned. The model and cost live in the
    /// footer; the header carries no status tag.
    pub(crate) fn render_header_line(&self, width: usize) -> Line<'static> {
        let t = self.theme;
        let wordmark = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
        let muted = Style::new().fg(t.muted);
        let lofi_w = unicode_width::UnicodeWidthStr::width("lofi");
        // Model label sits at the right edge; the cwd follows the wordmark
        // on the left, abbreviated to whatever the model leaves behind.
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

    /// Bottom-left footer: `↑in ↓out · context used/limit · N% cached`.
    /// separately on the right via [`render_footer_cost`].
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
        // Context gauge: the latest turn's full prompt size (input + output +
        // cache read + cache write).
        // Cache tokens are included so the gauge reflects the real window usage
        // rather than only the non-cached slice. When the provider reports
        // cache activity, append the hit rate as `N% cached`.
        let used = self.status_usage.map_or(0, |u| {
            u.input_tokens + u.output_tokens + u.cache_read_tokens + u.cache_write_tokens
        });
        let cached_suffix = self.status_usage.and_then(|u| {
            let prompt = u.input_tokens + u.cache_read_tokens + u.cache_write_tokens;
            if prompt > 0 && (u.cache_read_tokens > 0 || u.cache_write_tokens > 0) {
                let rate = u.cache_read_tokens as f64 / prompt as f64 * 100.0;
                Some(format!(" · {:.0}% cached", rate))
            } else {
                None
            }
        });
        segments.push(format!(
            "context {}/{}{}",
            compact_count(used),
            compact_count(self.ctx_limit),
            cached_suffix.unwrap_or_default()
        ));
        Line::from(vec![Span::styled(segments.join(sep), Style::new().fg(t.muted))])
    }

    /// Text for the transient "Copied to clipboard" badge, or `None` if the
    /// yank notification has expired.
    pub(crate) fn yank_badge(&self) -> Option<&'static str> {
        match self.yank_notify {
            Some(t) if t.elapsed() < YANK_NOTIFY => Some("Copied to clipboard"),
            _ => None,
        }
    }

    /// Text for the transient "Press Ctrl-C again to quit" badge shown after a
    /// first `C-c` on an empty prompt, or `None` once the double-press window
    /// has elapsed.
    pub(crate) fn quit_badge(&self) -> Option<&'static str> {
        match self.ctrl_c_at {
            Some(t) if t.elapsed() < QUIT_DOUBLE_PRESS => Some("Press Ctrl-C again to quit"),
            _ => None,
        }
    }

    /// The active notification's message and severity, or `None` once it has
    /// expired ([`NOTIFY_TTL`]).
    pub(crate) fn notify_badge(&self) -> Option<(&str, NotifyKind)> {
        match &self.notify {
            Some(n) if n.at.elapsed() < NOTIFY_TTL => Some((&n.msg, n.kind)),
            _ => None,
        }
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

    /// Bottom-right footer: the model badge (with thinking level).
    pub(crate) fn render_footer_right(&self) -> Line<'static> {
        let mut label = self.model_label.clone();
        if let Some(tl) = &self.thinking_label {
            label.push_str(tl);
        }
        Line::from(vec![Span::styled(label, Style::new().fg(self.theme.muted))])
    }
}
