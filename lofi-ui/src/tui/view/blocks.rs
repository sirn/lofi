#![allow(clippy::needless_lifetimes)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};

use crate::tui::theme::{active_indicator, agent_indicator, user_indicator, Theme};
use crate::tui::{App, Block, NativePreview, NativeTool, ThinkingBlock, ToolCall, Turn};

use super::component::{Component, Cx, Stack};
use super::prim::{self, RawLine, RenderLine};

const PREVIEW_LINES: usize = 3;

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
            Block::UserBash {
                command,
                output,
                exit_code,
                signal,
                duration,
                truncated,
                cancelled,
                exclude_from_context,
            } => stack.push(UserBashLine {
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

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
    Center,
}

fn is_table_separator(line: &str) -> bool {
    line.contains('-') && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

fn parse_table_row(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect()
}

fn parse_align(cell: &str) -> Align {
    let left = cell.starts_with(':');
    let right = cell.ends_with(':');
    match (left, right) {
        (true, true) => Align::Center,
        (false, true) => Align::Right,
        _ => Align::Left,
    }
}

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

fn rendered_width(cell: &str, t: Theme, base: Style) -> usize {
    inline_spans(cell, t, base)
        .iter()
        .map(|s| s.content.chars().count())
        .sum()
}

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
        let w = cx.width;
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
            let is_last = idx + 1 == n_total && !self.tool.done;
            out.append_component(&ExecBlockBranch { nt, is_last }, cx);
        }

        if self.tool.done {
            out.extend(exec_result_lines(self.tool, t, w, cx.app.verbose));
        }

        out
    }
}

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

struct ExecBlockBranch<'a> {
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
        _ => NativeBody {
            lines: split_lines(raw),
            numbered: false,
            start_line: 1,
            is_diff: false,
            notice: None,
        },
    }
}

pub(crate) fn compact_native_previews(turn: &mut Turn) {
    for block in &mut turn.blocks {
        let Block::Tool(tool) = block else { continue };
        for native in &mut tool.native {
            if native.is_error || native.result.as_deref().is_none_or(str::is_empty) {
                continue;
            }
            if !matches!(native.name.as_str(), "bash" | "write" | "edit" | "agent") {
                native.result = None;
                continue;
            }
            let raw = native.result.as_deref().unwrap_or_default();
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
            let range = native_preview_range(&native.name, total_lines, false);
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
            native.result = None;
        }
    }
}

