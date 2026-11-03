use std::fmt::Write as _;
use std::time::Duration;

use ratatui::style::Style;
use ratatui::text::Span;

use crate::tui::text::fmt_cost;
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
        let mut content = vec![
            Span::styled(
                format!(
                    "Working for {} with ",
                    prim::fmt_duration(cx.app.run_elapsed())
                ),
                t.subtle_style(),
            ),
            Span::styled(cx.app.run_label(), t.muted_style()),
        ];
        // A cancel is a request the turn answers on its own schedule. Say so
        // while it is pending, and say plainly when the turn has stopped
        // answering it, because only the user can end that run.
        if cx.app.cancel_pending() {
            let (text, style) = if cx.app.cancel_unacknowledged() {
                (
                    " · cancelling — no response, Ctrl-C to quit",
                    Style::new().fg(t.error),
                )
            } else {
                (" · cancelling", t.subtle_style())
            };
            content.push(Span::styled(text, style));
        }
        vec![prim::render(
            vec![Span::styled(
                format!("{frame} "),
                Style::new().fg(active_indicator(t)),
            )],
            content,
            vec![],
        )]
    }
}

/// One row per running item in the block above the input: `job 3: cargo
/// test` with its elapsed time on the right edge. A row never wraps.
pub(super) fn running_rows(
    cx: &Cx,
    jobs: &[lofi_core::JobInfo],
    subagents: &[lofi_core::SubagentRunning],
    timers: &[lofi_core::TimerInfo],
) -> Vec<RenderLine> {
    let mut rows: Vec<(String, Vec<String>)> = Vec::new();
    for job in jobs {
        rows.push((
            format!(
                "job {}: {}",
                lofi_types::short_id(job.id),
                prim::one_line(&job.cmd)
            ),
            vec![prim::fmt_duration_short(Duration::from_millis(
                job.duration_ms,
            ))],
        ));
    }
    for run in subagents {
        // The message goes first when the row runs out of room, so the model,
        // the spend, and the clock survive it.
        let mut columns = Vec::new();
        if let Some(model) = &run.model {
            columns.push(model.clone());
        }
        if let Some(cost) = run.cost {
            columns.push(fmt_cost(cost));
        }
        columns.push(prim::fmt_duration_short(Duration::from_millis(
            run.elapsed_ms,
        )));
        // A run that has not named anything yet shows the task it was given,
        // so the row keeps a message while the child thinks. An empty label
        // would leave a dangling colon.
        let doing = if run.activity.is_empty() {
            run.task.trim()
        } else {
            run.activity.as_str()
        };
        let label = if doing.is_empty() {
            format!("subagent {}", run.id)
        } else {
            format!("subagent {}: {doing}", run.id)
        };
        rows.push((label, columns));
    }
    for timer in timers {
        rows.push((timer_label(timer), vec![timer_remaining(timer)]));
    }
    rows.iter()
        .map(|(label, columns)| work_row(cx, label, columns))
        .collect()
}

/// A running item's row at `cx.width`: the label on the left, the columns it
/// still affords on the right edge, and at least one blank cell between them.
fn work_row(cx: &Cx, label: &str, columns: &[String]) -> RenderLine {
    let t = cx.theme;
    // The marker sits in the block's left column, in line with the queued
    // prompts' bars and the working spinner.
    let body_w = cx.width.saturating_sub(2);
    let (label, columns) = fit(label, columns, body_w);
    let right = columns.join(" ");
    let gap = body_w
        .saturating_sub(prim::width(&label) + prim::width(&right))
        .max(usize::from(!right.is_empty()));
    let mut content = vec![Span::styled(label, t.muted_style())];
    if !right.is_empty() {
        content.push(Span::raw(" ".repeat(gap)));
        content.push(Span::styled(right, t.subtle_style()));
    }
    prim::rline(vec![Span::styled("┆ ", Style::new().fg(t.info))], content)
}

/// The label as much as `body_w` affords, and the columns worth keeping. A
/// column drops only when the label could not say anything with it there:
/// the message first, then the model, then the spend. The clock is the
/// column a narrow row keeps longest.
fn fit(label: &str, columns: &[String], body_w: usize) -> (String, Vec<String>) {
    // Room the label needs before a column costs it more than it is worth.
    const MIN_LABEL: usize = 12;
    // The clock alone is still worth a stub of a label.
    const MIN_STUB: usize = 6;
    for drop in 0..columns.len() {
        let kept = &columns[drop..];
        let right_w: usize = kept.iter().map(|c| prim::width(c)).sum::<usize>() + kept.len() - 1;
        let room = body_w.saturating_sub(right_w + 1);
        let need = if drop + 1 == columns.len() {
            MIN_STUB
        } else {
            MIN_LABEL
        };
        if room >= need {
            return (clip(label, room), kept.to_vec());
        }
    }
    (clip(label, body_w), Vec::new())
}

/// Cut `text` to `room` cells, marking the cut with an ellipsis.
fn clip(text: &str, room: usize) -> String {
    if prim::width(text) <= room {
        return text.to_string();
    }
    if room == 0 {
        return String::new();
    }
    let mut out = prim::truncate(text, room - 1);
    out.push('\u{2026}');
    out
}

/// What a pending timer waits for: its schedule, in the words the agent used.
fn timer_label(timer: &lofi_core::TimerInfo) -> String {
    let id = lofi_types::short_id(timer.id);
    if let Some(every) = timer.every {
        return format!(
            "timer {id}: notify every {}",
            prim::fmt_duration_short(every)
        );
    }
    let due = chrono::DateTime::<chrono::Local>::from(timer.due);
    format!("timer {id}: notify at {}", due.format("%Y-%m-%d %H:%M"))
}

/// How long until a timer fires, or `due` once the poll is late.
fn timer_remaining(timer: &lofi_core::TimerInfo) -> String {
    match timer.due.duration_since(std::time::SystemTime::now()) {
        Ok(left) if left.as_secs() > 0 => format!("{} remaining", prim::fmt_duration_short(left)),
        _ => "due".to_string(),
    }
}

pub(super) struct QueuedPromptLine<'a> {
    pub(super) text: &'a str,
    pub(super) kind: lofi_types::PromptKind,
}

impl Component for QueuedPromptLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = t.muted_style();
        let (bar, body_style) = match self.kind {
            lofi_types::PromptKind::Notice => ("▎ ", t.annotation_style()),
            lofi_types::PromptKind::User | lofi_types::PromptKind::Task => ("▌ ", t.muted_style()),
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
        let command_style = t.text_style();
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
        let deco = vec![Span::raw("  "), Span::styled("  ", t.subtle_style())];
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
        let content = vec![Span::styled(status, t.text_style())];
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
                Span::styled("└ ", t.subtle_style()),
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
            vec![Span::raw("  "), Span::styled("◇ ", t.subtle_style())],
            vec![
                Span::styled(format!("Done in {dur} with "), t.subtle_style()),
                Span::styled(self.label.clone(), t.muted_style()),
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
            vec![Span::raw("  "), Span::styled("◇ ", t.subtle_style())],
            vec![Span::styled(body, t.muted_style())],
            vec![],
        );
        vec![marker]
    }
}

// `active_indicator` is re-exported for the working indicator in the chrome;
// keep the import here so the component module can surface it if needed.
#[allow(unused_imports)]
use active_indicator as _;
