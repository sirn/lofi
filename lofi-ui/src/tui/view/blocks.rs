#![allow(clippy::needless_lifetimes)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use pulldown_cmark::{Event, Options as MdOptions, Parser as MdParser, Tag as MdTag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::tui::theme::{active_indicator, agent_indicator, user_indicator, Theme};
use crate::tui::{
    App, Block, DetailKey, NativePreview, NativeTool, ThinkingBlock, ToolCall, Turn,
    DETAIL_VIEW_ROWS,
};

use super::component::{Component, Cx, Stack};
use super::prim::{self, Hyperlink, RawLine, RenderLine};

const PREVIEW_LINES: usize = 3;

fn detail_marker(expanded: bool) -> &'static str {
    if expanded { "▾" } else { "▸" }
}

fn detail_box(
    lines: &[String],
    key: DetailKey,
    tail: bool,
    force_open: bool,
    cx: &Cx,
    deco: Vec<Span<'static>>,
    style: Style,
) -> Vec<RenderLine> {
    let expanded = cx.app.expanded_details.get(&key);
    let total = lines.len();
    let target = |line: RenderLine| line.with_detail(key.clone(), total, tail);
    if expanded.is_none() && !force_open {
        return Vec::new();
    }
    let inner = cx.width.saturating_sub(prim::span_width(&deco) + 3);
    let max = total.saturating_sub(DETAIL_VIEW_ROWS);
    let start = expanded
        .and_then(|state| state.scroll)
        .unwrap_or_else(|| if tail { max } else { 0 })
        .min(max);
    let visible = &lines[start..total.min(start + DETAIL_VIEW_ROWS)];
    let border = Style::new().fg(cx.theme.subtle);
    let mut out = Vec::with_capacity(DETAIL_VIEW_ROWS + 2);
    out.push(target(prim::rline(
        deco.clone(),
        vec![Span::styled(format!("┌{}┐", "─".repeat(inner)), border)],
    )));
    for row in 0..DETAIL_VIEW_ROWS {
        let text = visible.get(row).map_or("", String::as_str);
        let shown = prim::truncate(text, inner);
        let used = prim::width(&shown);
        let thumb = total > DETAIL_VIEW_ROWS
            && row == (start * DETAIL_VIEW_ROWS / total.max(1)).min(DETAIL_VIEW_ROWS - 1);
        out.push(target(prim::rline(
            deco.clone(),
            vec![
                Span::styled("│", border),
                Span::styled(shown, style),
                Span::styled(" ".repeat(inner.saturating_sub(used)), style),
                Span::styled(if thumb { "┃" } else { "│" }, border),
            ],
        )));
    }
    out.push(target(prim::rline(
        deco,
        vec![Span::styled(format!("└{}┘", "─".repeat(inner)), border)],
    )));
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
        out.extend(
            render_turn_lines_indexed(&cx, turn, i)
                .into_iter()
                .map(|rl| rl.line),
        );
    }
    Text::from(out)
}

pub fn render_turn_lines(cx: &Cx, turn: &Turn) -> Vec<RenderLine> {
    render_turn_lines_indexed(cx, turn, 0)
}

pub fn render_turn_lines_indexed(cx: &Cx, turn: &Turn, turn_index: usize) -> Vec<RenderLine> {
    turn_stack(turn, turn_index).lines(cx)
}

pub fn render_turn_height(cx: &Cx, turn: &Turn) -> usize {
    render_turn_height_indexed(cx, turn, 0)
}

pub fn render_turn_height_indexed(cx: &Cx, turn: &Turn, turn_index: usize) -> usize {
    turn_stack(turn, turn_index).height(cx)
}

pub fn render_turn_window(cx: &Cx, turn: &Turn, range: std::ops::Range<usize>) -> Vec<RenderLine> {
    render_turn_window_indexed(cx, turn, 0, range)
}

pub fn render_turn_window_indexed(
    cx: &Cx,
    turn: &Turn,
    turn_index: usize,
    range: std::ops::Range<usize>,
) -> Vec<RenderLine> {
    turn_stack(turn, turn_index).lines_window(cx, range)
}

