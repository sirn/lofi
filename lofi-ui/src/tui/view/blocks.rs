#![allow(clippy::needless_lifetimes)]

//! Concrete log components and the turn-log orchestrator.
//!
//! Each component renders to an owned block of lines via [`Component`]; the
//! primitives in [`super::prim`] supply padding, rails, and styled spans.
//! [`render_turns`] builds a [`Stack`] per turn and joins turns with blanks.

use std::sync::Arc;
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::tui::theme::{active_indicator, agent_indicator, user_indicator, Theme};
use crate::tui::{App, Block, NativeTool, ThinkingBlock, ToolCall, Turn};

use super::component::{Component, Cx, Stack};
use super::prim::{self, RawLine, RenderLine};

/// Lines of preview/output shown before truncating with `… (N hidden)`.
const PREVIEW_LINES: usize = 3;

/// Counts all component rows while retaining only a requested window.
struct LineWindow {
    range: std::ops::Range<usize>,
    total: usize,
    lines: Vec<RenderLine>,
}

impl LineWindow {
    fn new(range: std::ops::Range<usize>) -> Self {
        Self { lines: Vec::with_capacity(range.end.saturating_sub(range.start).min(256)), range, total: 0 }
    }
    fn push(&mut self, line: RenderLine) {
        if self.range.contains(&self.total) { self.lines.push(line); }
        self.total = self.total.saturating_add(1);
    }
    fn extend(&mut self, lines: impl IntoIterator<Item = RenderLine>) {
        for line in lines { self.push(line); }
    }
    fn component(&mut self, child: &dyn Component, cx: &Cx) {
        let height = child.height(cx);
        let end = self.total.saturating_add(height);
        if end > self.range.start && self.total < self.range.end {
            let start = self.range.start.saturating_sub(self.total);
            let stop = self.range.end.saturating_sub(self.total).min(height);
            self.lines.extend(child.lines_window(cx, start..stop));
        }
        self.total = end;
    }
}

/// Build the whole turn log as a single [`Text`], turns separated by blanks.
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
        out.extend(render_turn_lines(&cx, turn).into_iter().map(|rl| rl.line));
    }
    Text::from(out)
}

/// Render a single turn's blocks to owned lines, each tagged with the char
/// range of its selectable content. Shared by the full-log builder
/// ([`render_turns`]) and the viewport renderer's per-turn cache: the cache
/// stores the result for frozen turns and rebuilds only the live last turn
/// each frame.
pub fn render_turn_lines(cx: &Cx, turn: &Turn) -> Vec<RenderLine> {
    let mut stack = Stack::new();
    if !turn.prompt.is_empty() {
        stack.push(UserMessage {
            prompt: &turn.prompt,
        });
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
                // A provider stream error is emitted twice: once as
                // AgentEvent::Error (rendered as a fatal line via ErrorLine)
                // and again here as the turn's `error` ("provider error:
                // <msg>"). When the turn already carries a fatal-error
                // block the message is already on screen, so drop it here to
                // avoid duplicating it below the `failed in Ns` header.
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
            Block::Compaction {
                summarized,
                kept,
                summary,
            } => {
                stack.push(CompactionLine {
                    summarized: *summarized,
                    kept: *kept,
                    summary: summary.clone(),
                });
            }
        }
    }
    stack.lines(cx)
}

// ── User message ─────────────────────────────────────────────────────────

/// A user message marked by a user-colored rail on every visual row.
/// It has no tile background or internal top/bottom padding; separation from
/// adjacent response blocks remains the enclosing Stack's responsibility.
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

// ── Assistant text ───────────────────────────────────────────────────────

/// Assistant response text marked by an agent-colored rail on every visual
/// row. Thinking and tool-call components deliberately do not use this rail.
/// Markdown-lite: headings bold (all six levels),
/// blockquotes dim, inline `code` on a tile, `**bold**`, `*italic*`,
/// `_underline_`, `~~strike~~`, fenced code as a plain triple-backtick fence
/// on a full-width surface tile, and `|`-delimited tables as box-drawn grids.
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
}

