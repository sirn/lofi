#![allow(clippy::needless_lifetimes)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::tui::theme::{active_indicator, agent_indicator, user_indicator};
use crate::tui::{
    App, Block, DetailKey, NativePreview, NativeTool, ResultAvailability, ThinkingBlock, ToolCall,
    Turn, DETAIL_VIEW_ROWS,
};

use super::component::{Component, Cx, Stack};
use super::markdown::{markdown_body_height, render_markdown_body, render_markdown_body_window};
use super::prim::{self, RawLine, RenderLine};

const PREVIEW_LINES: usize = 3;

struct DetailVisualRow {
    text: String,
    raw: RawLine,
}

fn detail_visual_rows(lines: &[String], inner: usize) -> Vec<DetailVisualRow> {
    let mut rows = Vec::new();
    for line in lines {
        let source: Arc<str> = Arc::from(line.as_str());
        let indent_len = line
            .bytes()
            .take_while(|&byte| byte == b' ' || byte == b'\t')
            .count();
        let indent_chars = line[..indent_len].chars().count();
        let body = &line[indent_len..];
        let body_offsets = std::iter::once(0)
            .chain(body.char_indices().map(|(byte, c)| byte + c.len_utf8()))
            .collect::<Vec<_>>();
        let segments = prim::wrap_pre(line, inner.max(1));
        let mut consumed = 0usize;
        for (index, segment) in segments.into_iter().enumerate() {
            let body_chars = segment.chars().count().saturating_sub(indent_chars);
            let map = (0..=body_chars)
                .map(|offset| {
                    indent_len
                        + body_offsets
                            .get(consumed + offset)
                            .copied()
                            .unwrap_or(body.len())
                })
                .collect();
            rows.push(DetailVisualRow {
                text: segment,
                raw: RawLine::new(source.clone(), map, index == 0),
            });
            consumed += body_chars;
        }
    }
    rows
}

fn detail_row_count(lines: &[String], width: usize, deco: &[Span<'static>]) -> usize {
    let inner = width.saturating_sub(prim::span_width(deco) + 2);
    detail_visual_rows(lines, inner).len()
}

fn detail_box(
    lines: &[String],
    key: &DetailKey,
    tail: bool,
    cx: &Cx,
    deco: &[Span<'static>],
    style: Style,
) -> Vec<RenderLine> {
    let expanded = cx.app.expanded_details.get(key);
    let Some(expanded) = expanded else {
        return Vec::new();
    };
    // Reserve one cell after the mini-scrollbar as outer right margin.
    let inner = cx.width.saturating_sub(prim::span_width(deco) + 3);
    let rows = detail_visual_rows(lines, inner);
    let total = rows.len();
    let target =
        |line: RenderLine, row: usize| line.with_detail_row((*key).clone(), total, tail, row);
    let max = total.saturating_sub(DETAIL_VIEW_ROWS);
    let start = expanded
        .scroll
        .unwrap_or(if tail { max } else { 0 })
        .min(max);
    let visible_rows = total.min(DETAIL_VIEW_ROWS);
    let visible = &rows[start..total.min(start + visible_rows)];
    let raised = style.bg(cx.theme.surface);
    let focused = cx
        .app
        .detail_focus
        .as_ref()
        .is_some_and(|focus| focus.key == *key);
    let track = Style::new().fg(cx.theme.subtle).bg(cx.theme.surface);
    let thumb = Style::new()
        .fg(if focused {
            cx.theme.user
        } else {
            cx.theme.muted
        })
        .bg(cx.theme.surface);
    let thumb_row = (start * visible_rows.saturating_sub(1)).checked_div(max);
    let mut out = Vec::with_capacity(visible_rows);
    for (row, visual) in visible.iter().enumerate() {
        let used = prim::width(&visual.text);
        let mut line_deco = deco.to_vec();
        line_deco.push(Span::styled(" ", raised));
        let detail_span = prim::render(
            line_deco,
            vec![Span::styled(visual.text.clone(), raised)],
            vec![
                Span::styled(" ".repeat(inner.saturating_sub(used)), raised),
                if max == 0 {
                    Span::styled(" ", raised)
                } else {
                    Span::styled(
                        if thumb_row == Some(row) { "┃" } else { "│" },
                        if thumb_row == Some(row) { thumb } else { track },
                    )
                },
                Span::raw(" "),
            ],
        )
        .with_raw(visual.raw.clone());
        out.push(target(detail_span, start + row));
    }
    out
}

struct RenderWindow {
    range: std::ops::Range<usize>,
    total: usize,
    lines: Vec<RenderLine>,
}

impl RenderWindow {
    fn new(range: std::ops::Range<usize>) -> Self {
        Self {
            lines: Vec::with_capacity(range.end.saturating_sub(range.start).min(256)),
            range,
            total: 0,
        }
    }

    fn push(&mut self, line: RenderLine) {
        if self.range.contains(&self.total) {
            self.lines.push(line);
        }
        self.total = self.total.saturating_add(1);
    }

    fn extend(&mut self, lines: impl IntoIterator<Item = RenderLine>) {
        for line in lines {
            self.push(line);
        }
    }

    fn append_component(&mut self, component: &dyn Component, cx: &Cx) {
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
    Text::from(out)
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

struct UserMessage<'a> {
    prompt: &'a str,
}

impl Component for UserMessage<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        let mark = Style::new().fg(user_indicator(t));
        render_markdown_body(
            self.prompt.trim(),
            t,
            w,
            content_w,
            Style::new().fg(t.fg),
            move |_| vec![Span::styled("▌ ", mark)],
        )
    }

    fn height(&self, cx: &Cx) -> usize {
        markdown_body_height(self.prompt.trim(), cx.width.saturating_sub(2))
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = Style::new().fg(user_indicator(t));
        render_markdown_body_window(
            self.prompt.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            Style::new().fg(t.fg),
            move |_| vec![Span::styled("▌ ", mark)],
            range,
        )
    }
}

struct NoticeMessage<'a> {
    prompt: &'a str,
}

