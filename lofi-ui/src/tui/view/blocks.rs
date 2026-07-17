#![allow(clippy::needless_lifetimes)]

//! Concrete log components and the turn-log orchestrator.
//!
//! Each component renders to an owned block of lines via [`Component`]; the
//! primitives in [`super::prim`] supply padding, rails, and styled spans.
//! [`render_turns`] builds a [`Stack`] per turn and joins turns with blanks.

use std::time::Duration;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::tui::theme::{active_indicator, user_indicator, Theme};
use crate::tui::{App, Block, NativeTool, ThinkingBlock, ToolCall, Turn};

use super::component::{Component, Cx, Stack};
use super::prim;

/// Lines of preview/output shown before truncating with `… (N hidden)`.
const PREVIEW_LINES: usize = 3;

/// Build the whole turn log as a single [`Text`], turns separated by blanks.
#[allow(dead_code)]
pub fn render_turns(app: &App, width: u16) -> Text<'static> {
    let theme = app.theme;
    let w = width as usize;
    let last = app.turns.len().saturating_sub(1);
    let running = app.run_active();
    let mut out: Vec<Line<'static>> = Vec::new();
    for (i, turn) in app.turns.iter().enumerate() {
        let cx = Cx {
            app,
            theme,
            width: w,
            active_turn: running && i == last,
        };
        if i > 0 {
            out.push(prim::blank());
        }
        out.extend(render_turn_lines(&cx, turn));
    }
    Text::from(out)
}

/// Render a single turn's blocks to owned lines. Shared by the full-log
/// builder ([`render_turns`]) and the viewport renderer's per-turn cache:
/// the cache stores the result for frozen turns and rebuilds only the live
/// last turn each frame.
pub fn render_turn_lines(cx: &Cx, turn: &Turn) -> Vec<Line<'static>> {
    let mut stack = Stack::new();
    if !turn.prompt.is_empty() {
        stack.push(UserMessage { prompt: &turn.prompt });
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
            Block::Error(msg) => stack.push(ErrorLine { msg }),
            Block::TurnEnd { label, elapsed } => {
                stack.push(TurnEnd { label: label.clone(), elapsed: *elapsed });
            }
        }
    }
    stack.lines(cx)
}

// ── User message ─────────────────────────────────────────────────────────

/// A user message: the prompt soft-wrapped with a `❯` lead on the first
/// line and a 2-space indent on continuations. No background fill.
struct UserMessage<'a> {
    prompt: &'a str,
}

impl Component for UserMessage<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        let lead = Style::new().fg(user_indicator(t));
        let body = Style::new().fg(t.fg);
        let mut out = Vec::new();
        for (i, seg) in prim::wrap(self.prompt, content_w).iter().enumerate() {
            let prefix = if i == 0 {
                Span::styled("❯ ", lead)
            } else {
                Span::raw("  ")
            };
            out.push(Line::from(vec![
                prefix,
                Span::styled(seg.clone(), body),
            ]));
        }
        out
    }
}

// ── Assistant text ───────────────────────────────────────────────────────

/// Plain assistant text with a 2-space left margin; soft-wrapped lines all
/// carry the margin. Markdown-lite: headings bold, blockquotes dim, inline
/// `code` on a tile, fenced code in a framed surface tile.
struct AssistantText<'a> {
    text: &'a str,
}

impl Component for AssistantText<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let w = cx.width;
        let lead: Vec<Span<'static>> = vec![Span::raw("  ")];
        let content_w = w.saturating_sub(2);
        let text = self.text.trim();
        if text.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut in_code = false;
        for raw in text.split('\n') {
            let trimmed = raw.trim_end();
            if trimmed.starts_with("```") {
                in_code = !in_code;
                let lang = trimmed.trim_start_matches('`');
                let label = if in_code {
                    format!("╭─ {}", if lang.is_empty() { "code" } else { lang })
                } else {
                    "╰──".to_string()
                };
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(label, Style::new().fg(t.muted).bg(t.surface)),
                ]));
                continue;
            }
            if in_code {
                let body = prim::truncate(raw, content_w.saturating_sub(2));
                out.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(format!("│ {body}"), Style::new().fg(t.fg).bg(t.surface)),
                ]));
                continue;
            }
            if let Some(h) = trimmed.strip_prefix("# ").or_else(|| trimmed.strip_prefix("## ")) {
                for seg in prim::wrap(h, content_w) {
                    out.push(prefixed(&lead, vec![Span::styled(
                        seg,
                        Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
                    )]));
                }
            } else if let Some(q) = trimmed.strip_prefix("> ") {
                for seg in prim::wrap(q, content_w) {
                    out.push(prefixed(&lead, vec![Span::styled(
                        seg,
                        Style::new().fg(t.muted).add_modifier(Modifier::ITALIC),
                    )]));
                }
            } else {
                for seg in prim::wrap(raw, content_w) {
                    out.push(prefixed(&lead, inline_code(&seg, t)));
                }
            }
        }
        out
    }
}

