use std::fmt::Write as _;
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::tui::theme::active_indicator;
use crate::tui::{DetailKey, ToolCall, SPINNER};

use super::detail::{detail_box, detail_row_count};
use super::native::split_lines;
use crate::tui::view::component::{Component, Cx};
use crate::tui::view::markdown::render_markdown_body;
use crate::tui::view::prim::{self, RenderLine};

pub(super) struct WorkingLine;

impl Component for WorkingLine {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let frame = SPINNER[cx.app.spinner_frame() % SPINNER.len()];
        vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled(format!("{frame} "), Style::new().fg(active_indicator(t))),
            ],
            vec![
                Span::styled(
                    format!(
                        "Working for {} with ",
                        prim::fmt_duration(cx.app.run_elapsed())
                    ),
                    Style::new().fg(t.subtle),
                ),
                Span::styled(cx.app.run_label(), Style::new().fg(t.muted)),
            ],
            vec![],
        )]
    }
}

pub(super) struct RunningJobLine<'a> {
    pub(super) job: &'a lofi_core::JobInfo,
}

impl Component for RunningJobLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let dur = prim::fmt_duration(Duration::from_millis(self.job.duration_ms));
        let cmd = self.job.cmd.trim();
        let text = format!("job {} running ({dur}): {cmd}", self.job.id);
        let mark = Style::new().fg(t.info);
        let body_style = Style::new().fg(t.muted);
        render_markdown_body(
            &text,
            t,
            cx.width,
            cx.width.saturating_sub(2),
            body_style,
            move |_| vec![Span::styled("┆ ", mark)],
        )
    }
}

pub(super) struct RunningSubagentLine<'a> {
    pub(super) run: &'a lofi_core::SubagentRunning,
}

impl Component for RunningSubagentLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let dur = prim::fmt_duration(Duration::from_millis(self.run.elapsed_ms));
        let task = self.run.task.trim();
        let text = if task.is_empty() {
            format!("subagent {} running ({dur})", self.run.id)
        } else {
            format!("subagent {} running ({dur}): {task}", self.run.id)
        };
        let mark = Style::new().fg(t.info);
        let body_style = Style::new().fg(t.muted);
        render_markdown_body(
            &text,
            t,
            cx.width,
            cx.width.saturating_sub(2),
            body_style,
            move |_| vec![Span::styled("┆ ", mark)],
        )
    }
}

pub(super) struct QueuedPromptLine<'a> {
    pub(super) text: &'a str,
    pub(super) kind: lofi_types::PromptKind,
}

impl Component for QueuedPromptLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = Style::new().fg(t.muted);
        let (bar, body_style) = match self.kind {
            lofi_types::PromptKind::Notice => (
                "▎ ",
                Style::new().fg(t.muted).add_modifier(Modifier::ITALIC),
            ),
            lofi_types::PromptKind::User => ("▌ ", Style::new().fg(t.muted)),
        };
        render_markdown_body(
            self.text.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            body_style,
            move |_| vec![Span::styled(bar, mark)],
        )
    }
}

pub(super) struct ToolLine<'a> {
    pub(super) tool: &'a ToolCall,
}

impl Component for ToolLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let working = !self.tool.done && cx.active_turn;
        let icon = prim::status_icon(t, working, self.tool.is_error, cx.spinner());
        let mut content = vec![Span::styled(
            self.tool.name.clone(),
            Style::new().fg(t.info),
        )];
        if let Some(first) = self.tool.input.split('\n').next() {
            if !first.is_empty() {
                content.push(prim::subtle(format!(" {first}"), t));
            }
        }
        vec![prim::rline(vec![Span::raw("  "), icon], content)]
    }
}

#[allow(clippy::struct_excessive_bools)] // view mirror of Block::UserShell's status flags
pub(super) struct UserShellLine<'a> {
    pub(super) id: u64,
    pub(super) command: &'a str,
    pub(super) output: &'a str,
    pub(super) exit_code: Option<i32>,
    pub(super) signal: Option<i32>,
    pub(super) duration: Duration,
    pub(super) truncated: bool,
    pub(super) cancelled: bool,
    pub(super) running: bool,
    pub(super) exclude_from_context: bool,
}

impl Component for UserShellLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let failed = !self.running
            && (self.cancelled || self.signal.is_some() || self.exit_code.is_some_and(|c| c != 0));
        let status_color = if failed { t.error } else { t.success };
        let command_style = Style::new().fg(t.fg);
        let mut out = Vec::new();

        let command_avail = cx.width.saturating_sub(4);
        for (i, seg) in prim::wrap_pre(self.command, command_avail)
            .into_iter()
            .enumerate()
        {
            let prompt = if i == 0 {
                Span::styled("$ ", Style::new().fg(t.success))
            } else {
                Span::raw("  ")
            };
            out.push(prim::rline(
                vec![Span::raw("  "), prompt],
                vec![Span::styled(seg, command_style)],
            ));
        }

        let lines = split_lines(self.output);
        let key = DetailKey::UserShell(self.id);
        let deco = vec![
            Span::raw("  "),
            Span::styled("  ", Style::new().fg(t.subtle)),
        ];
        let total = detail_row_count(&lines, cx.width, &deco);
        let has_detail = !self.output.is_empty();
        let status = if self.running {
            "Running".to_string()
        } else {
            let mut status = if self.cancelled {
                "Cancelled".to_string()
            } else if let Some(signal) = self.signal {
                format!("Signal {signal}")
            } else {
                format!("Exit {}", self.exit_code.unwrap_or(0))
            };
            let _ = write!(status, ", took {}", prim::fmt_duration(self.duration));
            if self.truncated {
                status.push_str(" · truncated");
            }
            if self.exclude_from_context {
                status.push_str(" · not in context");
            }
            status
        };
        let content = vec![Span::styled(status, Style::new().fg(t.fg))];
        let corner = if self.running {
            prim::status_icon(t, true, false, cx.spinner())
        } else {
            Span::styled(
                if failed { "✗ " } else { "✓ " },
                Style::new().fg(status_color),
            )
        };
        let mut header = prim::rline(
            vec![
                Span::raw("  "),
                Span::styled("└ ", Style::new().fg(t.subtle)),
                corner,
            ],
            content,
        );
        if has_detail {
            header = header.with_detail(key.clone(), total, true);
        }
        out.push(header);
        if has_detail {
            out.extend(detail_box(
                &lines,
                &key,
                true,
                cx,
                &deco,
                Style::new().fg(if failed { t.error } else { t.muted }),
            ));
        }
        out
    }
}