/// Shared markdown-lite renderer used by both user prompts and assistant
/// text. `lead_fn(i)` produces the decoration spans for each output row
/// (for example, the role-colored message rail). Markdown
/// parsing: headings, blockquotes, fenced code blocks, `|`-delimited tables,
/// and inline formatting (`code`, `**bold**`, `*italic*`, `_underline_`,
/// `~~strike~~`).
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
    let mut in_code = false;
    let lines: Vec<&str> = text.split('\n').collect();
    let mut idx = 0;
    while idx < lines.len() {
        let raw = lines[idx];
        let trimmed = raw.trim_end();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            let lang = trimmed.trim_start_matches('`');
            let label = if in_code {
                if lang.is_empty() {
                    "```".to_string()
                } else {
                    format!("```{lang}")
                }
            } else {
                "```".to_string()
            };
            out.push(
                prim::rtile(
                    lead_fn(row),
                    vec![Span::styled(label, Style::new().fg(t.muted).bg(t.surface))],
                    t.surface,
                    w,
                )
                .with_raw(RawLine::linear(
                    Arc::from(trimmed),
                    0,
                    trimmed.chars().count(),
                    true,
                )),
            );
            row += 1;
            idx += 1;
            continue;
        }
        if in_code {
            let avail = content_w.saturating_sub(2);
            let src: Arc<str> = Arc::from(raw);
            let indent_len = raw.bytes().take_while(|&b| b == b' ' || b == b'\t').count();
            let body = &raw[indent_len..];
            // Byte offsets of each body char boundary (0, after 1st, …, end).
            let body_offs: Vec<usize> = std::iter::once(0)
                .chain(body.char_indices().map(|(b, c)| b + c.len_utf8()))
                .collect();
            let indent_chars = raw[..indent_len].chars().count();
            let segments = prim::wrap_pre(raw, avail);
            let mut cum = 0usize;
            for (i, seg) in segments.into_iter().enumerate() {
                let body_chars = seg.chars().count().saturating_sub(indent_chars);
                let map: Vec<usize> = (0..=body_chars)
                    .map(|k| indent_len + body_offs.get(cum + k).copied().unwrap_or(body.len()))
                    .collect();
                out.push(
                    prim::rtile(
                        lead_fn(row),
                        vec![Span::styled(seg, Style::new().fg(t.fg).bg(t.surface))],
                        t.surface,
                        w,
                    )
                    .with_raw(RawLine::new(src.clone(), map, i == 0)),
                );
                row += 1;
                cum += body_chars;
            }
            idx += 1;
            continue;
        }
        // Markdown table: a `|`-row whose next line is a separator.
        if trimmed.starts_with('|')
            && idx + 1 < lines.len()
            && is_table_separator(lines[idx + 1].trim())
        {
            let start = idx;
            while idx < lines.len() && lines[idx].trim().starts_with('|') {
                idx += 1;
            }
            let tlines = &lines[start..idx];
            let header = parse_table_row(tlines[0]);
            let aligns: Vec<Align> = parse_table_row(tlines[1])
                .iter()
                .map(|c| parse_align(c))
                .collect();
            let data: Vec<Vec<String>> = tlines[2..].iter().map(|l| parse_table_row(l)).collect();
            // Source lines for raw markdown yank: header, separator,
            // then data rows.
            let src_lines: Vec<&str> = tlines.to_vec();
            let before = out.len();
            out.extend(render_table(
                &header, &data, &aligns, content_w, t, &src_lines, row, &lead_fn,
            ));
            row += out.len() - before;
            continue;
        }
        let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) && trimmed.as_bytes().get(hashes) == Some(&b' ') {
            let h = &trimmed[hashes + 1..];
            let head_fg = if hashes <= 2 {
                base_style.fg.unwrap_or(t.fg)
            } else {
                t.muted
            };
            let style = Style::new().fg(head_fg).add_modifier(Modifier::BOLD);
            let src: Arc<str> = Arc::from(trimmed);
            let rows = wrap_with_map(h, hashes + 1, trimmed.len(), content_w);
            for (i, (seg, map)) in rows.into_iter().enumerate() {
                out.push(
                    prim::rline(lead_fn(row), vec![Span::styled(seg, style)])
                        .with_raw(RawLine::new(src.clone(), map, i == 0)),
                );
                row += 1;
            }
        } else if trimmed == ">" || trimmed.starts_with("> ") {
            // Group consecutive blockquote lines.
            let start = idx;
            while idx < lines.len()
                && (lines[idx].trim() == ">" || lines[idx].trim().starts_with("> "))
            {
                idx += 1;
            }
            let q_lines = &lines[start..idx];
            let quote_style = Style::new().fg(t.muted);
            let bar = Span::styled("▎ ", Style::new().fg(t.subtle));
            for qraw in q_lines {
                let qtrimmed = qraw.trim_end();
                // Strip "> " or treat a bare ">" as an empty body line.
                let body = qtrimmed.strip_prefix("> ").unwrap_or_default();
                let src: Arc<str> = Arc::from(qtrimmed);
                if body.is_empty() {
                    // Empty quote line (bare ">"): just the bar.  Build
                    // the RenderLine directly so the content span is
                    // non-empty (a thin space) with a 2-entry map snapping
                    // to the full ">" source — selection_text yanks the
                    // raw markdown.
                    let map = vec![0, src.len()];
                    let mut lead = lead_fn(row);
                    lead.push(bar.clone());
                    let deco_len: usize = lead.iter().map(|s| s.content.chars().count()).sum();
                    let content_span = Span::styled("\u{2009}", quote_style); // thin space
                    let content_len = content_span.content.chars().count();
                    let mut all = lead.clone();
                    all.push(content_span);
                    out.push(RenderLine {
                        line: Line::from(all),
                        content: (deco_len, deco_len + content_len),
                        raw: Some(RawLine::new(src, map, true)),
                    });
                    row += 1;
                    continue;
                }
                let prefix_len = qtrimmed.len() - body.len(); // length of "> " or ">"
                let rows = wrap_with_map(
                    body,
                    prefix_len,
                    qtrimmed.len(),
                    content_w.saturating_sub(2),
                );
                for (i, (seg, map)) in rows.into_iter().enumerate() {
                    let mut lead = lead_fn(row);
                    lead.push(bar.clone());
                    out.push(
                        prim::rline(lead, vec![Span::styled(seg, quote_style)])
                            .with_raw(RawLine::new(src.clone(), map, i == 0)),
                    );
                    row += 1;
                }
            }
            continue;
        } else {
            let mapped = inline_spans_mapped(raw, t, base_style);
            let lead_ws = mapped
                .iter()
                .flat_map(|m| m.span.content.chars())
                .take_while(|c| *c == ' ' || *c == '\t')
                .count();
            let full_map = build_content_map(raw.len(), &mapped);
            let spans: Vec<Span> = mapped.into_iter().map(|m| m.span).collect();
            let line = Line::from(spans);
            let src: Arc<str> = Arc::from(raw);
            let rows = prim::wrap_line_styled(&line, content_w);
            let row_maps = split_map_by_rows(&full_map, lead_ws, &rows);
            for (i, wrapped) in rows.into_iter().enumerate() {
                out.push(
                    prim::rline(lead_fn(row), wrapped.spans).with_raw(RawLine::new(
                        src.clone(),
                        row_maps.get(i).cloned().unwrap_or_default(),
                        i == 0,
                    )),
                );
                row += 1;
            }
        }
        idx += 1;
    }
    out
}

