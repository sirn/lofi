#![allow(clippy::needless_lifetimes)]

use ratatui::text::{Line, Text};

use crate::tui::{App, Block, Turn};

use super::component::{Component, Cx, Stack};
use super::prim::{self, RenderLine};

mod detail;
mod exec;
mod message;
mod native;
mod status;
#[cfg(test)]
mod tests;

pub(in crate::tui) use native::cache_native_preview;
pub(crate) use native::compact_native_previews;

use exec::ExecBlock;
use message::{AssistantText, NoticeMessage, Thinking, UserMessage};
use status::{
    CompactionLine, ErrorLine, QueuedPromptLine, RunningJobLine, RunningSubagentLine, ToolLine,
    TurnCancelled, TurnEnd, TurnFailed, UserShellLine, WorkingLine,
};

pub(super) struct RenderWindow {
    range: std::ops::Range<usize>,
    total: usize,
    lines: Vec<RenderLine>,
}

impl RenderWindow {
    pub(super) fn new(range: std::ops::Range<usize>) -> Self {
        Self {
            lines: Vec::with_capacity(range.end.saturating_sub(range.start).min(256)),
            range,
            total: 0,
        }
    }

    pub(super) fn push(&mut self, line: RenderLine) {
        if self.range.contains(&self.total) {
            self.lines.push(line);
        }
        self.total = self.total.saturating_add(1);
    }

    pub(super) fn extend(&mut self, lines: impl IntoIterator<Item = RenderLine>) {
        for line in lines {
            self.push(line);
        }
    }

    pub(super) fn append_component(&mut self, component: &dyn Component, cx: &Cx) {
        let height = component.height(cx);
        let end = self.total.saturating_add(height);
        if end > self.range.start && self.total < self.range.end {
            let start = self.range.start.saturating_sub(self.total);
            let stop = self.range.end.saturating_sub(self.total).min(height);
            self.lines.extend(component.lines_window(cx, start..stop));
        }
        self.total = end;
    }
}

#[allow(dead_code)] // reference renderer; used as a test oracle (view.rs uses the cached viewport path)
pub fn render_turns(app: &App, width: u16) -> Text<'static> {
    let theme = app.theme;
    let w = width as usize;
    let last = app.transcript.turns.len().saturating_sub(1);
    let running = app.run_active();
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, turn) in app.transcript.turns.iter().enumerate() {
        let cx = Cx {
            app,
            theme,
            width: w,
            active_turn: running && i == last,
        };
        if i > 0 {
            out.push(prim::blank());
        }
        out.extend(render_turn_lines(&cx, turn).into_iter().map(|rl| rl.line));
    }
    let tail_cx = Cx {
        app,
        theme,
        width: w,
        active_turn: running,
    };
    let has_prior = !out.is_empty();
    let jobs = app.running_jobs();
    let subagents = app.running_subagents();
    out.extend(
        render_transcript_tail(&tail_cx, has_prior, &jobs, &subagents)
            .into_iter()
            .map(|rl| rl.line),
    );
    Text::from(out)
}

pub fn render_transcript_tail(
    cx: &Cx,
    has_prior: bool,
    jobs: &[lofi_core::JobInfo],
    subagents: &[lofi_core::SubagentRunning],
) -> Vec<RenderLine> {
    let mut out = Vec::new();
    let mut any = has_prior;
    if cx.active_turn {
        if any {
            out.push(prim::rblank());
        }
        out.extend(WorkingLine.lines(cx));
        any = true;
    }
    // Queued prompts render above running jobs: a queued prompt moves up
    // into the transcript when consumed, while a completed job's notice
    // rises out of the running block below.
    for queued in &cx.app.prompt_queue {
        if any {
            out.push(prim::rblank());
        }
        out.extend(
            QueuedPromptLine {
                text: &queued.text,
                kind: queued.kind,
            }
            .lines(cx),
        );
        any = true;
    }
    for run in subagents {
        if any {
            out.push(prim::rblank());
        }
        out.extend(RunningSubagentLine { run }.lines(cx));
        any = true;
    }
    for job in jobs {
        if any {
            out.push(prim::rblank());
        }
        out.extend(RunningJobLine { job }.lines(cx));
        any = true;
    }
    out
}

pub fn render_turn_lines(cx: &Cx, turn: &Turn) -> Vec<RenderLine> {
    turn_stack(turn).lines(cx)
}

pub fn render_turn_height(cx: &Cx, turn: &Turn) -> usize {
    turn_stack(turn).height(cx)
}

pub fn render_turn_window(cx: &Cx, turn: &Turn, range: std::ops::Range<usize>) -> Vec<RenderLine> {
    turn_stack(turn).lines_window(cx, range)
}

fn turn_stack(turn: &Turn) -> Stack<'_> {
    let mut stack = Stack::new();
    if !turn.prompt.is_empty() {
        match turn.kind {
            lofi_types::PromptKind::User => stack.push(UserMessage {
                prompt: &turn.prompt,
            }),
            lofi_types::PromptKind::Notice => stack.push(NoticeMessage {
                prompt: &turn.prompt,
            }),
        }
    }
    for block in &turn.blocks {
        match block {
            Block::Text(text) => stack.push(AssistantText { text }),
            Block::Thinking(tb) => stack.push(Thinking { block: tb }),
            Block::Tool(tool) => {
                if tool.name == "exec" {
                    stack.push(ExecBlock { tool });
                } else {
                    stack.push(ToolLine { tool });
                }
            }
            Block::UserShell {
                id,
                command,
                output,
                exit_code,
                signal,
                duration,
                truncated,
                cancelled,
                running,
                exclude_from_context,
            } => stack.push(UserShellLine {
                id: *id,
                command,
                output,
                exit_code: *exit_code,
                signal: *signal,
                duration: *duration,
                truncated: *truncated,
                cancelled: *cancelled,
                running: *running,
                exclude_from_context: *exclude_from_context,
            }),
            Block::Error(msg) => stack.push(ErrorLine { msg }),
            Block::TurnEnd { label, elapsed } => {
                stack.push(TurnEnd {
                    label: label.clone(),
                    elapsed: *elapsed,
                });
            }
            Block::TurnFailed {
                label,
                elapsed,
                error,
            } => {
                let has_fatal = turn.blocks.iter().any(|b| matches!(b, Block::Error(_)));
                let error = if has_fatal {
                    String::new()
                } else {
                    error.clone()
                };
                stack.push(TurnFailed {
                    label: label.clone(),
                    elapsed: *elapsed,
                    error,
                });
            }
            Block::TurnCancelled { label, elapsed } => {
                stack.push(TurnCancelled {
                    label: label.clone(),
                    elapsed: *elapsed,
                });
            }
            Block::Compaction {
                summarized, kept, ..
            } => {
                stack.push(CompactionLine {
                    summarized: *summarized,
                    kept: *kept,
                });
            }
        }
    }
    stack
}