/// Split a line into spans, turning `` `code` `` segments into tiles.
fn inline_code(line: &str, t: Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;
    let code_style = Style::new().fg(t.info).bg(t.inline_bg);
    while let Some(start) = rest.find('`') {
        if start > 0 {
            spans.push(Span::raw(rest[..start].to_string()));
        }
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            spans.push(Span::styled(after[..end].to_string(), code_style));
            rest = &after[end + 1..];
        } else {
            spans.push(Span::raw(rest[start..].to_string()));
            return spans;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::raw(rest.to_string()));
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

// ── Thinking ─────────────────────────────────────────────────────────────

/// Reasoning: muted italic text with a 2-space margin. While thinking a
/// trailing `Thinking...` pulses; once settled it becomes `Thought for Ns`.
struct Thinking<'a> {
    block: &'a ThinkingBlock,
}

impl Component for Thinking<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let lead: Vec<Span<'static>> = vec![Span::raw("  ")];
        let body = Style::new().fg(t.muted).add_modifier(Modifier::ITALIC);
        let content_w = cx.width.saturating_sub(2);
        let text = self.block.text.trim();
        let working = self.block.elapsed.is_none() && cx.active_turn;
        if text.is_empty() && !working {
            return Vec::new();
        }
        let mut out = Vec::new();
        for seg in prim::wrap(text, content_w) {
            out.push(prefixed(&lead, vec![Span::styled(seg, body)]));
        }
        if working {
            if !out.is_empty() {
                out.push(prim::blank());
            }
            out.push(prefixed(&lead, vec![Span::styled("Thinking...", body)]));
        } else if let Some(d) = self.block.elapsed {
            if !d.is_zero() {
                if !out.is_empty() {
                    out.push(prim::blank());
                }
                out.push(prefixed(&lead, vec![Span::styled(
                    format!("Thought for {}", prim::fmt_duration(d)),
                    Style::new().fg(t.muted),
                )]));
            }
        }
        out
    }
}

// ── Exec tree ────────────────────────────────────────────────────────────

/// An `exec` block drawn as a tree on a state-colored background: gray while
/// running, green when it succeeded, red when it failed. Header `Exec
/// <label>`, the code with line numbers behind a `│` rail, then each native
/// tool branched off that rail, and a final `└ ✓ Succeed`/`└ ✗ Failed` line
/// with a result preview once done.
struct ExecBlock<'a> {
    tool: &'a ToolCall,
}

impl Component for ExecBlock<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let w = cx.width;
        // No colored background for now — the tree sits on the default
        // surface. State is still conveyed by the per-branch status icon.
        let bg = Color::Reset;

        let mut out = Vec::new();

        let header = match &self.tool.label {
            Some(l) if !l.is_empty() => format!("Exec {l}"),
            _ => "Exec".to_string(),
        };
        out.push(prim::pad(
            vec![prim::gutter(bg), prim::bold(header.clone(), t, bg)],
            bg,
            w,
            2 + prim::width(&header),
        ));

        let code: Vec<&str> = self.tool.input.split('\n').collect();
        let lw = code.len().to_string().len().max(3);
        let rail = prim::rail(t, bg);
        let avail = w.saturating_sub(4).saturating_sub(lw + 1);
        for (i, line) in code.iter().enumerate() {
            let n = format!("{:>lw$} ", i + 1, lw = lw);
            let body = prim::truncate(line, avail);
            out.push(prim::pad(
                vec![
                    prim::gutter(bg),
                    rail.clone(),
                    Span::styled(n, Style::new().fg(t.subtle).bg(bg)),
                    Span::styled(body.clone(), Style::new().fg(t.fg).bg(bg)),
                ],
                bg,
                w,
                4 + lw + 1 + prim::width(&body),
            ));
        }

        let n_total = self.tool.native.len();
        for (idx, nt) in self.tool.native.iter().enumerate() {
            // While the exec is still running the last native tool is the
            // tail (`└`); once done the final `└` is the exec-result line, so
            // every native tool becomes a `├`.
            let is_last = idx + 1 == n_total && !self.tool.done;
            out.extend(ExecBlockBranch { nt, bg, is_last }.lines(cx));
        }

        if self.tool.done {
            out.extend(exec_result_lines(self.tool, t, bg, w, cx.app.verbose));
        }

        out
    }
}