impl Component for NoticeMessage<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        // Notices (job wake-ups, future automation) share the bar marker
        // and row rhythm of user/agent messages but stay de-emphasized so
        // an app-injected prompt never competes visually with typed input:
        // muted color for marker and body, italic body.
        let mark = Style::new().fg(t.muted);
        let body_style = Style::new().fg(t.muted).add_modifier(Modifier::ITALIC);
        render_markdown_body(self.prompt.trim(), t, w, content_w, body_style, move |_| {
            vec![Span::styled("▌ ", mark)]
        })
    }

    fn height(&self, cx: &Cx) -> usize {
        markdown_body_height(self.prompt.trim(), cx.width.saturating_sub(2))
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = Style::new().fg(t.muted);
        let body_style = Style::new().fg(t.muted).add_modifier(Modifier::ITALIC);
        render_markdown_body_window(
            self.prompt.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            body_style,
            move |_| vec![Span::styled("▌ ", mark)],
            range,
        )
    }
}

struct AssistantText<'a> {
    text: &'a str,
}

impl Component for AssistantText<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        let text = self.text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let mark = Style::new().fg(agent_indicator(t));
        render_markdown_body(text, t, w, content_w, Style::new().fg(t.fg), move |_| {
            vec![Span::styled("▌ ", mark)]
        })
    }

    fn height(&self, cx: &Cx) -> usize {
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        let text = self.text.trim();
        if text.is_empty() {
            return 0;
        }
        markdown_body_height(text, content_w)
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = Style::new().fg(agent_indicator(t));
        render_markdown_body_window(
            self.text.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            Style::new().fg(t.fg),
            move |_| vec![Span::styled("▌ ", mark)],
            range,
        )
    }
}

struct Thinking<'a> {
    block: &'a ThinkingBlock,
}

impl Component for Thinking<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let body = Style::new().fg(t.muted).add_modifier(Modifier::ITALIC);
        let content_w = cx.width.saturating_sub(2);
        let text = trim_reasoning_summary(&self.block.text);
        let working = self.block.elapsed.is_none() && cx.active_turn;
        if text.is_empty() && !working {
            return Vec::new();
        }
        let mut out = render_markdown_body(&text, t, cx.width, content_w, body, |_| {
            vec![Span::raw("  ")]
        });
        if working {
            if !out.is_empty() {
                out.push(prim::rblank());
            }
            out.push(prim::rline(
                vec![Span::raw("  ")],
                vec![Span::styled("Thinking...", body)],
            ));
        } else if let Some(d) = self.block.elapsed {
            if !d.is_zero() {
                if !out.is_empty() {
                    out.push(prim::rblank());
                }
                let thought_lead = vec![
                    Span::raw("  "),
                    Span::styled("◇ ", Style::new().fg(t.subtle)),
                ];
                out.push(prim::rline(
                    thought_lead,
                    vec![Span::styled(
                        format!("Thought for {}", prim::fmt_duration(d)),
                        Style::new().fg(t.subtle),
                    )],
                ));
            }
        }
        out
    }

    fn height(&self, cx: &Cx) -> usize {
        let text = trim_reasoning_summary(&self.block.text);
        let working = self.block.elapsed.is_none() && cx.active_turn;
        if text.is_empty() && !working {
            return 0;
        }
        let content_w = cx.width.saturating_sub(2);
        let md = markdown_body_height(&text, content_w);
        if working {
            md + if md > 0 { 2 } else { 1 }
        } else if let Some(d) = self.block.elapsed {
            if d.is_zero() {
                md
            } else {
                md + if md > 0 { 2 } else { 1 }
            }
        } else {
            md
        }
    }
}