fn native_preview_range(name: &str, total: usize, verbose: bool) -> std::ops::Range<usize> {
    if verbose {
        return 0..total;
    }
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
        let w = cx.width;
        let working = !self.nt.done && cx.active_turn;
        let mut out = RenderWindow::new(range);
        let exec_cont = if self.is_last { "  " } else { "│ " };

        let mut content = vec![
            Span::styled("Tool ", Style::new().fg(t.muted)),
            Span::styled(self.nt.name.clone(), Style::new().fg(t.info)),
        ];
        if !self.nt.args.is_empty() {
            content.push(Span::styled(
                format!(" {}", self.nt.args),
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
        let name_w = 5 + self.nt.name.chars().count(); // "Tool " + name
        let cont_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::raw(" ".repeat(name_w)),
        ];
        out.extend(prim::rline_wrapped(header_deco, &cont_deco, content, w));

        if let Some(preview) = &self.nt.preview {
            let indent = 2 + 2 + 2;
            let lw = preview.total_lines.to_string().len().max(3);
            let avail = w
                .saturating_sub(indent)
                .saturating_sub(if preview.numbered { lw + 1 } else { 0 });
            let body_fg = if self.nt.is_error { t.error } else { t.muted };
            let blank_n = " ".repeat(lw + 1);
            let base_deco = vec![
                Span::raw("  "),
                Span::styled(exec_cont, Style::new().fg(t.subtle)),
                Span::styled("│ ", Style::new().fg(t.subtle)),
            ];
            for (i, line) in preview.lines.iter().enumerate() {
                let logical = preview.preview_start + i;
                let n = format!("{:>lw$} ", preview.start_line + logical, lw = lw);
                let style = if preview.is_diff {
                    match line.chars().next() {
                        Some('-') => Style::new().fg(t.error),
                        Some('+') => Style::new().fg(t.success),
                        _ => Style::new().fg(t.muted),
                    }
                } else if preview.numbered {
                    Style::new().fg(t.fg)
                } else {
                    Style::new().fg(body_fg)
                };
                for (row, seg) in prim::wrap_pre(line, avail).into_iter().enumerate() {
                    let mut deco = base_deco.clone();
                    if preview.numbered {
                        deco.push(if row == 0 {
                            Span::styled(n.clone(), Style::new().fg(t.subtle))
                        } else {
                            Span::styled(blank_n.clone(), Style::new().fg(t.subtle))
                        });
                    }
                    out.push(prim::rline(deco, vec![Span::styled(seg, style)]));
                }
            }
            let hidden = preview.total_lines.saturating_sub(preview.lines.len());
            if hidden > 0 {
                out.push(prim::rline(
                    vec![
                        Span::raw("  "),
                        Span::styled(exec_cont, Style::new().fg(t.subtle)),
                        Span::styled("… ", Style::new().fg(t.subtle)),
                    ],
                    vec![Span::styled(
                        format!("({hidden} lines hidden)"),
                        Style::new().fg(t.subtle),
                    )],
                ));
            }
            if let Some(notice) = &preview.notice {
                out.push(prim::rline(
                    vec![
                        Span::raw("  "),
                        Span::styled(exec_cont, Style::new().fg(t.subtle)),
                        Span::styled("… ", Style::new().fg(t.subtle)),
                    ],
                    vec![Span::styled(notice.clone(), Style::new().fg(t.subtle))],
                ));
            }
            return out;
        }

        let Some(result) = &self.nt.result else {
            return out;
        };
        if result.is_empty() {
            return out;
        }
        // In non-verbose mode, hide read-only results for a cleaner
        // transcript — file reads/greps/finds/ls clutter the view. Mutating
        // tools (bash, write, edit) keep their result so the user sees the
        // outcome of an action; errors stay visible regardless so a failure
        // is never silently swallowed.
        if !cx.app.verbose
            && !matches!(self.nt.name.as_str(), "bash" | "write" | "edit" | "agent")
            && !self.nt.is_error
        {
            return out;
        }

        let indent = 2 + 2 + 2; // left gutter + exec-rail column + own rail
        let raw = result.as_str();
        let encoded_field = match self.nt.name.as_str() {
            "bash" => Some("output"),
            "write" | "read" | "view" | "bash_read" => Some("content"),
            _ => None,
        };
        let encoded = encoded_field.and_then(|field| json_string_field(raw, field));
        let body = encoded.is_none().then(|| native_body(self.nt));
        let numbered = body.as_ref().is_some_and(|body| body.numbered)
            || matches!(self.nt.name.as_str(), "read" | "view" | "bash_read");
        let start = body.as_ref().map_or_else(
            || {
                if numbered {
                    json_u64_field(raw, "start_line").unwrap_or(1) as usize
                } else {
                    1
                }
            },
            |body| body.start_line,
        );
        let total = encoded.map_or_else(
            || body.as_ref().map_or(0, |body| body.lines.len()),
            encoded_json_line_count,
        );
        let lw = total.to_string().len().max(3);
        let avail = w
            .saturating_sub(indent)
            .saturating_sub(if numbered { lw + 1 } else { 0 });
        let preview = native_preview_range(&self.nt.name, total, cx.app.verbose);
        let hidden = total.saturating_sub(preview.len());
        let body_fg = if self.nt.is_error { t.error } else { t.muted };
        let blank_n = " ".repeat(lw + 1);
        let num_style = Style::new().fg(t.subtle);
        let numbered_style = Style::new().fg(t.fg);
        let plain_style = Style::new().fg(body_fg);
        let diff_del = Style::new().fg(t.error);
        let diff_add = Style::new().fg(t.success);
        let diff_ctx = Style::new().fg(t.muted);
        let base_deco = vec![
            Span::raw("  "),
            Span::styled(exec_cont, Style::new().fg(t.subtle)),
            Span::styled("│ ", Style::new().fg(t.subtle)),
        ];
        let emit_line = |logical: usize, line: &str, out: &mut RenderWindow| {
            let n = format!("{:>lw$} ", start + logical, lw = lw);
            let content_style = if body.as_ref().is_some_and(|body| body.is_diff) {
                match line.chars().next() {
                    Some('-') => diff_del,
                    Some('+') => diff_add,
                    _ => diff_ctx,
                }
            } else if numbered {
                numbered_style
            } else {
                plain_style
            };
            let local_start = out.range.start.saturating_sub(out.total);
            let local_end = out.range.end.saturating_sub(out.total);
            let (rows, segments) = prim::wrap_pre_window(line, avail, local_start..local_end);
            let base = out.total;
            for (j, seg) in segments.into_iter().enumerate() {
                let row = local_start + j;
                let mut deco = base_deco.clone();
                if numbered {
                    deco.push(if row == 0 {
                        Span::styled(n.clone(), num_style)
                    } else {
                        Span::styled(blank_n.clone(), num_style)
                    });
                }
                out.lines
                    .push(prim::rline(deco, vec![Span::styled(seg, content_style)]));
            }
            out.total = base.saturating_add(rows);
        };
        if let Some(encoded) = encoded {
            for_each_encoded_json_line(encoded, preview.clone(), |i, line| {
                emit_line(i, &line, &mut out);
            });
        } else if let Some(body) = body.as_ref() {
            for (i, line) in body.lines[preview.clone()].iter().enumerate() {
                emit_line(preview.start + i, line, &mut out);
            }
        }
        if hidden > 0 {
            let cap = format!("({hidden} lines hidden)");
            out.push(prim::rline(
                vec![
                    Span::raw("  "),
                    Span::styled(exec_cont, Style::new().fg(t.subtle)),
                    Span::styled("… ", Style::new().fg(t.subtle)),
                ],
                vec![Span::styled(cap, Style::new().fg(t.subtle))],
            ));
        }
        if let Some(notice) = body.and_then(|body| body.notice) {
            out.push(prim::rline(
                vec![
                    Span::raw("  "),
                    Span::styled(exec_cont, Style::new().fg(t.subtle)),
                    Span::styled("… ", Style::new().fg(t.subtle)),
                ],
                vec![Span::styled(notice, Style::new().fg(t.subtle))],
            ));
        }
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

struct UserBashLine<'a> {
    command: &'a str,
    output: &'a str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    duration: Duration,
    truncated: bool,
    cancelled: bool,
    exclude_from_context: bool,
}

impl Component for UserBashLine<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let failed =
            self.cancelled || self.signal.is_some() || self.exit_code.is_some_and(|c| c != 0);
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

        let lines: Vec<&str> = self.output.trim_end_matches('\n').split('\n').collect();
        let limit = if cx.app.verbose {
            lines.len()
        } else {
            PREVIEW_LINES
        };
        let rail = vec![
            Span::raw("  "),
            Span::styled("│ ", Style::new().fg(t.subtle)),
        ];
        if !self.output.is_empty() {
            let body_style = Style::new().fg(if failed { t.error } else { t.muted });
            for raw in lines.iter().take(limit) {
                for seg in prim::wrap_pre(raw, cx.width.saturating_sub(4)) {
                    out.push(prim::rline(
                        rail.clone(),
                        vec![Span::styled(seg, body_style)],
                    ));
                }
            }
            let hidden = lines.len().saturating_sub(limit);
            if hidden > 0 {
                out.push(prim::rline(
                    rail,
                    vec![Span::styled(
                        format!("… ({hidden} lines hidden)"),
                        Style::new().fg(t.subtle),
                    )],
                ));
            }
        }

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
        out.push(prim::rline(
            vec![
                Span::raw("  "),
                Span::styled("└ ", Style::new().fg(t.subtle)),
                Span::styled(
                    if failed { "✗ " } else { "✓ " },
                    Style::new().fg(status_color),
                ),
            ],
            vec![Span::styled(status, Style::new().fg(t.fg))],
        ));
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

/// Turn-cancelled separator. Mirrors Pi's "Operation aborted" status while
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

/// Compaction marker: `◇ Compacted N messages · kept M` in the muted tint,
/// appended to a turn when `/compact` (or the auto-trigger) folds the
/// older history into a summary. Under `/verbose` the folded summary text
/// is expanded below the marker (soft-wrapped, muted) so the fold can be
/// inspected without leaving the transcript.
struct CompactionLine {
    summarized: usize,
    kept: usize,
    summary: String,
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
        let mut out = vec![marker];
        if cx.app.verbose {
            let text = self.summary.trim();
            if !text.is_empty() {
                let indent = "    ";
                let content_w = cx.width.saturating_sub(indent.len());
                for raw in text.split('\n') {
                    let line = raw.trim_end();
                    if line.is_empty() {
                        out.push(prim::rblank());
                    } else {
                        for seg in prim::wrap(line, content_w) {
                            out.push(prim::render(
                                vec![Span::raw(indent)],
                                vec![Span::styled(seg, Style::new().fg(t.muted))],
                                vec![],
                            ));
                        }
                    }
                }
            }
        }
        out
    }
}

// `active_indicator` is re-exported for the working indicator in the chrome;
// keep the import here so the component module can surface it if needed.
#[allow(unused_imports)]
use active_indicator as _;

#[cfg(test)]
mod tests {
    use super::{native_body, native_preview_range, trim_reasoning_summary};
    use crate::tui::NativeTool;

    #[test]
    fn bash_preview_keeps_tail_while_other_tools_keep_head() {
        assert_eq!(native_preview_range("bash", 10, false), 7..10);
        assert_eq!(native_preview_range("read", 10, false), 0..3);
        assert_eq!(native_preview_range("bash", 2, false), 0..2);
        assert_eq!(native_preview_range("bash", 10, true), 0..10);
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