/// The final `└ ✓ Succeed, took Ns` / `└ ✗ Failed, took Ns` branch with a
/// preview of the returned value or error.
fn exec_result_lines(
    tool: &ToolCall,
    t: Theme,
    bg: Color,
    w: usize,
    verbose: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
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
    let summary = format!("{label}{took}");
    out.push(prim::pad(
        vec![
            prim::gutter(bg),
            prim::branch(t, bg, true),
            Span::styled(format!("{icon} "), Style::new().fg(fg_color).bg(bg)),
            prim::fg(summary.clone(), t, bg),
        ],
        bg,
        w,
        2 + 2 + 2 + prim::width(&summary),
    ));

    let Some(result) = &tool.result else {
        return out;
    };
    let display = lofi_core::exec_result_display(result, tool.is_error);
    if display.is_empty() {
        return out;
    }
    let indent = 2 + 2 + 2; // gutter + branch col + own rail
    let all: Vec<&str> = display.split('\n').collect();
    let limit = if verbose { all.len() } else { PREVIEW_LINES };
    let hidden = all.len().saturating_sub(limit);
    let avail = w.saturating_sub(indent);
    let body_fg = if tool.is_error { t.error } else { t.muted };
    for line in all.iter().take(limit) {
        let body = prim::truncate(line, avail);
        out.push(prim::pad(
            vec![
                prim::gutter(bg),
                Span::styled("  ", Style::new().bg(bg)),
                prim::rail(t, bg),
                Span::styled(body.clone(), Style::new().fg(body_fg).bg(bg)),
            ],
            bg,
            w,
            indent + prim::width(&body),
        ));
    }
    if hidden > 0 {
        let cap = format!("({hidden} lines hidden)");
        out.push(prim::pad(
            vec![
                prim::gutter(bg),
                Span::styled("  ", Style::new().bg(bg)),
                Span::styled("… ", Style::new().fg(t.subtle).bg(bg)),
                Span::styled(cap.clone(), Style::new().fg(t.subtle).bg(bg)),
            ],
            bg,
            w,
            indent + prim::width(&cap),
        ));
    }
    out
}

// ── Exec block branch ───────────────────────────────────────────────────

/// One native tool call branched off the exec rail: a `├`/`└` header with a
/// status icon, then its result preview indented under a second rail.
struct ExecBlockBranch<'a> {
    nt: &'a NativeTool,
    bg: Color,
    /// Whether this is the tail of the exec tree (selects `└` and stops the
    /// exec rail from continuing through the result body).
    is_last: bool,
}