/// Split a line into styled spans, parsing inline markdown: `` `code` ``
/// (literal content on an inline-bg tile), `**bold**`, `__bold__`,
/// `*italic*`, `_underline_`, `~~strike~~`. Code spans are extracted first
/// (their content is literal); remaining text is recursively scanned for
/// marker pairs. Underscore markers are suppressed inside words so
/// identifiers like `my_var_name` stay literal.
fn inline_spans(line: &str, t: Theme, base: Style) -> Vec<Span<'static>> {
    inline_spans_mapped(line, t, base)
        .into_iter()
        .map(|m| m.span)
        .collect()
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
}

/// [`inline_spans`] with source-offset tracking. Returns mapped spans whose
/// styles and content are identical to [`inline_spans`]' output.
fn inline_spans_mapped(line: &str, t: Theme, base: Style) -> Vec<MappedSpan> {
    let code_style = Style::new().fg(t.info).bg(t.inline_bg);
    let mut out = Vec::new();
    let mut rest = line;
    let mut pos = 0usize;
    while let Some(start) = rest.find('`') {
        if start > 0 {
            out.extend(parse_markers_mapped(&rest[..start], pos, base, None));
        }
        let after = &rest[start + 1..];
        if let Some(end) = after.find('`') {
            // Pad the code tile with one space on each side (styled with the
            // same inline-bg) so adjacent text doesn't touch the tile and the
            // layout doesn't shift when a code span appears/disappears.
            let padded = format!(" {} ", &after[..end]);
            out.push(MappedSpan {
                span: Span::styled(padded, code_style),
                content_start: pos + start + 1,
                boundary_start: pos + start,
            });
            pos += start + 1 + end + 1;
            rest = &after[end + 1..];
        } else {
            out.extend(parse_markers_mapped(rest, pos, base, None));
            return nonempty_mapped(out);
        }
    }
    out.extend(parse_markers_mapped(rest, pos, base, None));
    nonempty_mapped(out)
}