/// Remove standalone empty reasoning-summary parts from display. `OpenAI` uses
/// `<!-- -->` as a placeholder, sometimes after a bold status header. Keep
/// literal comments that are part of otherwise non-empty summary content.
fn trim_reasoning_summary(text: &str) -> String {
    text.split("\n\n")
        .filter_map(|part| {
            let part = part.trim();
            if part.is_empty() {
                return None;
            }
            let header_end = part.strip_prefix("**").and_then(|after_open| {
                after_open
                    .find("**")
                    .and_then(|close| (close > 0).then_some(close + 4))
            });
            let body = header_end.map_or(part, |header_end| &part[header_end..]);
            (body.trim() != "<!-- -->").then_some(part)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

struct ExecBlock<'a> {
    tool: &'a ToolCall,
}

impl Component for ExecBlock<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        self.render_window(cx, 0..usize::MAX).lines
    }

    fn height(&self, cx: &Cx) -> usize {
        self.render_window(cx, 0..0).total
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        self.render_window(cx, range).lines
    }
}

impl ExecBlock<'_> {
    fn render_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> RenderWindow {
        let t = cx.theme;
        let mut out = RenderWindow::new(range);

        let header = match &self.tool.label {
            Some(l) if !l.is_empty() => format!("Exec {l}"),
            _ => "Exec".to_string(),
        };
        let status_color = if !self.tool.done {
            active_indicator(t)
        } else if self.tool.is_error {
            t.error
        } else {
            t.success
        };
        let key = DetailKey::Exec(self.tool.detail_id);
        let code_deco = vec![
            Span::raw("  "),
            Span::styled("│ ", Style::new().fg(t.subtle)),
        ];
        let has_detail = !self.tool.input.is_empty();
        let code = cx.app.expanded_details.contains_key(&key).then(|| {
            self.tool
                .input
                .trim_end_matches('\n')
                .split('\n')
                .map(str::to_string)
                .collect::<Vec<_>>()
        });
        let total = code
            .as_deref()
            .map_or(0, |code| detail_row_count(code, cx.width, &code_deco));
        let content = vec![Span::styled(
            header,
            Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
        )];
        let mut header = prim::rline(
            vec![
                Span::raw("  "),
                Span::styled("· ", Style::new().fg(status_color)),
            ],
            content,
        );
        if has_detail {
            header = header.with_detail(key.clone(), total, false);
        }
        out.push(header);
        if let Some(code) = code {
            out.extend(detail_box(
                &code,
                &key,
                false,
                cx,
                &code_deco,
                Style::new().fg(t.fg),
            ));
        }

        let n_total = self.tool.native.len();
        for (idx, nt) in self.tool.native.iter().enumerate() {
            let is_last = idx + 1 == n_total && !self.tool.done;
            out.append_component(
                &ExecBlockBranch {
                    parent: self.tool.detail_id,
                    nt,
                    is_last,
                },
                cx,
            );
        }

        if self.tool.done {
            out.extend(exec_result_lines(self.tool, cx));
        }

        out
    }
}
fn exec_result_lines(tool: &ToolCall, cx: &Cx) -> Vec<RenderLine> {
    let t = cx.theme;
    let (icon, label, fg_color) = if tool.is_error {
        ("✗", "Failed", t.error)
    } else {
        ("✓", "Succeed", t.success)
    };
    let took = tool
        .elapsed
        .filter(|d| !d.is_zero())
        .map(|d| format!(", took {}", prim::fmt_duration(d)))
        .unwrap_or_default();
    let content = vec![Span::styled(
        format!("{label}{took}"),
        Style::new().fg(t.fg),
    )];
    let display = tool
        .result
        .as_deref()
        .map(|result| lofi_core::exec_result_display(result, tool.is_error))
        .unwrap_or_default();
    let lines = split_lines(&display);
    let key = DetailKey::ExecResult(tool.detail_id);
    let deco = vec![Span::raw("  "), Span::raw("  "), Span::raw("  ")];
    let total = detail_row_count(&lines, cx.width, &deco);
    let has_detail =
        matches!(tool.result_availability, ResultAvailability::Available) || !display.is_empty();
    let mut header = prim::rline(
        vec![
            Span::raw("  "),
            Span::styled("└ ", Style::new().fg(t.subtle)),
            Span::styled(format!("{icon} "), Style::new().fg(fg_color)),
        ],
        content,
    );
    if has_detail {
        header = header.with_detail(key.clone(), total, true);
    }
    let mut out = vec![header];
    if has_detail {
        out.extend(detail_box(
            &lines,
            &key,
            true,
            cx,
            &deco,
            Style::new().fg(if tool.is_error { t.error } else { t.muted }),
        ));
    }
    out
}