impl Component for ExecBlockBranch<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let w = cx.width;
        let bg = self.bg;
        let working = !self.nt.done && cx.active_turn;
        let mut out = Vec::new();

        let mut header = vec![
            prim::gutter(bg),
            prim::branch(t, bg, self.is_last),
            prim::status_icon(t, bg, working, self.nt.is_error, cx.spinner()),
            prim::muted("Tool ".to_string(), t, bg),
            Span::styled(self.nt.name.clone(), Style::new().fg(t.info).bg(bg)),
        ];
        let mut used = 2 + 2 + 2 + 5 + prim::width(&self.nt.name);
        if !self.nt.args.is_empty() {
            header.push(prim::subtle(format!(" {}", self.nt.args), t, bg));
            used += 1 + prim::width(&self.nt.args);
        }
        out.push(prim::pad(header, bg, w, used));

        let Some(result) = &self.nt.result else {
            return out;
        };
        if result.is_empty() {
            return out;
        }

        let exec_cont = if self.is_last { "  " } else { "│ " };
        let indent = 2 + 2 + 2; // gutter + exec-rail col + own rail
        let all: Vec<&str> = result.split('\n').collect();
        let numbered = matches!(self.nt.name.as_str(), "read" | "view");
        let total = all.len();
        let lw = total.to_string().len().max(3);
        let avail = w
            .saturating_sub(indent)
            .saturating_sub(if numbered { lw + 1 } else { 0 });
        let limit = if cx.app.verbose { total } else { PREVIEW_LINES };
        let hidden = total.saturating_sub(limit);
        let body_fg = if self.nt.is_error { t.error } else { t.muted };
        for (i, line) in all.iter().take(limit).enumerate() {
            let mut spans = vec![
                prim::gutter(bg),
                Span::styled(exec_cont, Style::new().fg(t.subtle).bg(bg)),
                prim::rail(t, bg),
            ];
            let line_used = if numbered {
                let n = format!("{:>lw$} ", i + 1, lw = lw);
                let body = prim::truncate(line, avail);
                spans.push(Span::styled(n, Style::new().fg(t.subtle).bg(bg)));
                spans.push(Span::styled(body.clone(), Style::new().fg(t.fg).bg(bg)));
                indent + lw + 1 + prim::width(&body)
            } else {
                let body = prim::truncate(line, avail);
                spans.push(Span::styled(body.clone(), Style::new().fg(body_fg).bg(bg)));
                indent + prim::width(&body)
            };
            out.push(prim::pad(spans, bg, w, line_used));
        }
        if hidden > 0 {
            let cap = format!("({hidden} lines hidden)");
            out.push(prim::pad(
                vec![
                    prim::gutter(bg),
                    Span::styled(exec_cont, Style::new().fg(t.subtle).bg(bg)),
                    Span::styled("… ", Style::new().fg(t.subtle).bg(bg)),
                    Span::styled(cap.clone(), Style::new().fg(t.subtle).bg(bg)),
                ],
                bg,
                w,
                indent + prim::width(&cap),
            ));
        }
        out
    }
}



// ── Non-exec tool ────────────────────────────────────────────────────────

/// A non-`exec` tool — e.g. a hallucinated name the model emitted despite
/// only `exec` being advertised — rendered as a single status line without
/// the tree. Not reached in normal operation; kept as a defensive fallback.
struct ToolLine<'a> {
    tool: &'a ToolCall,
}

impl Component for ToolLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let working = !self.tool.done && cx.active_turn;
        let icon = prim::status_icon(t, Color::Reset, working, self.tool.is_error, cx.spinner());
        let mut spans = vec![Span::raw("  "), icon, Span::styled(self.tool.name.clone(), Style::new().fg(t.info))];
        if let Some(first) = self.tool.input.split('\n').next() {
            if !first.is_empty() {
                spans.push(prim::subtle(format!(" {first}"), t, Color::Reset));
            }
        }
        vec![Line::from(spans)]
    }
}

// ── Fatal error ─────────────────────────────────────────────────────────

/// A fatal error line: `✗ <message>`.
struct ErrorLine<'a> {
    msg: &'a str,
}

impl Component for ErrorLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        vec![Line::from(vec![
            Span::raw("  "),
            Span::styled("✗ ", Style::new().fg(t.error)),
            Span::styled(self.msg.to_string(), Style::new().fg(t.error)),
        ])]
    }
}

// ── Turn-end rule ─────────────────────────────────────────────────────────

/// Crush-style turn-end separator: `<label> done in Ns` followed by a dash
/// fill to the content width. Appended to a turn when its run finishes.
struct TurnEnd {
    label: String,
    elapsed: Duration,
}

impl Component for TurnEnd {
    fn lines(&self, cx: &Cx) -> Vec<Line<'static>> {
        let t = cx.theme;
        let w = cx.width;
        let dur = prim::fmt_duration(self.elapsed);
        let model_w = prim::width(&self.label);
        let done = format!(" done in {dur}");
        let dash_w = w.saturating_sub(4 + model_w + prim::width(&done) + 1);
        let mut spans = vec![
            Span::raw("  "),
            Span::styled("◇ ", Style::new().fg(t.subtle)),
            Span::styled(self.label.clone(), Style::new().fg(t.muted)),
            Span::styled(done, Style::new().fg(t.subtle)),
        ];
        if dash_w > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("─".repeat(dash_w), Style::new().fg(t.subtle)));
        }
        vec![Line::from(spans)]
    }
}

// ── helpers ──────────────────────────────────────────────────────────────

fn prefixed(prefix: &[Span<'static>], spans: Vec<Span<'static>>) -> Line<'static> {
    let mut v = prefix.to_vec();
    v.extend(spans);
    Line::from(v)
}

// `active_indicator` is re-exported for the working indicator in the chrome;
// keep the import here so the component module can surface it if needed.
#[allow(unused_imports)]
use active_indicator as _;