pub(super) struct ErrorLine<'a> {
    pub(super) msg: &'a str,
}

impl Component for ErrorLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let err = Style::new().fg(t.error);
        let content_w = cx.width.saturating_sub(4); // "  " + "✗ "
        let mut out = Vec::new();
        for (i, seg) in prim::wrap(self.msg, content_w).iter().enumerate() {
            let deco = if i == 0 {
                vec![Span::raw("  "), Span::styled("✗ ", err)]
            } else {
                vec![Span::raw("  "), Span::raw("  ")]
            };
            out.push(prim::rline(deco, vec![Span::styled(seg.clone(), err)]));
        }
        out
    }
}

/// Turn-end separator: `Done in Ns with <label>`. Appended to a turn when
/// its run finishes. Carries only model, level, and duration so the line
/// never overflows; nothing is wrapped below it.
pub(super) struct TurnEnd {
    pub(super) label: String,
    pub(super) elapsed: Duration,
}

impl Component for TurnEnd {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        if cx.active_turn {
            return Vec::new();
        }
        let t = cx.theme;
        let dur = prim::fmt_duration(self.elapsed);
        vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.subtle)),
            ],
            vec![
                Span::styled(format!("Done in {dur} with "), Style::new().fg(t.subtle)),
                Span::styled(self.label.clone(), Style::new().fg(t.muted)),
            ],
            vec![],
        )]
    }
}

/// Turn-failed separator. Line 1 carries model, level, and duration only
/// (`◇ Failed in Ns with <label>`) so the status never overflows; the provider
/// error is wrapped below it, indented and word-broken with a wide-char
/// fallback. Mirrors [`TurnEnd`] but signals the turn did not complete;
/// the turn's partial content precedes it on the same branch.
/// When the error is empty the failure was already surfaced as a fatal `✗`
/// line (see [`ErrorLine`]) earlier in the turn, so nothing is repeated
/// below the header. The builder drops the text in that case rather than
/// rendering a redundant copy.
pub(super) struct TurnFailed {
    pub(super) label: String,
    pub(super) elapsed: Duration,
    pub(super) error: String,
}

impl Component for TurnFailed {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        if cx.active_turn {
            return Vec::new();
        }
        let t = cx.theme;
        let dur = prim::fmt_duration(self.elapsed);
        let mut out = vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.error)),
            ],
            vec![
                Span::styled(format!("Failed in {dur} with "), Style::new().fg(t.error)),
                Span::styled(self.label.clone(), Style::new().fg(t.error)),
            ],
            vec![],
        )];
        let err = self.error.trim();
        if !err.is_empty() {
            // Wrap below the header so a long provider error isn't clipped
            // at the terminal edge. Indented to align under the label;
            // blank source lines are preserved as blank wrapped lines.
            let indent = "    ";
            let content_w = cx.width.saturating_sub(indent.len());
            for raw in err.split('\n') {
                let line = raw.trim_end();
                if line.is_empty() {
                    out.push(prim::rblank());
                } else {
                    for seg in prim::wrap(line, content_w) {
                        out.push(prim::render(
                            vec![Span::raw(indent)],
                            vec![Span::styled(seg, Style::new().fg(t.error))],
                            vec![],
                        ));
                    }
                }
            }
        }
        out
    }
}

/// Turn-cancelled separator. Shows an "Operation aborted" status while
/// keeping partial assistant content immediately above it.
pub(super) struct TurnCancelled {
    pub(super) label: String,
    pub(super) elapsed: Duration,
}

impl Component for TurnCancelled {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        if cx.active_turn {
            return Vec::new();
        }
        let t = cx.theme;
        let dur = prim::fmt_duration(self.elapsed);
        vec![prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.error)),
            ],
            vec![
                Span::styled(
                    format!("Cancelled after {dur} with "),
                    Style::new().fg(t.error),
                ),
                Span::styled(self.label.clone(), Style::new().fg(t.error)),
            ],
            vec![],
        )]
    }
}

/// Compaction marker appended when `/compact` or the automatic trigger folds
/// older history into a summary.
pub(super) struct CompactionLine {
    pub(super) summarized: usize,
    pub(super) kept: usize,
}

impl Component for CompactionLine {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let body = format!(
            "Compacted {} messages · kept {}",
            self.summarized, self.kept
        );
        let marker = prim::render(
            vec![
                Span::raw("  "),
                Span::styled("◇ ", Style::new().fg(t.subtle)),
            ],
            vec![Span::styled(body, Style::new().fg(t.muted))],
            vec![],
        );
        vec![marker]
    }
}

// `active_indicator` is re-exported for the working indicator in the chrome;
// keep the import here so the component module can surface it if needed.
#[allow(unused_imports)]
use active_indicator as _;