struct ExecBlockBranch<'a> {
    parent: u64,
    nt: &'a NativeTool,
    is_last: bool,
}

struct NativeBody {
    lines: Vec<String>,
    numbered: bool,
    start_line: usize,
    is_diff: bool,
    notice: Option<String>,
}

fn native_body(nt: &NativeTool) -> NativeBody {
    let name = nt.name.as_str();
    let raw = nt.result.as_deref().unwrap_or("");
    if nt.is_error {
        let display = if name == "bash" {
            serde_json::from_str::<serde_json::Value>(raw)
                .ok()
                .and_then(|value| value.get("output")?.as_str().map(str::to_string))
                .unwrap_or_else(|| raw.to_string())
        } else {
            raw.to_string()
        };
        return NativeBody {
            lines: split_lines(&display),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        };
    }
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        // Non-JSON result (e.g. a plain error string or an older
        // persisted string): show it verbatim rather than dropping it.
        Err(_) => {
            return NativeBody {
                lines: split_lines(raw),
                numbered: matches!(name, "read" | "view" | "bash_read"),
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("");
    let b = |k: &str| {
        v.get(k)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_u64).unwrap_or(0) as usize;
    match name {
        "write" => NativeBody {
            lines: split_lines(s("content")),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "edit" => NativeBody {
            lines: edit_diff(s("old"), s("new")),
            numbered: false,
            start_line: 1,
            is_diff: true,
            notice: None,
        },
        "bash" => NativeBody {
            lines: split_lines(s("output")),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
        "read" | "view" | "bash_read" => {
            let start = n("start_line").max(1);
            let total = n("total_lines");
            let lines = split_lines(s("content"));
            let notice = b("truncated").then_some(format!(
                "(showing {start}-{} of {total}; use offset={next} to continue)",
                start + lines.len().saturating_sub(1),
                next = start + lines.len()
            ));
            NativeBody {
                lines,
                numbered: true,
                start_line: start,
                is_diff: false,
                notice,
            }
        }
        "ls" => {
            let lines = v.get("entries").and_then(|x| x.as_array()).map_or_else(
                || split_lines(raw),
                |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                },
            );
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "find" => {
            let lines = v.get("matches").and_then(|x| x.as_array()).map_or_else(
                || split_lines(raw),
                |a| {
                    a.iter()
                        .filter_map(|e| e.as_str().map(String::from))
                        .collect()
                },
            );
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "agent" => {
            let text = s("text");
            let model = s("model");
            let thinking = s("thinking");
            let rounds = n("rounds");
            let duration_ms = n("durationMs");
            let cost = v
                .get("cost")
                .and_then(serde_json::Value::as_f64)
                .unwrap_or(0.0);
            let notice = (!model.is_empty()).then(|| {
                format!(
                    "({model}, {thinking}, {rounds} rounds, {}, ${cost:.4})",
                    prim::fmt_duration(std::time::Duration::from_millis(duration_ms as u64))
                )
            });
            NativeBody {
                lines: split_lines(text),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice,
            }
        }
        "grep" => {
            let mut lines = Vec::new();
            if let Some(arr) = v.get("matches").and_then(|x| x.as_array()) {
                for m in arr {
                    let file = m.get("file").and_then(|x| x.as_str()).unwrap_or("");
                    let line = m
                        .get("line")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let content = m.get("content").and_then(|x| x.as_str()).unwrap_or("");
                    lines.push(format!("{file}:{line}:{content}"));
                }
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: b("truncated").then_some("(truncated)".into()),
            }
        }
        "jobRead" => {
            let output = s("output");
            let cursor = v
                .get("cursor")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let total = v
                .get("totalBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let done = b("done");
            let notice = if done {
                Some(format!("(end of log; {total} bytes total)"))
            } else if total > 0 {
                Some(format!("(read {cursor} of {total} bytes)"))
            } else {
                None
            };
            NativeBody {
                lines: split_lines(output),
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice,
            }
        }
        "jobSpawn" | "jobStatus" | "jobKill" | "jobNotify" => {
            let mut lines: Vec<String> = Vec::new();
            let state = s("state");
            if !state.is_empty() {
                lines.push(format!("state: {state}"));
            }
            let mut stats: Vec<String> = Vec::new();
            if let Some(code) = v.get("exitCode").and_then(serde_json::Value::as_i64) {
                stats.push(format!("exit {code}"));
            }
            if let Some(sig) = v.get("signal").and_then(serde_json::Value::as_i64) {
                stats.push(format!("signal {sig}"));
            }
            if let Some(ms) = v.get("durationMs").and_then(serde_json::Value::as_u64) {
                stats.push(format!(
                    "duration {}",
                    prim::fmt_duration(std::time::Duration::from_millis(ms))
                ));
            }
            if !stats.is_empty() {
                lines.push(stats.join("  "));
            }
            let mut notify_bits: Vec<String> = Vec::new();
            if v.get("notify").is_some() {
                notify_bits.push(format!("notify: {}", b("notify")));
            }
            if let Some(iv) = v
                .get("notifyIntervalMs")
                .and_then(serde_json::Value::as_u64)
            {
                notify_bits.push(format!(
                    "interval {}",
                    prim::fmt_duration(std::time::Duration::from_millis(iv))
                ));
            }
            if v.get("notifyChanged").is_some() {
                notify_bits.push(format!("changed {}", b("notifyChanged")));
            }
            if !notify_bits.is_empty() {
                lines.push(notify_bits.join("  "));
            }
            let log_path = s("logPath");
            if !log_path.is_empty() {
                lines.push(format!("log: {log_path}"));
            }
            NativeBody {
                lines,
                numbered: false,
                start_line: 1,
                is_diff: false,
                notice: None,
            }
        }
        _ => NativeBody {
            lines: split_lines(raw),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
    }
}

pub(in crate::tui) fn cache_native_preview(native: &mut NativeTool) {
    let Some(raw) = native.result.as_deref().filter(|result| !result.is_empty()) else {
        return;
    };
    let header_suffix = native_header_suffix(&native.name, Some(raw));
    let encoded_field = match native.name.as_str() {
        "bash" => Some("output"),
        "write" => Some("content"),
        _ => None,
    };
    let encoded = encoded_field.and_then(|field| json_string_field(raw, field));
    let body = encoded.is_none().then(|| native_body(native));
    let total_lines = encoded.map_or_else(
        || body.as_ref().map_or(0, |body| body.lines.len()),
        encoded_json_line_count,
    );
    let range = native_preview_range(&native.name, total_lines);
    let mut lines = Vec::with_capacity(range.len());
    if let Some(encoded) = encoded {
        for_each_encoded_json_line(encoded, range.clone(), |_, line| lines.push(line));
    } else if let Some(body) = &body {
        lines.extend(body.lines[range.clone()].iter().cloned());
    }
    native.preview = Some(Box::new(NativePreview {
        header_suffix,
        lines,
        total_lines,
        preview_start: range.start,
        numbered: body.as_ref().is_some_and(|body| body.numbered),
        start_line: body.as_ref().map_or(1, |body| body.start_line),
        is_diff: body.as_ref().is_some_and(|body| body.is_diff),
        notice: body.and_then(|body| body.notice),
    }));
}

pub(crate) fn compact_native_previews(turn: &mut Turn) {
    for block in &mut turn.blocks {
        let Block::Tool(tool) = block else { continue };
        for native in &mut tool.native {
            if native.is_error || native.result.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            cache_native_preview(native);
            native.result = None;
        }
    }
}

fn native_preview_range(name: &str, total: usize) -> std::ops::Range<usize> {
    let shown = total.min(PREVIEW_LINES);
    let start = if name == "bash" {
        total.saturating_sub(shown)
    } else {
        0
    };
    start..start + shown
}

fn split_lines(s: &str) -> Vec<String> {
    s.trim_end_matches('\n')
        .split('\n')
        .map(String::from)
        .collect()
}

/// Collapse JSON tool args into a short header label for known tools so
/// the `Tool <name> <args>` line scans like a sentence instead of dumping
/// the request envelope. Returns `None` for tools we have no tailored
/// view for; the caller falls back to the verbatim args.
///
/// Only shapes the surface actually emits are recognised (every job op
/// takes an `id`; `jobSpawn` takes a command). Anything else returns
/// `None`.
fn summarize_tool_args(name: &str, args: &str) -> Option<String> {
    if args.is_empty() {
        return Some(String::new());
    }
    let id = json_string_field(args, "id");
    match name {
        "jobSpawn" => json_string_field(args, "cmd").map(|cmd| truncate_args_display(cmd, 60)),
        "jobStatus" | "jobRead" | "jobKill" | "jobNotify" => id.map(|s| format!("job {s}")),
        _ => None,
    }
}

/// Cap an args-rendered text cell at `width` chars (multi-byte safe),
/// replacing the overflow with U+2026.
fn truncate_args_display(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut s: String = text.chars().take(width.saturating_sub(1)).collect();
    s.push('\u{2026}');
    s
}

fn json_string_field<'a>(raw: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let bytes = raw.as_bytes();
    let mut i = key + needle.len();
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    i = start;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i = i.saturating_add(2),
            b'"' => return Some(&raw[start..i]),
            _ => i += 1,
        }
    }
    None
}

fn json_u64_field(raw: &str, field: &str) -> Option<u64> {
    let needle = format!("\"{field}\"");
    let key = raw.find(&needle)?;
    let mut rest = raw.get(key + needle.len()..)?.trim_start();
    rest = rest.strip_prefix(':')?.trim_start();
    let end = rest.bytes().take_while(u8::is_ascii_digit).count();
    (end > 0).then(|| rest[..end].parse().ok()).flatten()
}

fn encoded_json_line_count(encoded: &str) -> usize {
    let bytes = encoded.as_bytes();
    let mut i = 0usize;
    let mut breaks = 0usize;
    let mut trailing = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            if bytes[i + 1] == b'n' {
                breaks += 1;
                trailing += 1;
            } else {
                trailing = 0;
            }
            i += 2;
        } else {
            trailing = 0;
            i += 1;
        }
    }
    breaks.saturating_add(1).saturating_sub(trailing).max(1)
}

fn for_each_encoded_json_line(
    encoded: &str,
    range: std::ops::Range<usize>,
    mut f: impl FnMut(usize, String),
) {
    let wanted_end = range.end.min(encoded_json_line_count(encoded));
    if range.start >= wanted_end {
        return;
    }
    let bytes = encoded.as_bytes();
    let mut start = 0usize;
    let mut line = 0usize;
    let mut i = 0usize;
    while i <= bytes.len() && line < wanted_end {
        let at_break =
            i == bytes.len() || (bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'n');
        if at_break {
            if line >= range.start {
                let quoted = format!("\"{}\"", &encoded[start..i]);
                if let Ok(decoded) = serde_json::from_str::<String>(&quoted) {
                    f(line, decoded);
                }
            }
            line += 1;
            if i == bytes.len() {
                break;
            }
            i += 2;
            start = i;
        } else if bytes[i] == b'\\' {
            i = i.saturating_add(2);
        } else {
            i += 1;
        }
    }
}

fn native_header_suffix(name: &str, result: Option<&str>) -> Option<String> {
    let raw = result?;
    if raw.is_empty() {
        return None;
    }
    if matches!(name, "read" | "view" | "bash_read") {
        let start = json_u64_field(raw, "start_line")?.max(1);
        let total = json_u64_field(raw, "total_lines")?;
        let shown = json_string_field(raw, "content").map(encoded_json_line_count)? as u64;
        let end = start.saturating_add(shown.saturating_sub(1)).min(total);
        return Some(format!("(lines {start}-{end})"));
    }
    if name == "bash" {
        if let Some(ms) = json_u64_field(raw, "duration_ms") {
            return Some(format!(
                "(took {})",
                prim::fmt_duration(Duration::from_millis(ms))
            ));
        }
    }
    if name == "agent" {
        let model = json_string_field(raw, "model")?;
        let thinking = json_string_field(raw, "thinking").unwrap_or("off");
        let rounds = json_u64_field(raw, "rounds").unwrap_or(0);
        let duration = json_u64_field(raw, "durationMs").unwrap_or(0);
        return Some(format!(
            "({model}:{thinking}, {rounds} rounds, {})",
            prim::fmt_duration(Duration::from_millis(duration))
        ));
    }
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    match name {
        "read" | "view" | "bash_read" => {
            let start = v.get("start_line").and_then(serde_json::Value::as_u64)?;
            if start == 0 {
                return None;
            }
            let content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let count = content.split('\n').count();
            if count == 0 {
                return None;
            }
            let end = start + count as u64 - 1;
            if end <= start {
                return None;
            }
            Some(format!("(lines {start}-{end})"))
        }
        "bash" => {
            let ms = v.get("duration_ms").and_then(serde_json::Value::as_u64)?;
            if ms == 0 {
                return None;
            }
            let dur = std::time::Duration::from_millis(ms);
            Some(format!("(took {})", prim::fmt_duration(dur)))
        }
        _ => None,
    }
}

fn edit_diff(old: &str, new: &str) -> Vec<String> {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = Vec::new();
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Delete => '-',
            ChangeTag::Insert => '+',
            ChangeTag::Equal => ' ',
        };
        let val = change.value();
        let line = val.strip_suffix('\n').unwrap_or(val);
        out.push(format!("{prefix}{line}"));
    }
    out
}