fn turn_stack(turn: &Turn, turn_index: usize) -> Stack<'_> {
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
    for (block_index, block) in turn.blocks.iter().enumerate() {
        match block {
            Block::Text(text) => stack.push(AssistantText { text }),
            Block::Thinking(tb) => stack.push(Thinking {
                block: tb,
                key: DetailKey::Thinking {
                    turn: turn_index,
                    block: block_index,
                },
            }),
            Block::Tool(tool) => {
                if tool.name == "exec" {
                    stack.push(ExecBlock {
                        tool,
                        turn: turn_index,
                        block: block_index,
                    });
                } else {
                    stack.push(ToolLine { tool });
                }
            }
            Block::UserShell {
                command,
                output,
                exit_code,
                signal,
                duration,
                truncated,
                cancelled,
                exclude_from_context,
            } => stack.push(UserShellLine {
                command,
                output,
                exit_code: *exit_code,
                signal: *signal,
                duration: *duration,
                truncated: *truncated,
                cancelled: *cancelled,
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
                summarized,
                kept,
                summary,
            } => {
                stack.push(CompactionLine {
                    summarized: *summarized,
                    kept: *kept,
                    summary: summary.clone(),
                    key: DetailKey::Compaction {
                        turn: turn_index,
                        block: block_index,
                    },
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
}

fn render_markdown_body(
    text: &str,
    t: Theme,
    w: usize,
    content_w: usize,
    base_style: Style,
    lead_fn: impl Fn(usize) -> Vec<Span<'static>>,
) -> Vec<RenderLine> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut row = 0usize;
    for block in analyze(text, t, base_style) {
        match block {
            MdBlock::Code { lang, lines, fence } => {
                emit_code(
                    &mut out,
                    &mut row,
                    text,
                    lang.as_deref(),
                    &lines,
                    &fence,
                    t,
                    w,
                    content_w,
                    &lead_fn,
                );
            }
            MdBlock::Table {
                header,
                aligns,
                rows,
                src,
            } => {
                emit_table(
                    &mut out, &mut row, text, &header, &aligns, &rows, &src, t, content_w, &lead_fn,
                );
            }
            MdBlock::Heading {
                level,
                content_start,
                src,
                ..
            } => {
                emit_heading(
                    &mut out,
                    &mut row,
                    text,
                    level,                    level,
                    content_start,
                    &src,
                    t,
                    base_style,
                    content_w,
                    &lead_fn,
                );
            }
            MdBlock::Quote { lines, .. } => {
                emit_quote(&mut out, &mut row, text, &lines, t, content_w, &lead_fn);
            }
            MdBlock::Paragraph { spans, src } => {
                emit_paragraph(&mut out, &mut row, text, &spans, &src, content_w, &lead_fn);
            }
            MdBlock::Blank { src } => {
                emit_blank(&mut out, &mut row, text, &src, &lead_fn);
            }
        }
    }
    out
}

/// Emit a blank source line as a single empty content row, matching how an
/// empty paragraph wraps (one row, no content).
fn emit_blank(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    src: &std::ops::Range<usize>,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let lead = lead_fn(*row);
    let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
    // The blank maps to its (empty) source line so selection reproduces the
    // blank as an empty line rather than dropping it.
    let raw_src = &text[src.clone()];
    out.push(RenderLine {
        line: Line::from(lead),
        content: (deco_len, deco_len),
        raw: Some(RawLine::new(
            Arc::from(raw_src),
            vec![0, raw_src.len()],
            true,
        )),
        links: Vec::new(),
        detail: None,
    });
    *row += 1;
}
/// Emit a fenced code block: opening/closing fence tiles and one tile row per
/// wrapped body segment. Source ranges come from `analyze`; body rows map back
/// to their source line for yank/selection.
#[allow(clippy::too_many_arguments)]
fn emit_code(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    lang: Option<&str>,
    lines: &[(String, std::ops::Range<usize>)],
    fence: &std::ops::Range<usize>,
    t: Theme,
    w: usize,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let fence_text = &text[fence.clone()];
    let open_label = match lang {
        Some(l) => format!("```{l}"),
        None => "```".to_string(),
    };
    let open_raw = fence_text.split('\n').next().unwrap_or("```");
    out.push(
        prim::rtile(
            lead_fn(*row),
            vec![Span::styled(
                open_label,
                Style::new().fg(t.muted).bg(t.surface),
            )],
            t.surface,
            w,
        )
        .with_raw(RawLine::linear(
            Arc::from(open_raw),
            0,
            open_raw.chars().count(),
            true,
        )),
    );
    *row += 1;
    let avail = content_w.saturating_sub(2);
    for (line_text, _line_range) in lines {
        let src: Arc<str> = Arc::from(line_text.as_str());
        let indent_len = line_text
            .bytes()
            .take_while(|&b| b == b' ' || b == b'\t')
            .count();
        let body = &line_text[indent_len..];
        // Byte offsets of every char boundary within `body`, including the end.
        // Indexing past the recorded boundary (a wrap segment wider than the
        // remaining body) clamps to `body.len()`, a valid boundary, so the map
        // never yields a mid-UTF8 or out-of-range source offset.
        let body_offs: Vec<usize> = std::iter::once(0)
            .chain(body.char_indices().map(|(b, c)| b + c.len_utf8()))
            .collect();
        let indent_chars = line_text[..indent_len].chars().count();
        let segments = prim::wrap_pre(line_text, avail);
        let mut cum = 0usize;
        for (i, seg) in segments.into_iter().enumerate() {
            let body_chars = seg.chars().count().saturating_sub(indent_chars);
            let map: Vec<usize> = (0..=body_chars)
                .map(|k| {
                    let byte = body_offs
                        .get(cum + k)
                        .copied()
                        .unwrap_or(body.len())
                        .min(body.len());
                    indent_len + byte
                })
                .collect();
            out.push(
                prim::rtile(
                    lead_fn(*row),
                    vec![Span::styled(seg, Style::new().fg(t.fg).bg(t.surface))],
                    t.surface,
                    w,
                )
                .with_raw(RawLine::new(src.clone(), map, i == 0)),
            );
            *row += 1;
            cum += body_chars;
        }
    }
    let close_raw = fence_text.split('\n').next_back().unwrap_or("```");
    out.push(
        prim::rtile(
            lead_fn(*row),
            vec![Span::styled("```", Style::new().fg(t.muted).bg(t.surface))],
            t.surface,
            w,
        )
        .with_raw(RawLine::linear(
            Arc::from(close_raw),
            0,
            close_raw.chars().count(),
            true,
        )),
    );
    *row += 1;
}
/// Concatenate inline spans into plain text for the table grid (which
/// re-parses inline styling itself).
fn spans_text(spans: &[MappedSpan]) -> String {
    spans.iter().map(|m| m.span.content.as_ref()).collect()
}

/// Emit a pipe table via the shared grid renderer. Cells carry inline spans
/// and source ranges from `analyze`; the raw `| ... |` source lines feed yank.
#[allow(clippy::too_many_arguments)]
fn emit_table(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    header: &[MdCell],
    aligns: &[Align],
    rows: &[Vec<MdCell>],
    src: &std::ops::Range<usize>,
    t: Theme,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let src_block = &text[src.clone()];
    let src_lines: Vec<&str> = src_block.split('\n').collect();
    // Pass each cell's raw source text so render_table re-parses inline
    // formatting (bold/code/links) exactly as any other inline context.
    let cell_src = |c: &MdCell| text[c.src.clone()].trim().to_string();
    let header_txt: Vec<String> = header.iter().map(&cell_src).collect();
    let data_txt: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(&cell_src).collect())
        .collect();
    let before = out.len();
    out.extend(render_table(
        &header_txt,
        &data_txt,
        aligns,
        content_w,
        t,
        &src_lines,
        *row,
        lead_fn,
    ));
    *row += out.len() - before;
}

/// Emit an ATX heading. `content_start` is the byte offset of the text after
/// the `# ` prefix; the heading text is the trimmed remainder of the source.
#[allow(clippy::too_many_arguments)]
fn emit_heading(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    level: u8,
    content_start: usize,
    src: &std::ops::Range<usize>,
    t: Theme,
    base_style: Style,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let trimmed = text[src.clone()].trim_end();
    let h = text[content_start..src.end].trim_end();
    let head_fg = if level <= 2 {
        base_style.fg.unwrap_or(t.fg)
    } else {
        t.muted
    };
    let style = Style::new().fg(head_fg).add_modifier(Modifier::BOLD);
    let prefix_len = content_start - src.start;
    let src_arc: Arc<str> = Arc::from(trimmed);
    let rows = wrap_with_map(h, prefix_len, trimmed.len(), content_w);
    for (i, (seg, map)) in rows.into_iter().enumerate() {
        out.push(
            prim::rline(lead_fn(*row), vec![Span::styled(seg, style)]).with_raw(RawLine::new(
                src_arc.clone(),
                map,
                i == 0,
            )),
        );
        *row += 1;
    }
}
/// Emit a block quote: a `▎ ` bar followed by each wrapped body line. Each
/// source line maps back to its `> ...` raw text for yank.
fn emit_quote(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    lines: &[MdQuoteLine],
    t: Theme,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let quote_style = Style::new().fg(t.muted);
    let bar = Span::styled("▎ ", Style::new().fg(t.subtle));
    for ql in lines {
        let qtrimmed = text[ql.src.clone()].trim_end();
        let src_arc: Arc<str> = Arc::from(qtrimmed);
        if ql.spans.is_empty() {
            // Bare `>` line: just the bar with a thin-space content span so
            // selection yanks the raw `>` markdown.
            let map = vec![0, src_arc.len()];
            let mut lead = lead_fn(*row);
            lead.push(bar.clone());
            let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
            let content_span = Span::styled("\u{2009}", quote_style);
            let content_len = content_span.content.chars().count();
            let mut all = lead.clone();
            all.push(content_span);
            out.push(RenderLine {
                line: Line::from(all),
                content: (deco_len, deco_len + content_len),
                raw: Some(RawLine::new(src_arc, map, true)),
                links: Vec::new(),
                detail: None,
            });
            *row += 1;
            continue;
        }
        let body = &qtrimmed[ql.prefix_len..];
        let rows = wrap_with_map(
            body,
            ql.prefix_len,
            qtrimmed.len(),
            content_w.saturating_sub(2),
        );
        for (i, (seg, map)) in rows.into_iter().enumerate() {
            let mut lead = lead_fn(*row);
            lead.push(bar.clone());
            out.push(
                prim::rline(lead, vec![Span::styled(seg, quote_style)]).with_raw(RawLine::new(
                    src_arc.clone(),
                    map,
                    i == 0,
                )),
            );
            *row += 1;
        }
    }
}

/// Emit a paragraph: inline-parse the source sub-slice, wrap to the content
/// width, and split the source-offset map and hyperlinks across wrapped rows.
#[allow(clippy::too_many_arguments)]
fn emit_paragraph(
    out: &mut Vec<RenderLine>,
    row: &mut usize,
    text: &str,
    block_spans: &[MappedSpan],
    src: &std::ops::Range<usize>,
    content_w: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) {
    let raw = text[src.clone()].trim_end();
    // Reuse the spans already produced by `analyze`'s single inline pass. They
    // carry global source offsets; shift them to the block-local sub-slice so
    // the content-map / hyperlink logic below works unchanged.
    let base_off = src.start;
    let mapped: Vec<MappedSpan> = block_spans
        .iter()
        .cloned()
        .map(|mut m| {
            m.content_start = m.content_start.saturating_sub(base_off);
            m.boundary_start = m.boundary_start.saturating_sub(base_off);
            m
        })
        .collect();
    let lead_ws = mapped
        .iter()
        .flat_map(|m| m.span.content.chars())
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    let full_map = build_content_map(raw.len(), &mapped);
    let links = mapped_hyperlinks(&mapped);
    let spans: Vec<Span> = mapped.into_iter().map(|m| m.span).collect();
    let line = Line::from(spans);
    let src_arc: Arc<str> = Arc::from(raw);
    let rows = prim::wrap_line_styled(&line, content_w);
    let row_maps = split_map_by_rows(&full_map, lead_ws, &rows);
    let row_links = split_links_by_rows(&links, &rows);
    for (i, wrapped) in rows.into_iter().enumerate() {
        let lead = lead_fn(*row);
        let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
        let links = row_links.get(i).map_or_else(Vec::new, |links| {
            links
                .iter()
                .map(|link| Hyperlink {
                    start: deco_len + link.start,
                    end: deco_len + link.end,
                    url: link.url.clone(),
                })
                .collect()
        });
        out.push(
            prim::rline(lead, wrapped.spans)
                .with_raw(RawLine::new(
                    src_arc.clone(),
                    row_maps.get(i).cloned().unwrap_or_default(),
                    i == 0,
                ))
                .with_links(links),
        );
        *row += 1;
    }
}

// === Block-level markdown analysis (single pulldown pass) ====================
//
// `analyze` is the single structural entry point for markdown. One
// `into_offset_iter` pass produces a flat `Vec<MdBlock>` in which every block
// carries the source byte ranges needed to rebuild yank/selection raw text.
// Both `render_markdown_body` and `markdown_body_height` walk this same tree,
// so the render/height row contract (guarded by `markdown_height_matches_lines`)
// holds by construction rather than by two parallel line-scanners that drift.

/// A table cell: inline content plus the source range it came from.
#[derive(Clone)]
struct MdCell {
    /// Source byte range of the cell text (without surrounding `|`/padding).
    src: std::ops::Range<usize>,
}

/// One source line inside a block quote.
#[derive(Clone)]
struct MdQuoteLine {
    spans: Vec<MappedSpan>,
    /// Byte range of the full source line including the `>` marker.
    src: std::ops::Range<usize>,
    /// Byte length of the `> `/`>` prefix.
    prefix_len: usize,
}

/// One structural block of a markdown document.
#[derive(Clone)]
enum MdBlock {
    /// A run of one or more blank source lines (rendered as empty rows).
    Blank { src: std::ops::Range<usize> },
    Paragraph {
        spans: Vec<MappedSpan>,
        src: std::ops::Range<usize>,
    },
    Heading {
        level: u8,
        src: std::ops::Range<usize>,
        /// Byte offset of the first content char (after the `# ` prefix).
        content_start: usize,
    },
    Code {
        lang: Option<String>,
        /// Body source lines with byte ranges (fence markers excluded).
        lines: Vec<(String, std::ops::Range<usize>)>,
        fence: std::ops::Range<usize>,
    },
    Table {
        header: Vec<MdCell>,
        aligns: Vec<Align>,
        rows: Vec<Vec<MdCell>>,
        src: std::ops::Range<usize>,
    },
    Quote {
        lines: Vec<MdQuoteLine>,
        src: std::ops::Range<usize>,
    },
}

/// Tracks the tightest source extent of one inline-bearing node while the block
/// pass streams events. The node tag range is the outer bound; the inner text
/// extent lets us re-parse exactly the inline sub-slice.
#[derive(Clone, Copy)]
struct InlineExtent {
    start: usize,
    end: usize,
    seen: bool,
}

impl InlineExtent {
    fn new() -> Self {
        Self {
            start: 0,
            end: 0,
            seen: false,
        }
    }
    fn observe(&mut self, range: &std::ops::Range<usize>) {
        if self.seen {
            self.start = self.start.min(range.start);
            self.end = self.end.max(range.end);
        } else {
            self.start = range.start;
            self.end = range.end;
            self.seen = true;
        }
    }
}

/// Re-parse the inline sub-slice with the shared inline machine and shift its
/// span offsets to global source bytes. The extent is narrowed to non-whitespace
/// content first: pulldown tag ranges (notably table cells) include surrounding
/// padding, and cell spans must point at the trimmed text.
fn inline_extent_spans(ext: &InlineExtent, text: &str, t: Theme, base: Style) -> Vec<MappedSpan> {
    let raw = &text[ext.start..ext.end];
    let trimmed = raw.trim_matches(|c: char| c.is_whitespace());
    if trimmed.is_empty() {
        return Vec::new();
    }
    let lead = raw.len() - raw.trim_start_matches(|c: char| c.is_whitespace()).len();
    let base_off = ext.start + lead;
    let mut spans = inline_spans_mapped(trimmed, t, base);
    for m in &mut spans {
        m.content_start += base_off;
        m.boundary_start += base_off;
    }
    spans
}

/// The sub-range of `ext` with leading/trailing whitespace removed, or `None`
/// when the extent is entirely whitespace.
fn trimmed_range(text: &str, ext: &InlineExtent) -> Option<std::ops::Range<usize>> {
    if !ext.seen {
        return None;
    }
    let raw = &text[ext.start..ext.end];
    let lead = raw.len() - raw.trim_start_matches(|c: char| c.is_whitespace()).len();
    let trail = raw.len() - raw.trim_end_matches(|c: char| c.is_whitespace()).len();
    if lead + trail >= raw.len() {
        return None;
    }
    Some(ext.start + lead..ext.end - trail)
}
/// Numeric level (1..=6) for a `HeadingLevel`.
fn heading_level_num(level: pulldown_cmark::HeadingLevel) -> u8 {
    use pulldown_cmark::HeadingLevel as H;
    match level {
        H::H1 => 1,
        H::H2 => 2,
        H::H3 => 3,
        H::H4 => 4,
        H::H5 => 5,
        H::H6 => 6,
    }
}

/// In-progress block frames for `analyze`. The renderer is flat (no nested
/// block containers), so an explicit stack suffices; quote/table frames own
/// their inline extents and no nested block events are expected at depth.
enum BlockFrame {
    Para {
        ext: InlineExtent,
        src: std::ops::Range<usize>,
    },
    Heading {
        level: u8,
        ext: InlineExtent,
        src: std::ops::Range<usize>,
    },
    Code {
        lang: Option<String>,
        fence: std::ops::Range<usize>,
        body: String,
        body_range: Option<std::ops::Range<usize>>,
    },
    Table {
        aligns: Vec<Align>,
        src: std::ops::Range<usize>,
        header: Vec<MdCell>,
        rows: Vec<Vec<MdCell>>,
        cur: Vec<MdCell>,
        in_head: bool,
        cell: Option<(InlineExtent, std::ops::Range<usize>)>,
    },
    Quote {
        src: std::ops::Range<usize>,
    },
    /// The outermost list. Rendered line-preserving (each source line a
    /// paragraph, markers and indent intact), so the frame captures the source
    /// range plus a depth counter for the nested lists pulldown reports inside
    /// it (their Start/End must not close this frame).
    List {
        src: std::ops::Range<usize>,
        depth: usize,
    },
}

/// True when a container frame (block quote or list) remains on the stack.
/// Nested blocks inside a container are rendered by the container, so the block
/// pass must not also emit them at top level.
fn stack_in_container(stack: &[BlockFrame]) -> bool {
    stack
        .iter()
        .any(|f| matches!(f, BlockFrame::Quote { .. } | BlockFrame::List { .. }))
}

/// Route an inline event range to the innermost inline-owning frame. The
/// structure is flat (no nested inline containers), so only the top frame can
/// own inline extents; table cells delegate to their open cell.
fn observe_inline(stack: &mut [BlockFrame], range: &std::ops::Range<usize>) {
    match stack.last_mut() {
        Some(BlockFrame::Para { ext, .. } | BlockFrame::Heading { ext, .. }) => {
            ext.observe(range);
        }
        Some(BlockFrame::Table {
            cell: Some((cext, _)),
            ..
        }) => cext.observe(range),
        _ => {}
    }
}

/// Parse `text` into structural blocks via a single `pulldown-cmark` pass.
fn analyze(text: &str, t: Theme, base: Style) -> Vec<MdBlock> {
    if text.is_empty() {
        return Vec::new();
    }
    let opts = MdOptions::ENABLE_STRIKETHROUGH
        | MdOptions::ENABLE_TABLES
        | MdOptions::ENABLE_FOOTNOTES
        | MdOptions::ENABLE_TASKLISTS
        | MdOptions::ENABLE_HEADING_ATTRIBUTES;
    let mut blocks: Vec<MdBlock> = Vec::new();
    let mut stack: Vec<BlockFrame> = Vec::new();

    for (ev, range) in MdParser::new_ext(text, opts).into_offset_iter() {
        match ev {
            // CommonMark treats tags like `<input>` as HTML, not text.
            // Keep them as paragraphs so the transcript shows the source.
            Event::Start(MdTag::Paragraph | MdTag::HtmlBlock) => {
                stack.push(BlockFrame::Para {
                    ext: InlineExtent::new(),
                    src: range,
                });
            }
            Event::End(TagEnd::Paragraph | TagEnd::HtmlBlock) => {
                if let Some(BlockFrame::Para { ext, src }) = stack.pop() {
                    let spans = if ext.seen {
                        inline_extent_spans(&ext, text, t, base)
                    } else {
                        Vec::new()
                    };
                    // Skip source-blank paragraphs (whitespace-only): they carry
                    // no renderable content and would emit a spurious block. A
                    // paragraph inside a block quote is owned by that quote.
                    if !spans.is_empty() && !stack_in_container(&stack) {
                        blocks.push(MdBlock::Paragraph { spans, src });
                    }
                }
            }
            Event::Start(MdTag::Heading { level, .. }) => stack.push(BlockFrame::Heading {
                level: heading_level_num(level),
                ext: InlineExtent::new(),
                src: range,
            }),
            Event::End(TagEnd::Heading(_)) => {
                if let Some(BlockFrame::Heading { level, ext, src }) = stack.pop() {
                    // `InlineExtent` initializes `start` to 0 before any inline event is
                    // observed. A bare heading marker (`#` alone, e.g. mid-stream) yields
                    // no inline events, so `ext.start` is still 0 while `src.start > 0`,
                    // and `content_start - src.start` would underflow. Clamp to the block
                    // range: with no content the heading renders as one row regardless.
                    let content_start = if ext.seen {
                        ext.start.max(src.start)
                    } else {
                        src.start
                    };
                    blocks.push(MdBlock::Heading {
                        level,
                        content_start,
                        src,
                    });
                }
            }
            Event::Start(MdTag::CodeBlock(kind)) => {
                let lang = match kind {
                    pulldown_cmark::CodeBlockKind::Fenced(l) if !l.is_empty() => {
                        Some(l.to_string())
                    }
                    _ => None,
                };
                stack.push(BlockFrame::Code {
                    lang,
                    fence: range,
                    body: String::new(),
                    body_range: None,
                });
            }
            Event::Text(t) => {
                if let Some(BlockFrame::Code {
                    body, body_range, ..
                }) = stack.last_mut()
                {
                    body.push_str(&t);
                    *body_range = Some(match body_range.take() {
                        Some(br) => br.start..range.end,
                        None => range,
                    });
                } else {
                    observe_inline(&mut stack, &range);
                }
            }
            Event::End(TagEnd::CodeBlock) => {
                if let Some(BlockFrame::Code {
                    lang,
                    fence,
                    body,
                    body_range,
                }) = stack.pop()
                {
                    let lines = split_code_lines(&body, body_range);
                    blocks.push(MdBlock::Code { lang, lines, fence });
                }
            }
            Event::Start(MdTag::Table(aligns)) => stack.push(BlockFrame::Table {
                aligns: aligns
                    .iter()
                    .map(|a| match a {
                        pulldown_cmark::Alignment::Center => Align::Center,
                        pulldown_cmark::Alignment::Right => Align::Right,
                        _ => Align::Left,
                    })
                    .collect(),
                src: range,
                header: Vec::new(),
                rows: Vec::new(),
                cur: Vec::new(),
                in_head: false,
                cell: None,
            }),
            Event::Start(MdTag::TableHead) => {
                if let Some(BlockFrame::Table { in_head, .. }) = stack.last_mut() {
                    *in_head = true;
                }
            }
            Event::End(TagEnd::TableHead) => {
                if let Some(BlockFrame::Table {
                    in_head,
                    header,
                    cur,
                    ..
                }) = stack.last_mut()
                {
                    *in_head = false;
                    *header = std::mem::take(cur);
                }
            }
            Event::End(TagEnd::TableRow) => {
                if let Some(BlockFrame::Table {
                    in_head, rows, cur, ..
                }) = stack.last_mut()
                {
                    if !*in_head {
                        rows.push(std::mem::take(cur));
                    }
                }
            }
            Event::Start(MdTag::TableCell) => {
                if let Some(BlockFrame::Table { cell, .. }) = stack.last_mut() {
                    *cell = Some((InlineExtent::new(), range));
                }
            }
            Event::End(TagEnd::TableCell) => {
                if let Some(BlockFrame::Table { cell, cur, .. }) = stack.last_mut() {
                    if let Some((cext, csrc)) = cell.take() {
                        let src = trimmed_range(text, &cext).unwrap_or(csrc);
                        cur.push(MdCell { src });
                    }
                }
            }
            Event::End(TagEnd::Table) => {
                if let Some(BlockFrame::Table {
                    aligns,
                    src,
                    header,
                    rows,
                    ..
                }) = stack.pop()
                {
                    blocks.push(MdBlock::Table {
                        header,
                        aligns,
                        rows,
                        src,
                    });
                }
            }
            Event::Start(MdTag::BlockQuote(_)) => stack.push(BlockFrame::Quote { src: range }),
            Event::End(TagEnd::BlockQuote(_)) => {
                if let Some(BlockFrame::Quote { src }) = stack.pop() {
                    let lines = quote_lines(text, &src, t, base);
                    blocks.push(MdBlock::Quote { lines, src });
                }
            }
            // Lists are rendered line-preserving: only the outermost list frame
            // is captured (nested lists fall inside its range), and on close the
            // whole range is split into per-line paragraphs keeping the literal
            // markers and indentation.
            Event::Start(MdTag::List(_)) => {
                match stack.last_mut() {
                    // Already inside a list or quote: count the nested list so
                    // its End does not close the outer frame.
                    Some(BlockFrame::List { depth, .. }) => *depth += 1,
                    Some(BlockFrame::Quote { .. }) => {}
                    _ => stack.push(BlockFrame::List {
                        src: range,
                        depth: 0,
                    }),
                }
            }
            Event::End(TagEnd::List(_)) => {
                if let Some(BlockFrame::List { src, depth }) = stack.last_mut() {
                    if *depth > 0 {
                        *depth -= 1;
                    } else {
                        let src = src.clone();
                        stack.pop();
                        split_paragraph_lines(text, &src, t, base, &mut blocks);
                    }
                }
            }
            _ => observe_inline(&mut stack, &range),
        }
    }
    preserve_lines(text, blocks, t, base)
}

/// The source byte range a block occupies.
fn block_src(b: &MdBlock) -> std::ops::Range<usize> {
    match b {
        MdBlock::Blank { src }
        | MdBlock::Paragraph { src, .. }
        | MdBlock::Code { fence: src, .. }
        | MdBlock::Heading { src, .. }
        | MdBlock::Table { src, .. }
        | MdBlock::Quote { src, .. } => src.clone(),
    }
}

/// Restore source-line granularity over the pulldown block structure.
///
/// pulldown is structure-semantic: it collapses blank lines and merges
/// soft-wrapped lines into a single paragraph. The transcript renderer is
/// line-preserving — a user's blank lines and explicit line breaks must each
/// occupy their own row (the user/agent rail is drawn per source line). This
/// pass walks the emitted blocks against the raw source and:
/// - inserts a `Blank` block for every run of empty source lines between or
///   around blocks, and
/// - splits any paragraph that spans multiple source lines into one paragraph
///   per line (pulldown still owns the inline parse of each line).
///
/// Block classification remains entirely pulldown's; this only enforces line
/// granularity for rendering.
fn preserve_lines(text: &str, blocks: Vec<MdBlock>, t: Theme, base: Style) -> Vec<MdBlock> {
    let mut out: Vec<MdBlock> = Vec::new();
    // `cursor` tracks the byte just past the prior block's content. Block
    // ranges from pulldown inconsistently include a trailing newline, so the
    // cursor is normalized to the end of the block's last content line
    // (trailing newlines excluded) before each gap is measured.
    let mut cursor = 0usize;
    for block in blocks {
        let src = block_src(&block);
        // Fill any blank-line gap before this block.
        emit_blanks(text, cursor, src.start, &mut out);
        // Split multi-line paragraphs into per-line paragraphs.
        match block {
            MdBlock::Paragraph { src, .. } => {
                split_paragraph_lines(text, &src, t, base, &mut out);
            }
            other => out.push(other),
        }
        cursor = content_end(text, &src);
    }
    // Trailing blank lines after the last block.
    emit_blanks(text, cursor, text.len(), &mut out);
    out
}

/// The byte offset just past a block's last content character, excluding any
/// trailing newline(s) pulldown folded into the block range.
fn content_end(text: &str, src: &std::ops::Range<usize>) -> usize {
    let mut end = src.end;
    while end > src.start && text.as_bytes()[end - 1] == b'\n' {
        end -= 1;
    }
    end
}

/// Emit a `Blank` block for each empty source line in `text[start..end]`,
/// tracking byte offsets so each blank maps to its (empty) source line.
fn emit_blanks(text: &str, start: usize, end: usize, out: &mut Vec<MdBlock>) {
    if start >= end {
        return;
    }
    // The gap between two blocks opens with the newline terminating the prior
    // block's last line; that separator is not itself a blank line. Each
    // additional newline introduces one blank line.
    let gap = &text[start..end];
    let newlines = gap.bytes().filter(|&b| b == b'\n').count();
    let blank_count = newlines.saturating_sub(1);
    // Byte offset of each blank line: walk the gap's lines, skipping the first
    // fragment (the separator's empty head) and the last (the next block's line
    // start), emitting one Blank per interior empty line.
    let mut off = start;
    let mut emitted = 0usize;
    for (i, line) in gap.split('\n').enumerate() {
        let line_end = off + line.len();
        if i > 0 && emitted < blank_count {
            out.push(MdBlock::Blank { src: off..line_end });
            emitted += 1;
        }
        off = line_end + 1;
    }
}

/// Split a paragraph block that spans multiple source lines into one
/// `Paragraph` per line, preserving each line's source range and inline parse.
fn split_paragraph_lines(
    text: &str,
    src: &std::ops::Range<usize>,
    t: Theme,
    base: Style,
    out: &mut Vec<MdBlock>,
) {
    // Block ranges may fold in a trailing newline; strip it so the split does
    // not yield a phantom empty final line.
    let block = text[src.clone()].trim_end_matches('\n');
    if !block.contains('\n') {
        // Single line: keep spans as analyzed.
        let spans = line_spans(block.trim_end(), t, base);
        let mut spans = spans;
        for m in &mut spans {
            m.content_start += src.start;
            m.boundary_start += src.start;
        }
        if !spans_text(&spans).trim().is_empty() {
            out.push(MdBlock::Paragraph {
                spans,
                src: src.clone(),
            });
        }
        return;
    }
    let mut off = src.start;
    for line in block.split('\n') {
        let trimmed = line.trim_end();
        let line_range = off..off + trimmed.len();
        let spans = line_spans(trimmed, t, base);
        let mut spans = spans;
        for m in &mut spans {
            m.content_start += off;
            m.boundary_start += off;
        }
        if trimmed.trim().is_empty() {
            out.push(MdBlock::Blank {
                src: off..off + line.len(),
            });
        } else {
            out.push(MdBlock::Paragraph {
                spans,
                src: line_range,
            });
        }
        off += line.len() + 1;
    }
}

/// Byte length of the list marker (bullet or ordered) plus its trailing
/// whitespace at the start of `line`, after any indentation. Returns 0 when
/// the line does not open with a list marker.
fn list_marker_len(line: &str) -> usize {
    let indent = line.len() - line.trim_start_matches([' ', '\t']).len();
    let rest = &line[indent..];
    let after_bullet = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "));
    if let Some(r) = after_bullet {
        return indent + (rest.len() - r.len());
    }
    // Ordered list: `N.` or `N)` followed by a space.
    let digit_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digit_len > 0 {
        if let Some(r) = rest[digit_len..]
            .strip_prefix(". ")
            .or_else(|| rest[digit_len..].strip_prefix(") "))
        {
            return indent + (rest.len() - r.len());
        }
    }
    0
}

/// Parse one source line into spans, preserving a leading list marker as
/// literal text. The renderer is line-preserving: a list line such as
/// `- item` must keep its `- ` prefix on screen, but `pulldown`'s block
/// parser consumes the marker, so re-parsing the whole line would drop it.
/// The marker is emitted verbatim and only the item body is inline-parsed.
fn line_spans(line: &str, t: Theme, base: Style) -> Vec<MappedSpan> {
    let marker_len = list_marker_len(line);
    if marker_len == 0 {
        return inline_spans_mapped(line, t, base);
    }
    let marker = &line[..marker_len];
    let body = &line[marker_len..];
    let mut spans = Vec::with_capacity(2);
    spans.push(MappedSpan {
        span: Span::styled(marker.to_string(), Style::new().fg(t.muted)),
        content_start: 0,
        boundary_start: 0,
        link: None,
    });
    if !body.is_empty() {
        for m in inline_spans_mapped(body, t, base) {
            spans.push(MappedSpan {
                span: m.span,
                content_start: m.content_start + marker_len,
                boundary_start: m.boundary_start + marker_len,
                link: m.link,
            });
        }
    }
    spans
}

/// Split a fenced code body into per-source-line strings with byte ranges.
/// `body_range` is the concatenated body text range; when absent (empty block)
/// there are no body lines.
fn split_code_lines(
    body: &str,
    body_range: Option<std::ops::Range<usize>>,
) -> Vec<(String, std::ops::Range<usize>)> {
    let Some(br) = body_range else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let base = br.start;
    let mut line_start = 0usize;
    for (i, &b) in body.as_bytes().iter().enumerate() {
        if b == b'\n' {
            out.push((body[line_start..i].to_string(), base + line_start..base + i));
            line_start = i + 1;
        }
    }
    if line_start < body.len() {
        out.push((
            body[line_start..].to_string(),
            base + line_start..base + body.len(),
        ));
    }
    out
}

/// Build per-source-line quote content from the quote block source range. Each
/// line keeps its `> ` prefix length (for yank) and inline-parsed body.
fn quote_lines(
    text: &str,
    src: &std::ops::Range<usize>,
    t: Theme,
    base: Style,
) -> Vec<MdQuoteLine> {
    // The quote block range may include a trailing newline; strip it so
    // `split` does not yield a phantom empty final line.
    let block = text[src.clone()].trim_end_matches('\n');
    let mut out = Vec::new();
    let mut off = src.start;
    for line in block.split('\n') {
        let trimmed = line.trim_end();
        let line_len = trimmed.len();
        let prefix_len = if trimmed == ">" {
            1
        } else if let Some(rest) = trimmed.strip_prefix("> ") {
            line_len - rest.len()
        } else {
            0
        };
        let body = &trimmed[prefix_len..];
        let spans = if body.is_empty() {
            Vec::new()
        } else {
            let mut s = inline_spans_mapped(body, t, base);
            for m in &mut s {
                m.content_start += off + prefix_len;
                m.boundary_start += off + prefix_len;
            }
            s
        };
        out.push(MdQuoteLine {
            spans,
            src: off..off + line_len,
            prefix_len,
        });
        off += line.len() + 1;
    }
    out
}
/// Count the visual rows `render_markdown_body` would emit for `text`, without
/// allocating any styled lines. `Component::height` for the markdown-heavy
/// components routes here so measuring a resumed transcript's heights does not
/// build and immediately discard thousands of `RenderLine` graphs — that
/// transient was the dominant startup/resize allocation. The row arithmetic
/// must mirror `render_markdown_body` exactly; `markdown_height_matches_lines`
/// (test) guards against drift.
fn markdown_body_height(text: &str, content_w: usize) -> usize {
    if text.is_empty() {
        return 0;
    }
    // Walk the same block tree the renderer emits, counting rows with the
    // exact wrap calls each emitter uses but allocating no styled lines. This
    // keeps `height == render(...).len()` structural rather than maintained by
    // a parallel line-scanner.
    let t = Theme::default();
    let base = Style::default();
    let mut row = 0usize;
    for block in analyze(text, t, base) {
        match block {
            MdBlock::Blank { .. } => row += 1,
            MdBlock::Code { lines, .. } => {
                // Opening fence + body rows + closing fence.
                row += 1;
                for (line_text, _) in &lines {
                    row += prim::wrap_pre(line_text, content_w.saturating_sub(2)).len();
                }
                row += 1;
            }
            MdBlock::Table {
                header,
                aligns,
                rows,
                ..
            } => {
                let cell_text = |c: &MdCell| text[c.src.clone()].trim().to_string();
                let header_txt: Vec<String> = header.iter().map(&cell_text).collect();
                let data_txt: Vec<Vec<String>> = rows
                    .iter()
                    .map(|r| r.iter().map(&cell_text).collect())
                    .collect();
                row += table_height_cells(&header_txt, &data_txt, &aligns, content_w);
            }
            MdBlock::Heading {
                content_start, src, ..
            } => {
                let trimmed = text[src.clone()].trim_end();
                let h = text[content_start..src.end].trim_end();
                row += wrap_with_map(h, content_start - src.start, trimmed.len(), content_w).len();
            }
            MdBlock::Quote { lines, .. } => {
                for ql in &lines {
                    if ql.spans.is_empty() {
                        row += 1;
                        continue;
                    }
                    let qtrimmed = text[ql.src.clone()].trim_end();
                    let body = &qtrimmed[ql.prefix_len..];
                    row += wrap_with_map(
                        body,
                        ql.prefix_len,
                        qtrimmed.len(),
                        content_w.saturating_sub(2),
                    )
                    .len();
                }
            }
            MdBlock::Paragraph { src, .. } => {
                let raw = text[src.clone()].trim_end();
                // Match the render path exactly: `line_spans` keeps a leading
                // list marker literal, so a list line wraps to the same width.
                let mapped = line_spans(raw, t, base);
                let spans: Vec<Span> = mapped.into_iter().map(|m| m.span).collect();
                let line = Line::from(spans);
                row += prim::wrap_line_styled(&line, content_w).len();
            }
        }
    }
    row
}
/// Count the visual rows a table would occupy, mirroring `render_table` +
/// `table_row` without allocating the styled grid. Used by `markdown_body_height`.
/// Operates on the same parsed cell set as `render_table`, so both agree on the
/// grid even when pulldown classifies adjacent `|`-lines as one table.
fn table_height_cells(
    header: &[String],
    data: &[Vec<String>],
    _aligns: &[Align],
    content_w: usize,
) -> usize {
    let n_cols = header.len();
    if n_cols == 0 {
        return 0;
    }
    let t = Theme::default();
    let base = Style::new().fg(t.fg);

    // Column widths, identical to render_table.
    let mut col_w = vec![0usize; n_cols];
    for (i, cell) in header.iter().enumerate() {
        col_w[i] = col_w[i].max(rendered_width(cell, t, base));
    }
    for row in data {
        for (i, cell) in row.iter().enumerate().take(n_cols) {
            col_w[i] = col_w[i].max(rendered_width(cell, t, base));
        }
    }
    let table_w = content_w.saturating_sub(2);
    let total: usize = col_w.iter().map(|&w| w + 2).sum::<usize>() + n_cols + 1;
    if total > table_w {
        let overhead = n_cols * 2 + n_cols + 1;
        let avail = table_w.saturating_sub(overhead).max(n_cols);
        let natural = col_w.iter().sum::<usize>().max(1);
        let mut assigned = 0usize;
        for cw in col_w.iter_mut().take(n_cols - 1) {
            *cw = (*cw * avail / natural).max(1);
            assigned += *cw;
        }
        col_w[n_cols - 1] = avail.saturating_sub(assigned).max(1);
    }

    // A row group occupies the tallest wrapped cell across its columns.
    let row_group = |cells: &[String]| -> usize {
        col_w
            .iter()
            .enumerate()
            .map(|(i, &w)| {
                let cell = cells.get(i).map_or("", String::as_str);
                let (spans, _) = inline_spans_and_links(cell, t, base);
                prim::wrap_line_styled(&Line::from(spans), w).len()
            })
            .max()
            .unwrap_or(1)
            .max(1)
    };

    let mut rows = 1; // top border
    rows += row_group(header);
    rows += 1; // header separator
    for (i, row) in data.iter().enumerate() {
        rows += row_group(row);
        if i + 1 < data.len() {
            rows += 1; // interior border
        }
    }
    rows + 1 // bottom border
}

fn inline_spans(line: &str, t: Theme, base: Style) -> Vec<Span<'static>> {
    inline_spans_and_links(line, t, base).0
}

fn inline_spans_and_links(
    line: &str,
    t: Theme,
    base: Style,
) -> (Vec<Span<'static>>, Vec<Hyperlink>) {
    let mapped = inline_spans_mapped(line, t, base);
    let links = mapped_hyperlinks(&mapped);
    let spans = mapped.into_iter().map(|mapped| mapped.span).collect();
    (spans, links)
}

/// A styled span paired with the byte offsets it occupies in the source
/// line, so a display selection can be mapped back to the raw markdown.
/// `content_start` is where the span's text begins; `boundary_start` is
/// where a selection *starting* at this span should slice from — the
/// position of any opening marker (`**`, backtick) preceding the content,
/// so wrapped markers come along with the selection.
#[derive(Clone)]
struct MappedSpan {
    span: Span<'static>,
    content_start: usize,
    boundary_start: usize,
    link: Option<Arc<str>>,
}

/// Render one source line to styled spans with source-offset tracking, using
/// `pulldown-cmark` for `CommonMark` inline parsing. Unterminated markers stay
/// literal (the parser emits them as text until a closer exists), so a
/// still-growing streamed line keeps the same laid-out width until its span
/// closes — the marker characters, never a reflow of settled rows.
fn inline_spans_mapped(line: &str, t: Theme, base: Style) -> Vec<MappedSpan> {
    let code_style = Style::new().fg(t.info).bg(t.inline_bg);
    let mut out: Vec<MappedSpan> = Vec::new();
    // Active inline formatting, outermost first. Each entry carries the byte
    // offset of its opening marker so nested content snaps its selection
    // boundary to the outermost open, plus the modifier and link it applies.
    let mut fmt_stack: Vec<(usize, Modifier, Option<Arc<str>>)> = Vec::new();

    let current_style = |fmt_stack: &[(usize, Modifier, Option<Arc<str>>)]| {
        fmt_stack
            .iter()
            .fold(base, |style, (_, m, _)| style.add_modifier(*m))
    };
    let current_link = |fmt_stack: &[(usize, Modifier, Option<Arc<str>>)]| {
        fmt_stack.iter().rev().find_map(|(_, _, l)| l.clone())
    };
    let boundary_of = |fmt_stack: &[(usize, Modifier, Option<Arc<str>>)], fallback: usize| {
        fmt_stack.first().map_or(fallback, |(open, _, _)| *open)
    };

    let parser = MdParser::new_ext(line, MdOptions::ENABLE_STRIKETHROUGH);
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(tag) => {
                let (modifier, link) = match &tag {
                    MdTag::Strong => (Some(Modifier::BOLD), None),
                    MdTag::Emphasis => (Some(Modifier::ITALIC), None),
                    MdTag::Strikethrough => (Some(Modifier::CROSSED_OUT), None),
                    MdTag::Link { dest_url, .. } => {
                        // Only hyperlink safe URLs (non-empty, no control or
                        // whitespace); an unsafe destination renders as plain
                        // underlined text without a clickable target.
                        let url = dest_url.as_ref();
                        let link = is_safe_link_url(url).then(|| Arc::from(url));
                        (Some(Modifier::UNDERLINED), link)
                    }
                    _ => (None, None),
                };
                if modifier.is_some() || link.is_some() {
                    fmt_stack.push((range.start, modifier.unwrap_or(Modifier::empty()), link));
                }
            }
            Event::End(tag) => {
                let pops = match tag {
                    TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough | TagEnd::Link => 1,
                    _ => 0,
                };
                for _ in 0..pops {
                    fmt_stack.pop();
                }
            }
            Event::Code(code) => {
                // Content-only (no backticks, no padding): the tile width is
                // stable across the span boundary.
                out.push(MappedSpan {
                    span: Span::styled(code.to_string(), code_style),
                    content_start: range.start + 1,
                    boundary_start: range.start,
                    link: None,
                });
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => {
                if text.is_empty() {
                    continue;
                }
                out.push(MappedSpan {
                    span: Span::styled(text.to_string(), current_style(&fmt_stack)),
                    content_start: range.start,
                    boundary_start: boundary_of(&fmt_stack, range.start),
                    link: current_link(&fmt_stack),
                });
            }
            Event::SoftBreak | Event::HardBreak => {
                out.push(MappedSpan {
                    span: Span::raw(" "),
                    content_start: range.start,
                    boundary_start: boundary_of(&fmt_stack, range.start),
                    link: current_link(&fmt_stack),
                });
            }
            _ => {}
        }
    }
    nonempty_mapped(out)
}

fn nonempty_mapped(mut spans: Vec<MappedSpan>) -> Vec<MappedSpan> {
    if spans.is_empty() {
        spans.push(MappedSpan {
            span: Span::raw(String::new()),
            content_start: 0,
            boundary_start: 0,
            link: None,
        });
    }
    spans
}

fn build_content_map(source_len: usize, spans: &[MappedSpan]) -> Vec<usize> {
    let mut full = Vec::new();
    for m in spans {
        let c = m.span.content.chars().count();
        full.push(m.boundary_start);
        let chars: Vec<(usize, char)> = m.span.content.char_indices().collect();
        for k in 1..c {
            let off = chars.get(k).map_or(m.span.content.len(), |(b, _)| *b);
            full.push(m.content_start + off);
        }
    }
    full.push(source_len);
    // Drop leading-whitespace positions so the map is content-relative
    // (render excludes leading whitespace from the selectable range).
    let lead_ws = spans
        .iter()
        .flat_map(|m| m.span.content.chars())
        .take_while(|c| *c == ' ' || *c == '\t')
        .count();
    if lead_ws < full.len() {
        full.drain(..lead_ws);
    }
    full
}

fn split_map_by_rows(
    full_map: &[usize],
    lead_ws: usize,
    rows: &[ratatui::text::Line<'static>],
) -> Vec<Vec<usize>> {
    let mut out = Vec::with_capacity(rows.len());
    let mut pos = 0usize;
    let mut first = true;
    for row in rows {
        let display_len: usize = row.spans.iter().map(|s| s.content.chars().count()).sum();
        let row_lead = if first { lead_ws } else { 0 };
        let content_len = display_len.saturating_sub(row_lead);
        let end = (pos + content_len + 1).min(full_map.len());
        out.push(full_map[pos..end].to_vec());
        pos += content_len;
        first = false;
    }
    out
}

fn collapse(source: &str) -> (String, Vec<usize>) {
    let mut out = String::new();
    let mut map = Vec::new();
    let mut byte_pos = 0usize;
    let mut prev_end: Option<usize> = None;
    for word in source.split(' ') {
        if word.is_empty() {
            byte_pos += 1;
            continue;
        }
        if let Some(pe) = prev_end {
            out.push(' ');
            map.push(pe);
        }
        for (b, c) in word.char_indices() {
            out.push(c);
            map.push(byte_pos + b);
        }
        byte_pos += word.len();
        prev_end = Some(byte_pos);
        byte_pos += 1;
    }
    (out, map)
}

fn wrap_with_map(
    content: &str,
    prefix_len: usize,