/// [`parse_markers`] with source-offset tracking. `base` is the byte offset
/// of `text` within the source line; `pending_open` carries the byte offset
/// of an enclosing marker so the first span of an inner group snaps its
/// boundary to it (nested markers include the outermost open).
fn parse_markers_mapped(
    text: &str,
    base: usize,
    style: Style,
    pending_open: Option<usize>,
) -> Vec<MappedSpan> {
    for (marker, modifier, check) in [
        ("**", Modifier::BOLD, false),
        ("__", Modifier::BOLD, true),
        ("~~", Modifier::CROSSED_OUT, false),
        ("*", Modifier::ITALIC, false),
        ("_", Modifier::UNDERLINED, true),
    ] {
        let Some(open) = find_marker(text, marker, check, true) else {
            continue;
        };
        let after_open = &text[open + marker.len()..];
        if let Some(close) = find_marker(after_open, marker, check, false) {
            let before = &text[..open];
            let inner = &after_open[..close];
            let after = &after_open[close + marker.len()..];
            let open_pos = base + open;
            let inner_base = base + open + marker.len();
            let after_base = inner_base + close + marker.len();
            // The first span of `inner` snaps to the marker open. If `before`
            // is empty, inherit the enclosing pending_open so a nested
            // marker's outermost open is preserved.
            let inner_pending = if before.is_empty() {
                pending_open.or(Some(open_pos))
            } else {
                Some(open_pos)
            };
            let mut out = Vec::new();
            out.extend(parse_markers_mapped(before, base, style, pending_open));
            out.extend(parse_markers_mapped(
                inner,
                inner_base,
                style.add_modifier(modifier),
                inner_pending,
            ));
            out.extend(parse_markers_mapped(after, after_base, style, None));
            return out;
        }
    }
    if text.is_empty() {
        Vec::new()
    } else {
        vec![MappedSpan {
            span: Span::styled(text.to_string(), style),
            content_start: base,
            boundary_start: pending_open.unwrap_or(base),
        }]
    }
}

/// Ensure at least one span so empty input still produces a renderable row.
fn nonempty_mapped(mut spans: Vec<MappedSpan>) -> Vec<MappedSpan> {
    if spans.is_empty() {
        spans.push(MappedSpan {
            span: Span::raw(String::new()),
            content_start: 0,
            boundary_start: 0,
        });
    }
    spans
}