impl Component for ExecBlockBranch<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        self.render_window(cx, 0..usize::MAX).lines
    }

    fn height(&self, cx: &Cx) -> usize {
        self.render_window(cx, 0..0).total
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        self.render_window(cx, range).lines
    }
}

impl ExecBlockBranch<'_> {
    fn render_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> RenderWindow {
        let t = cx.theme;
        let working = !self.nt.done && cx.active_turn;
        let exec_cont = if self.is_last { "  " } else { "│ " };
        let key = DetailKey::NativeTool {
            parent: self.parent,
            id: self.nt.id,
        };
        let expanded = cx.app.expanded_details.contains_key(&key);
        let preview = self.nt.preview.as_deref();
        let body = self
            .nt
            .result
            .as_deref()
            .filter(|result| !result.is_empty())
            .filter(|_| expanded || preview.is_none())
            .map(|_| native_body(self.nt));
        let mut lines = if let Some(body) = &body {
            body.lines.clone()
        } else if expanded {
            preview.map_or_else(Vec::new, |preview| preview.lines.clone())
        } else {
            Vec::new()
        };
        if let Some(body) = &body {
            if body.numbered {
                for (index, line) in lines.iter_mut().enumerate() {
                    *line = format!("{:>4} {line}", body.start_line + index);
                }
            }
            if let Some(notice) = &body.notice {
                lines.push(notice.clone());
            }
        } else if expanded {
            if let Some(notice) = preview.and_then(|preview| preview.notice.as_ref()) {
                lines.push(notice.clone());
            }
        }
        let total = body.as_ref().map_or_else(
            || preview.map_or(lines.len(), |preview| preview.total_lines),
            |body| body.lines.len(),
        );
        let detail_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::raw("  "),
        ];
        let visual_total = if expanded || preview.is_none() {
            detail_row_count(&lines, cx.width, &detail_deco)
        } else {
            total + usize::from(preview.is_some_and(|preview| preview.notice.is_some()))
        };
        let has_detail = if expanded || preview.is_none() {
            visual_total > 0 && lines.iter().any(|line| !line.is_empty())
        } else {
            preview.is_some_and(|preview| {
                preview.notice.is_some() || preview.lines.iter().any(|line| !line.is_empty())
            })
        };

        let mut content = vec![
            Span::styled("Tool ", Style::new().fg(t.muted)),
            Span::styled(self.nt.name.clone(), Style::new().fg(t.info)),
        ];
        let args_label = summarize_tool_args(&self.nt.name, &self.nt.args)
            .unwrap_or_else(|| self.nt.args.clone());
        if !args_label.is_empty() {
            content.push(Span::styled(
                format!(" {args_label}"),
                Style::new().fg(t.subtle),
            ));
        }
        let header_suffix = self
            .nt
            .preview
            .as_ref()
            .and_then(|preview| preview.header_suffix.as_deref())
            .map(str::to_owned)
            .or_else(|| native_header_suffix(&self.nt.name, self.nt.result.as_deref()));
        if let Some(note) = header_suffix {
            content.push(Span::styled(format!(" {note}"), Style::new().fg(t.subtle)));
        }
        let header_deco = vec![
            Span::raw("  "),
            Span::styled(
                if self.is_last { "└ " } else { "├ " },
                Style::new().fg(t.subtle),
            ),
            prim::status_icon(t, working, self.nt.is_error, cx.spinner()),
        ];
        let name_w = 5 + self.nt.name.chars().count();
        let cont_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::raw(" ".repeat(name_w)),
        ];
        let mut rendered = prim::rline_wrapped(header_deco, &cont_deco, content, cx.width);
        if self.nt.done && has_detail {
            for line in &mut rendered {
                line.detail = Some(super::prim::DetailTarget {
                    key: key.clone(),
                    total: visual_total.max(total),
                    tail: self.nt.name == "bash",
                    row: None,
                });
            }
        }
        rendered.extend(detail_box(
            &lines,
            &key,
            self.nt.name == "bash",
            cx,
            &detail_deco,
            Style::new().fg(if self.nt.is_error { t.error } else { t.muted }),
        ));
        let mut out = RenderWindow::new(range);
        out.extend(rendered);
        out
    }
}