/// Build a content-relative display-position → source-byte-offset map from
/// mapped spans. Position 0 is the first selectable content char (after any
/// leading whitespace, which [`prim::render`] excludes from the content
/// range); position `len` is the source end. Marker gaps make the map
/// non-linear: e.g. for source `**bold**` the map is `[0, 3, 4, 5, 8]`, so
/// selecting the whole display "bold" slices `source[0..8]` = `**bold**`.
fn build_content_map(source_len: usize, spans: &[MappedSpan]) -> Vec<usize> {
    let mut full = Vec::new();
    for m in spans {
        let c = m.span.content.chars().count();
        full.push(m.boundary_start);
        let chars: Vec<(usize, char)> = m.span.content.char_indices().collect();
        for k in 1..c {
            // Boundary before the k-th char (0-indexed) = content_start +
            // the byte offset of that char within the span.
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

/// Split a full-line content map into per-row maps. `full_map` is
/// content-relative (leading whitespace already dropped) with
/// `content_len + 1` entries. `lead_ws` is the leading-whitespace char
/// count that [`prim::render`] excludes from row 0's selectable range;
/// subsequent rows have none. Each row's map is a contiguous slice of
/// `full_map`, so soft-wrap continuation rows are contiguous with their
/// neighbors (`row[i].map[last] == row[i+1].map[0]`).
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

/// Mirror [`prim::wrap`]'s whitespace collapsing: split on `' '`, drop empty
/// fragments, rejoin with single spaces. Returns the collapsed text and a
/// map from each collapsed char index to its source byte offset (the join
/// space maps to the first source space after the preceding word).
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

/// Wrap `content` like [`prim::wrap`] (whitespace-collapsing) and return each
/// row's text plus a content-relative position map. `prefix_len` offsets the
/// map so positions address `source` (which prefixes `content`, e.g. `## `);
/// `source_len` is the fallback for the final boundary.
fn wrap_with_map(
    content: &str,
    prefix_len: usize,
    source_len: usize,
    max_w: usize,
) -> Vec<(String, Vec<usize>)> {
    let (collapsed, cmap) = collapse(content);
    let rows = prim::wrap(&collapsed, max_w);
    let mut offset = 0usize;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let len = row.chars().count();
        let mut map = Vec::with_capacity(len + 1);
        for k in 0..len {
            let src = cmap.get(offset + k).copied().unwrap_or(content.len());
            map.push(prefix_len + src);
        }
        let last = cmap.get(offset + len).copied().unwrap_or(content.len());
        // `last` is a byte offset within `content`; offset by `prefix_len`
        // to address `source`, clamped to its end.
        map.push((prefix_len + last).min(source_len));
        // The first row's start boundary snaps to the source start so the
        // prefix (e.g. `## `, `> `) is included in whole-row yank — the
        // display strips it, but the raw markdown retains it. Continuation
        // rows keep their contiguous start (the next row's beginning).
        if out.is_empty() {
            map[0] = 0;
        }
        out.push((row, map));
        offset += len;
    }
    out
}

// ── Markdown table ───────────────────────────────────────────────────────

/// Column alignment inferred from the separator row (`:--`, `--:`, `:--:`).
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

/// A table separator line contains only `|`, `-`, `:`, and spaces, with at
/// least one dash.
fn is_table_separator(line: &str) -> bool {
    line.contains('-') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Split a `|`-delimited row into trimmed cell strings.
fn parse_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

/// Derive alignment from a separator cell (`:--` left, `--:` right, `:--:`
/// center, `---` default left).
fn parse_align(cell: &str) -> Align {
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

/// Render a markdown table with box-drawing borders. Column widths are the
/// max cell width per column; the last column is shrunk if the table would
/// exceed `content_w`.
fn render_table(
    header: &[String],
    data: &[Vec<String>],
    aligns: &[Align],
    content_w: usize,
    t: Theme,
    src_lines: &[&str],
    start_row: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) -> Vec<RenderLine> {
    let pad: Vec<Span<'static>> = vec![Span::raw("  ")];
    let n_cols = header.len();
    if n_cols == 0 {
        return Vec::new();
    }
    let base = Style::new().fg(t.fg);
    let mut col_w = vec![0usize; n_cols];
    for (i, cell) in header.iter().enumerate() {
        col_w[i] = col_w[i].max(rendered_width(cell, t, base));
    }
    for row in data {
        for (i, cell) in row.iter().enumerate().take(n_cols) {
            col_w[i] = col_w[i].max(rendered_width(cell, t, base));
        }
    }
    // Distribute available width proportionally across all columns when
    // the natural table width exceeds the content area. Two chars are
    // reserved for right padding so the table doesn't hug the edge.
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

    let border = Style::new().fg(t.subtle);
    let hdr_style = Style::new().fg(t.fg).add_modifier(Modifier::BOLD);
    let body_style = Style::new().fg(t.fg);
    let mut out = Vec::new();
    let mut tr = start_row;

    // Border row: left + (─×(w+2) + mid)×n + right
    let border_row = |left: char, mid: char, right: char, r: usize| -> RenderLine {
        let mut s = String::from(left);
        for (i, &cw) in col_w.iter().enumerate() {
            for _ in 0..cw + 2 {
                s.push('─');
            }
            s.push(if i + 1 < n_cols { mid } else { right });
        }
        // Border rows carry an empty-source raw so they are suppressed
        // in SELECT yank (no corresponding markdown source line).
        let mut rl = prim::render(lead_fn(r), vec![Span::styled(s, border)], pad.clone());
        rl.raw = Some(RawLine::new(Arc::from(""), Vec::new(), true));
        rl
    };

    out.push(border_row('┌', '┬', '┐', tr));
    tr += 1;
    // Attach the raw markdown source to the first row of each header/data
    // group so whole-line yank recovers the `| … |` line. The display grid
    // doesn't map 1:1 to the source, so the map is empty (degenerate).
    let hdr_rows = table_row(
        &col_w, header, aligns, hdr_style, border, &pad, t, tr, lead_fn,
    );
    let hdr_len = hdr_rows.len();
    let mut hdr_rows = hdr_rows;
    if let Some(first) = hdr_rows.first_mut() {
        if let Some(src) = src_lines.first() {
            first.raw = Some(RawLine::new(Arc::from(*src), Vec::new(), true));
        }
    }
    out.extend(hdr_rows);
    tr += hdr_len;
    // The header-separator border carries the markdown separator line
    // (`|---|---|`) so yanking it recovers the table-header syntax.
    let mut sep = border_row('├', '┼', '┤', tr);
    if let Some(src) = src_lines.get(1) {
        sep.raw = Some(RawLine::new(Arc::from(*src), Vec::new(), true));
    }
    out.push(sep);
    tr += 1;
    for (i, row) in data.iter().enumerate() {
        let rows = table_row(
            &col_w, row, aligns, body_style, border, &pad, t, tr, lead_fn,
        );
        let rows_len = rows.len();
        let mut rows = rows;
        if let Some(first) = rows.first_mut() {
            if let Some(src) = src_lines.get(2 + i) {
                first.raw = Some(RawLine::new(Arc::from(*src), Vec::new(), true));
            }
        }
        out.extend(rows);
        tr += rows_len;
        if i + 1 < data.len() {
            out.push(border_row('├', '┼', '┤', tr));
            tr += 1;
        }
    }
    out.push(border_row('└', '┴', '┘', tr));
    out
}

/// One data row: `│ cell │ cell │` with borders in `border` style and cells in
/// `style`. Each cell is parsed for inline markdown, wrapped to its column
/// width, and aligned; a row whose cells wrap to different line counts
/// produces one `RenderLine` per line, with shorter cells padded on
/// continuation rows.
#[allow(clippy::too_many_arguments)]
fn table_row(
    col_w: &[usize],
    cells: &[String],
    aligns: &[Align],
    style: Style,
    border: Style,
    pad: &[Span<'static>],
    t: Theme,
    start_row: usize,
    lead_fn: &impl Fn(usize) -> Vec<Span<'static>>,
) -> Vec<RenderLine> {
    let wrapped: Vec<Vec<Line<'static>>> = col_w
        .iter()
        .enumerate()
        .map(|(i, &w)| {
            let cell = cells.get(i).map_or("", String::as_str);
            let line = Line::from(inline_spans(cell, t, style));
            prim::wrap_line_styled(&line, w)
        })
        .collect();
    let max_lines = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let mut out = Vec::with_capacity(max_lines);
    for line_idx in 0..max_lines {
        let mut spans = vec![Span::styled("│", border)];
        for (i, &w) in col_w.iter().enumerate() {
            let cell_spans = wrapped[i]
                .get(line_idx)
                .map_or(Vec::new(), |l| l.spans.clone());
            let aligned = align_spans(
                cell_spans,
                w,
                aligns.get(i).copied().unwrap_or(Align::Left),
                style,
            );
            spans.push(Span::raw(" "));
            spans.extend(aligned);
            spans.push(Span::raw(" "));
            spans.push(Span::styled("│", border));
        }
        out.push(prim::render(
            lead_fn(start_row + line_idx),
            spans,
            pad.to_vec(),
        ));
    }
    out
}

/// Rendered width of a cell after stripping markdown markers.
fn rendered_width(cell: &str, t: Theme, base: Style) -> usize {
    inline_spans(cell, t, base)
        .iter()
        .map(|s| s.content.chars().count())
        .sum()
}

/// Pad a sequence of styled spans to exactly `w` chars per the alignment.
/// Spans that exceed `w` (trailing-space overflow from wrapping) are clipped.
fn align_spans(
    spans: Vec<Span<'static>>,
    w: usize,
    align: Align,
    pad_style: Style,
) -> Vec<Span<'static>> {
    let len: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    if len > w {
        let mut out = Vec::new();
        let mut remaining = w;
        for span in spans {
            let count = span.content.chars().count();
            if remaining == 0 {
                break;
            }
            if count <= remaining {
                out.push(span);
                remaining -= count;
            } else {
                let clipped: String = span.content.chars().take(remaining).collect();
                out.push(Span::styled(clipped, span.style));
                remaining = 0;
            }
        }
        return out;
    }
    let pad = w - len;
    match align {
        Align::Left => {
            let mut out = spans;
            out.push(Span::styled(" ".repeat(pad), pad_style));
            out
        }
        Align::Right => {
            let mut out = vec![Span::styled(" ".repeat(pad), pad_style)];
            out.extend(spans);
            out
        }
        Align::Center => {
            let left = pad / 2;
            let mut out = vec![Span::styled(" ".repeat(left), pad_style)];
            out.extend(spans);
            out.push(Span::styled(" ".repeat(pad - left), pad_style));
            out
        }
    }
}

/// Find the first occurrence of `marker` in `text`, optionally enforcing a
/// word-boundary check (for underscore markers so `my_var` stays literal).
/// `is_open` distinguishes opening (char before) from closing (char after).
fn find_marker(text: &str, marker: &str, check: bool, is_open: bool) -> Option<usize> {
    let mut search = 0;
    while let Some(rel) = text[search..].find(marker) {
        let pos = search + rel;
        if check {
            let ok = if is_open {
                text[..pos]
                    .chars()
                    .next_back()
                    .is_none_or(|c| !c.is_alphanumeric())
            } else {
                text[pos + marker.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric())
            };
            if !ok {
                search = pos + marker.len();
                continue;
            }
        }
        return Some(pos);
    }
    None
}

// ── Thinking ─────────────────────────────────────────────────────────────

/// Reasoning: muted italic text with a 2-space margin. While thinking a
/// trailing `Thinking...` pulses; once settled it becomes `Thought for Ns`.
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

// ── Exec tree ────────────────────────────────────────────────────────────

/// An `exec` block drawn as a plain tree without a background tile or
/// top/bottom padding. A two-cell left gutter aligns it with other transcript
/// blocks. Header `· Exec <label>` with a status-colored dot, the code with
/// line numbers behind a `│` rail, then each native tool branched off that rail, and a final
/// `└ ✓ Succeed`/`└ ✗ Failed` line with a result preview once done.
struct ExecBlock<'a> {
    tool: &'a ToolCall,
}

impl Component for ExecBlock<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> { self.render_window(cx, 0..usize::MAX).lines }
    fn height(&self, cx: &Cx) -> usize { self.render_window(cx, 0..0).total }
    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> { self.render_window(cx, range).lines }
}

impl ExecBlock<'_> {
    fn render_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> LineWindow {
        let t = cx.theme;
        let w = cx.width;
        let mut out = LineWindow::new(range);

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
        out.push(prim::rline(
            vec![
                Span::raw("  "),
                Span::styled("· ", Style::new().fg(status_color)),
            ],
            vec![Span::styled(
                header,
                Style::new().fg(t.fg).add_modifier(Modifier::BOLD),
            )],
        ));

        // Trim a trailing newline so a terminated command doesn't render an
        // empty rail line at the bottom of the code body.
        let code: Vec<&str> = self.tool.input.trim_end_matches('\n').split('\n').collect();
        let lw = code.len().to_string().len().max(3);
        let rail = Span::styled("│ ", Style::new().fg(t.subtle));
        let avail = w.saturating_sub(4).saturating_sub(lw + 1);
        for (i, line) in code.iter().enumerate() {
            let n = format!("{:>lw$} ", i + 1, lw = lw);
            let blank_n = " ".repeat(lw + 1);
            let body_style = Style::new().fg(t.fg);
            let num_style = Style::new().fg(t.subtle);
            // Wrap each command line preserving its indentation; the line
            // number labels the first row and a blank of the same width
            // aligns continuation rows under the body.
            for (j, seg) in prim::wrap_pre(line, avail).into_iter().enumerate() {
                let num_span = if j == 0 {
                    Span::styled(n.clone(), num_style)
                } else {
                    Span::styled(blank_n.clone(), num_style)
                };
                out.push(prim::rline(
                    vec![Span::raw("  "), rail.clone(), num_span],
                    vec![Span::styled(seg, body_style)],
                ));
            }
        }

        let n_total = self.tool.native.len();
        for (idx, nt) in self.tool.native.iter().enumerate() {
            // While the exec is still running the last native tool is the
            // tail (`└`); once done the final `└` is the exec-result line, so
            // every native tool becomes a `├`.
            let is_last = idx + 1 == n_total && !self.tool.done;
            out.component(&ExecBlockBranch { nt, is_last }, cx);
        }

        if self.tool.done {
            out.extend(exec_result_lines(self.tool, t, w, cx.app.verbose));
        }

        out
    }
}

/// The final `└ ✓ Succeed, took Ns` / `└ ✗ Failed, took Ns` branch with a
/// preview of the returned value or error.
fn exec_result_lines(tool: &ToolCall, t: Theme, w: usize, verbose: bool) -> Vec<RenderLine> {
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
    out.push(prim::rline(
        vec![
            Span::raw("  "),
            Span::styled("└ ", Style::new().fg(t.subtle)),
            Span::styled(format!("{icon} "), Style::new().fg(fg_color)),
        ],
        vec![Span::styled(summary, Style::new().fg(t.fg))],
    ));
    // In non-verbose mode, hide the final result body for a cleaner
    // transcript — the native-tool lines above already showed the work.
    // Keep it on error (and in verbose) so a failure is never swallowed.
    if !verbose && !tool.is_error {
        return out;
    }

    let Some(result) = &tool.result else {
        return out;
    };
    let display = lofi_core::exec_result_display(result, tool.is_error);
    if display.is_empty() {
        return out;
    }
    let indent = 2 + 2 + 2; // left gutter + branch column + own rail
    let all: Vec<&str> = display.trim_end_matches('\n').split('\n').collect();
    let limit = if verbose { all.len() } else { PREVIEW_LINES };
    let hidden = all.len().saturating_sub(limit);
    let avail = w.saturating_sub(indent);
    let body_fg = if tool.is_error { t.error } else { t.muted };
    let rail_deco = vec![
        Span::raw("  "),
        Span::raw("  "),
        Span::styled("│ ", Style::new().fg(t.subtle)),
    ];
    let body_style = Style::new().fg(body_fg);
    // Wrap each result line preserving its formatting; the rail repeats on
    // every continuation row. `hidden` counts logical lines, not wrapped
    // rows, so the `(N lines hidden)` cap stays accurate.
    for line in all.iter().take(limit) {
        for seg in prim::wrap_pre(line, avail) {
            out.push(prim::rline(
                rail_deco.clone(),
                vec![Span::styled(seg, body_style)],
            ));
        }
    }
    if hidden > 0 {
        let cap = format!("({hidden} lines hidden)");
        out.push(prim::rline(
            vec![
                Span::raw("  "),
                Span::raw("  "),
                Span::styled("… ", Style::new().fg(t.subtle)),
            ],
            vec![Span::styled(cap, Style::new().fg(t.subtle))],
        ));
    }
    out
}

// ── Exec block branch ───────────────────────────────────────────────────

/// One native tool call branched off the exec rail: a `├`/`└` header with a
/// status icon, then its result preview indented under a second rail.
struct ExecBlockBranch<'a> {
    nt: &'a NativeTool,
    /// Whether this is the tail of the exec tree (selects `└` and stops the
    /// exec rail from continuing through the result body).
    is_last: bool,
}

/// Per-tool interpretation of a native tool's structured result: the body
/// lines to render, whether to number them, the first line number, whether
/// the body is a color-coded diff, and an optional always-shown notice.
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
        // Bash failures are structured results; display only their output.
        // Keep the complete JSON untouched in the native record/transcript.
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
        "grep" => {
            let mut lines = Vec::new();
            if let Some(arr) = v.get("matches").and_then(|x| x.as_array()) {
                for m in arr {
                    let file = m.get("file").and_then(|x| x.as_str()).unwrap_or("");
                    let line = m
                        .get("line")
                        .and_then(serde_json::Value::as_u64)