struct ToolLine<'a> {
    tool: &'a ToolCall,
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
struct UserShellLine<'a> {
    id: u64,
    command: &'a str,
    output: &'a str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration: Duration,
    truncated: bool,
    cancelled: bool,
    running: bool,
    exclude_from_context: bool,
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

struct ErrorLine<'a> {
    msg: &'a str,
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
struct TurnEnd {
    label: String,
    elapsed: Duration,
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
struct TurnFailed {
    label: String,
    elapsed: Duration,
    error: String,
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
struct TurnCancelled {
    label: String,
    elapsed: Duration,
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
struct CompactionLine {
    summarized: usize,
    kept: usize,
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    use super::{native_body, native_preview_range, summarize_tool_args, trim_reasoning_summary};
    use crate::tui::NativeTool;

    #[test]
    fn bash_preview_keeps_tail_while_other_tools_keep_head() {
        assert_eq!(native_preview_range("bash", 10), 7..10);
        assert_eq!(native_preview_range("read", 10), 0..3);
        assert_eq!(native_preview_range("bash", 2), 0..2);
    }

    #[test]
    fn bash_error_body_renders_only_structured_output() {
        let raw = serde_json::json!({
            "ok": false,
            "output": "command not found\n",
            "code": 127
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "bash".to_string(),
            args: String::new(),
            result: Some(raw.clone()),
            preview: None,
            is_error: true,
            done: true,
        };
        assert_eq!(native_body(&tool).lines, vec!["command not found"]);
        assert_eq!(tool.result.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn summarize_tool_args_shortens_job_envelopes() {
        assert_eq!(
            summarize_tool_args("jobStatus", "{\"id\":\"1786294694788353138\"}").as_deref(),
            Some("job 1786294694788353138")
        );
        assert_eq!(
            summarize_tool_args("jobSpawn", "{\"cmd\":\"sleep 1\"}").as_deref(),
            Some("sleep 1")
        );
        assert_eq!(
            summarize_tool_args("jobSpawn", "{\"cmd\":\"a very long command line that exceeds the 60-character display budget\"}").as_deref(),
            Some("a very long command line that exceeds the 60-character disp…")
        );
        assert_eq!(
            summarize_tool_args("read", "{\"path\":\"/tmp/x\"}"),
            None,
            "read is not a job tool — keep verbatim"
        );
    }

    #[test]
    fn job_status_body_renders_compact_summary() {
        let raw = serde_json::json!({
            "id": "1786294694788353138",
            "state": "completed",
            "exitCode": 0,
            "signal": null,
            "durationMs": 30089,
            "logPath": "/tmp/lofi-job.log",
            "notify": true,
            "notifyChanged": true,
            "notifyIntervalMs": 5000,
            "ok": true,
            "command": "for i in 1 2; do echo $i; done",
            "directory": "/tmp",
            "timeoutMs": 30000
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "jobStatus".to_string(),
            args: String::new(),
            result: Some(raw),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert_eq!(body.lines[0], "state: completed");
        assert!(
            body.lines[1].contains("exit 0"),
            "exit code line: {:?}",
            body.lines[1]
        );
        assert!(
            body.lines[1].contains("duration"),
            "duration line: {:?}",
            body.lines[1]
        );
        let notify_line = body
            .lines
            .iter()
            .find(|l| l.starts_with("notify:"))
            .expect("notify line present");
        assert!(notify_line.contains("notify: true"), "{notify_line}");
        assert!(notify_line.contains("interval"), "{notify_line}");
        assert!(
            body.lines.last().is_some_and(|l| l.starts_with("log: ")),
            "log path last: {:?}",
            body.lines
        );
    }

    #[test]
    fn job_read_body_renders_output_with_tail_notice() {
        let raw = serde_json::json!({
            "id": "1",
            "state": "completed",
            "output": "beat-1\nbeat-2\n",
            "cursor": 14,
            "totalBytes": 71,
            "done": false,
            "ok": true
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "jobRead".to_string(),
            args: String::new(),
            result: Some(raw),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert_eq!(body.lines, vec!["beat-1", "beat-2"]);
        assert_eq!(body.notice.as_deref(), Some("(read 14 of 71 bytes)"));
    }

    #[test]
    fn job_read_body_marks_done_at_end() {
        let raw = serde_json::json!({
            "id": "1",
            "output": "done\n",
            "cursor": 71,
            "totalBytes": 71,
            "done": true,
            "ok": true
        })
        .to_string();
        let tool = NativeTool {
            id: 1,
            name: "jobRead".to_string(),
            args: String::new(),
            result: Some(raw),
            preview: None,
            is_error: false,
            done: true,
        };
        let body = native_body(&tool);
        assert_eq!(body.notice.as_deref(), Some("(end of log; 71 bytes total)"));
    }

    #[test]
    fn reasoning_summary_trims_empty_placeholder_parts() {
        let text = "**Checking**\n<!-- -->\n\nActual <!-- --> content.\n\n**Done**\nResult";
        assert_eq!(
            trim_reasoning_summary(text),
            "Actual <!-- --> content.\n\n**Done**\nResult"
        );
    }

    #[test]
    fn reasoning_summary_trims_plain_empty_placeholder() {
        assert_eq!(trim_reasoning_summary(" <!-- --> "), "");
    }
}
