//! Modal and popover overlays: the resume picker, tree picker, info modal,
//! and slash-complete popover. The scrollbar itself lives in [`prim`].

#[allow(clippy::wildcard_imports)]
use super::*;
use crate::tui::Theme;

use ratatui::widgets::{Block as WidgetBlock, BorderType, Padding};

/// Shared modal chrome: a filled surface, one-cell horizontal breathing room,
/// and a primary border so the active overlay is immediately obvious.
fn rich_modal_block(t: Theme) -> WidgetBlock<'static> {
    WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.primary))
        .padding(Padding::horizontal(1))
        .style(Style::new().fg(t.fg).bg(t.surface))
}

fn modal_title(t: Theme, title: impl Into<String>) -> Line<'static> {
    let title = title.into();
    Line::from(Span::styled(
        title.trim().to_string(),
        Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
    ))
}

fn modal_help(t: Theme, text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(text.into(), Style::new().fg(t.subtle)))
}

fn centered_modal(area: Rect, width: u16, desired_h: u16) -> Rect {
    let width = width.min(area.width);
    let height = desired_h.min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect::new(x, y, width, height)
}

#[derive(Clone, Copy)]
struct ModalRows {
    title: Rect,
    content: Rect,
    help: Rect,
}

/// Keep title and help attached to the inside edges when a terminal is too
/// short, allowing the content viewport to collapse before either chrome row.
fn modal_rows(inner: Rect) -> ModalRows {
    let title_h = inner.height.min(1);
    let help_h = inner.height.saturating_sub(title_h).min(1);
    let content_y = inner.y.saturating_add(title_h);
    let content_h = inner.height.saturating_sub(title_h + help_h);
    ModalRows {
        title: Rect::new(inner.x, inner.y, inner.width, title_h),
        content: Rect::new(inner.x, content_y, inner.width, content_h),
        help: Rect::new(
            inner.x,
            inner.bottom().saturating_sub(help_h),
            inner.width,
            help_h,
        ),
    }
}

fn render_modal_help_line(f: &mut Frame, area: Rect, help: Line<'static>) {
    if area.height > 0 {
        f.render_widget(
            Paragraph::new(help).alignment(ratatui::layout::Alignment::Center),
            area,
        );
    }
}

fn render_modal_help(f: &mut Frame, area: Rect, t: Theme, text: &str) {
    render_modal_help_line(f, area, modal_help(t, text));
}

fn render_modal_frame(
    f: &mut Frame,
    popup: Rect,
    t: Theme,
    title: Line<'static>,
    help: Line<'static>,
) -> ModalRows {
    let block = rich_modal_block(t);
    let rows = modal_rows(block.inner(popup));
    f.render_widget(block, popup);
    if rows.title.height > 0 {
        f.render_widget(Paragraph::new(title), rows.title);
    }
    render_modal_help_line(f, rows.help, help);
    rows
}

fn focus_style(t: Theme) -> Style {
    Style::new()
        .fg(t.panel_bg)
        .bg(t.primary)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn render_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::ListState;
    let Some(picker) = &app.picker else {
        return;
    };
    let t = app.theme;
    let total = picker.entries.len();
    let title = " Resume a session ";
    let help = " ↑/↓ navigate  enter resume  esc close ";

    // Two lines per entry: header line + preview line.
    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .map(|e| {
            let id = e.id();
            let header = format!(
                "{}  ({} msgs, {})",
                id,
                e.message_count,
                e.meta.model.label()
            );
            let preview = if e.last_message.is_empty() {
                String::new()
            } else {
                format!("  {}", e.last_message)
            };
            ListItem::new(vec![
                Line::from(Span::styled(header, Style::new().fg(t.fg))),
                Line::from(Span::styled(preview, Style::new().fg(t.muted))),
            ])
        })
        .collect();
    let content_w = picker
        .entries
        .iter()
        .map(|e| {
            let header_w = prim::width(&format!(
                "{}  ({} msgs, {})",
                e.id(),
                e.message_count,
                e.meta.model.label()
            ));
            let preview_w = if e.last_message.is_empty() {
                0
            } else {
                prim::width(&format!("  {}", e.last_message))
            };
            header_w.max(preview_w)
        })
        .max()
        .unwrap_or(0);
    let chrome_w = prim::width(title).max(prim::width(help));
    let w = u16::try_from(content_w.max(chrome_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    let desired_frame_h = u16::try_from(total.min(8) * 2 + 2).unwrap_or(14);
    let (popup, help_area) = centered_modal_with_help(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t, modal_title(t, title));
    let inner = block.inner(popup);
    let need_sb = total * 2 > inner.height as usize;
    let content = if need_sb {
        Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    let list = List::new(items)
        .style(Style::default().fg(t.fg).bg(t.surface))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    render_modal_help(f, help_area, t, help);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(
            f,
            track,
            state.offset(),
            inner.height as usize,
            total,
            t.subtle,
            t.muted,
        );
    }
}

/// Slash-command autocomplete popover: a popup listing commands that
/// start with the current input, anchored just above the prompt cursor.
/// `↑/↓` or `j`/`k` move; `Tab` accepts; `Esc` dismisses.
pub(super) fn render_slash_complete(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::ListState;
    let Some(sc) = &app.slash_complete else {
        return;
    };
    let t = app.theme;
    let n_max = sc.candidates.len().min(8);
    // Width: longest rich row or help footer, plus border and padding.
    let row_w = SLASH_COMMANDS
        .iter()
        .map(|(cmd, desc)| prim::width(cmd) + 2 + prim::width(desc))
        .max()
        .unwrap_or(20);
    let help_w = prim::width(" ↑/↓ navigate  enter complete  esc close ");
    let w = u16::try_from(row_w.max(help_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    // Anchor horizontally at the cursor's column within the prompt area,
    // so the popover tracks the cursor as the user types. `input_rect` is
    // already inset past the gutter, and the cursor x is relative to its
    // left edge, so add directly. Clamp so the popover stays on screen.
    let content_width = app.input_rect.width as usize;
    let (cursor_row, cursor_x) = app.input_cursor_pos(content_width);
    let cursor_screen_x = app
        .input_rect
        .x
        .saturating_add(u16::try_from(cursor_x).unwrap_or(u16::MAX));
    let cursor_screen_y = app
        .input_rect
        .y
        .saturating_add(u16::try_from(cursor_row).unwrap_or(u16::MAX));
    // The frame and its separate help row sit above the cursor as one
    // anchored group. Reserve the outside help row before deciding how many
    // candidates fit on short terminals.
    let avail_above = cursor_screen_y.saturating_sub(area.y) as usize;
    let n = n_max.min(avail_above.saturating_sub(3));
    if n == 0 {
        return;
    }
    let frame_h = u16::try_from(n + 2).unwrap_or(10);
    let group_h = frame_h.saturating_add(1);
    let popup_y = cursor_screen_y.saturating_sub(group_h);
    let popup_x = cursor_screen_x.min(area.right().saturating_sub(w));
    let popup = Rect::new(popup_x, popup_y, w, frame_h);
    let help_area = Rect::new(popup_x, popup.bottom(), w, 1);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t, modal_title(t, " Commands "));
    let inner = block.inner(popup);
    let total = sc.candidates.len();
    let need_sb = total > inner.height as usize;
    let content = if need_sb {
        Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    let cmd_style = Style::new().fg(t.fg);
    let desc_style = Style::new().fg(t.subtle);
    let items: Vec<ListItem> = sc
        .candidates
        .iter()
        .map(|&idx| {
            let (cmd, desc) = SLASH_COMMANDS[idx];
            ListItem::new(Line::from(vec![
                Span::styled(cmd.to_string(), cmd_style),
                Span::styled(format!("  {desc}"), desc_style),
            ]))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg).bg(t.surface))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(sc.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    render_modal_help(f, help_area, t, " ↑/↓ navigate  enter complete  esc close ");
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(
            f,
            track,
            state.offset(),
            inner.height as usize,
            total,
            t.subtle,
            t.muted,
        );
    }
}

/// Pre-wrap each styled line to `width` cells (span-aware, whitespace-
/// preserving) and flatten into the visible row list.
fn wrap_info_lines_styled(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in lines {
        out.extend(prim::wrap_line_styled(l, width));
    }
    out
}

/// Read-only, scrollable information modal (`/help`, `/session`): a
/// centered box showing the title over the body, with a trailing hint
/// line. Body lines are pre-wrapped to the available width so long lines
/// never clip horizontally; when the body exceeds the viewport, a
/// scrollbar appears and `j`/`k`/`↑`/`↓`/`Ctrl+N`/`Ctrl+P`/`PgUp`/`PgDn`
/// scroll it. `y` copies the body; `Esc`/`q`/`Enter` dismiss.
pub(super) fn render_info_modal(f: &mut Frame, area: Rect, app: &mut App) {
    let t = app.theme;
    let Some(info) = app.info.as_mut() else {
        return;
    };
    let title = format!(" {} ", info.title);
    // Body lines are styled spans; ratatui wraps them span-aware into the
    // body area. We only need the wrapped row *count* up front (for the
    // scrollbar and scroll clamping), computed from each line's plain text
    // with the same greedy word-wrap prim uses.
    // Body lines are styled spans; we pre-wrap them span-aware (preserving
    // whitespace and styles) so the row count for the scrollbar and the
    // rendered output agree exactly.
    let max_w = area.width.saturating_sub(4) as usize;
    let line_w = |l: &Line| {
        l.spans
            .iter()
            .map(|s| prim::width(s.content.as_ref()))
            .sum::<usize>()
    };
    let max_body = info.lines.iter().map(line_w).max().unwrap_or(0);
    let inner_w = max_body.min(max_w).max(prim::width(&title));
    let max_body_h = (area.height as usize).saturating_sub(3);
    let need_sb = wrap_info_lines_styled(&info.lines, inner_w.max(1)).len() > max_body_h;
    let body_w = if need_sb {
        inner_w.saturating_sub(1).max(1)
    } else {
        inner_w
    };
    let wrapped = wrap_info_lines_styled(&info.lines, body_w);
    let total = wrapped.len();
    let view_h = total.min(max_body_h);
    let desired_frame_h = u16::try_from(view_h + 2).unwrap_or(10);
    // Publish scroll geometry for the key handler; clamp any stale offset.
    info.total = total;
    info.view_h = view_h;
    if info.scroll > info.max_scroll() {
        info.scroll = info.max_scroll();
    }
    let scroll = info.scroll;

    let w = u16::try_from(inner_w + 4).unwrap_or(40).min(area.width);
    let (popup, help_area) = centered_modal_with_help(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t, modal_title(t, title));
    let inner = block.inner(popup);
    let body_w = u16::try_from(body_w).unwrap_or(inner.width);
    let body_rect = Rect {
        width: body_w,
        ..inner
    };
    f.render_widget(block, popup);

    let body = Paragraph::new(wrapped).scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
    f.render_widget(body, body_rect);

    let key_style = Style::new().fg(t.fg).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(t.muted);
    let mut hint_spans: Vec<Span<'static>> = Vec::new();
    if need_sb {
        hint_spans.push(Span::styled("j/k", key_style));
        hint_spans.push(Span::styled(" to scroll · ", dim));
    }
    hint_spans.push(Span::styled("y", key_style));
    hint_spans.push(Span::styled(" to copy · ", dim));
    hint_spans.push(Span::styled("q", key_style));
    hint_spans.push(Span::styled(" to dismiss", dim));
    render_modal_help_line(f, help_area, Line::from(hint_spans));

    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(f, track, scroll, view_h, total, t.subtle, t.muted);
    }
}

/// '/tree' branch-picker overlay: a popup showing the session's event tree
/// rendered with ASCII tree art (`|-`, `` `-``, `|  `). `user:` nodes roll
/// back to before the prompt (edit and resend); `agent:` nodes roll back to
/// after the turn (continue from here). Nodes on the active path are
/// highlighted so the current branch is visible at a glance.
/// `/model` picker: a centered list of available `provider/model` entries.
/// The current model is highlighted; `↑/↓` or `j`/`k` move, `Enter` switches,
/// `Esc`/`q` cancels.
pub(super) fn render_model_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::ListState;
    let Some(picker) = &app.model_picker else {
        return;
    };
    let t = app.theme;
    let total = picker.choices.len();
    let title = " Switch model ";
    let help = " ↑/↓ navigate  enter switch  esc close ";
    let active = app.model_label.clone();
    let row_for = |c: &lofi_types::ModelChoice| {
        let mut s = format!("{}/{}", c.provider, c.id);
        if !c.name.is_empty() && c.name != c.id {
            s.push_str("  ");
            s.push_str(&c.name);
        }
        if !c.thinking_levels.is_empty() {
            s.push_str("  ·thinks");
        }
        if c.supports_image {
            s.push_str("  ·img");
        }
        s
    };
    let content_w = picker
        .choices
        .iter()
        .map(|c| prim::width(&row_for(c)))
        .max()
        .unwrap_or(0);
    let chrome_w = prim::width(title).max(prim::width(help));
    let w = u16::try_from(content_w.max(chrome_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    let rows = total.min(20);
    let desired_frame_h = u16::try_from(rows + 2).unwrap_or(22);
    let (popup, help_area) = centered_modal_with_help(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t, modal_title(t, title));
    let inner = block.inner(popup);
    let need_sb = total > inner.height as usize;
    let content = if need_sb {
        Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    let active_style = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
    let inactive_style = Style::new().fg(t.fg);
    let items: Vec<ListItem> = picker
        .choices
        .iter()
        .map(|c| {
            let is_active = format!("{}/{}", c.provider, c.id) == active;
            ListItem::new(Span::styled(
                row_for(c),
                if is_active {
                    active_style
                } else {
                    inactive_style
                },
            ))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg).bg(t.surface))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    render_modal_help(f, help_area, t, help);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(
            f,
            track,
            state.offset(),
            inner.height as usize,
            total,
            t.subtle,
            t.muted,
        );
    }
}

/// `/thinking` picker: a centered list of thinking levels offered for the
/// current model (`off` plus its declared levels). The current level is
/// highlighted; `↑/↓` or `j`/`k` move, `Enter` switches, `Esc`/`q` cancels.
pub(super) fn render_thinking_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::ListState;
    let Some(picker) = &app.thinking_picker else {
        return;
    };
    let t = app.theme;
    let total = picker.levels.len();
    let title = " Thinking level ";
    let help = " ↑/↓ navigate  enter apply  esc close ";
    let row_for = |l: &lofi_types::ThinkingLevel| l.as_str().to_string();
    let content_w = picker
        .levels
        .iter()
        .map(|l| prim::width(&row_for(l)))
        .max()
        .unwrap_or(0);
    let chrome_w = prim::width(title).max(prim::width(help));
    let w = u16::try_from(content_w.max(chrome_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    let rows = total.min(20);
    let desired_frame_h = u16::try_from(rows + 2).unwrap_or(22);
    let (popup, help_area) = centered_modal_with_help(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t, modal_title(t, title));
    let inner = block.inner(popup);
    let need_sb = total > inner.height as usize;
    let content = if need_sb {
        Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    let active_style = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
    let inactive_style = Style::new().fg(t.fg);
    let items: Vec<ListItem> = picker
        .levels
        .iter()
        .map(|l| {
            let is_active = *l == app.thinking;
            ListItem::new(Span::styled(
                row_for(l),
                if is_active {
                    active_style
                } else {
                    inactive_style
                },
            ))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg).bg(t.surface))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    render_modal_help(f, help_area, t, help);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(
            f,
            track,
            state.offset(),
            inner.height as usize,
            total,
            t.subtle,
            t.muted,
        );
    }
}

pub(super) fn render_tree_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::ListState;
    let Some(picker) = &app.tree_picker else {
        return;
    };
    let t = app.theme;
    let total = picker.entries.len();
    let title = " Roll back to a turn ";
    let help = " ↑/↓ navigate  enter restore  esc close ";
    let content_w = picker
        .entries
        .iter()
        .map(|e| prim::width(&e.prefix) + prim::width(&e.label))
        .max()
        .unwrap_or(0);
    let chrome_w = prim::width(title).max(prim::width(help));
    let w = u16::try_from(content_w.max(chrome_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    let rows = total.min(20);
    let desired_frame_h = u16::try_from(rows + 2).unwrap_or(22);
    let (popup, help_area) = centered_modal_with_help(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t, modal_title(t, title));
    let inner = block.inner(popup);
    let need_sb = total > inner.height as usize;
    let content = if need_sb {
        Rect {
            width: inner.width.saturating_sub(1),
            ..inner
        }
    } else {
        inner
    };
    let tree_art = Style::new().fg(t.subtle);
    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .map(|e| {
            // Color by turn kind so user/agent/compact nodes read at a
            // glance; the active path (the displayed conversation) is bolded.
            let kind_color = if e.label.starts_with("user:") {
                t.secondary
            } else if e.label.starts_with("agent:") {
                t.info
            } else if e.label.starts_with("tool:") || e.label.starts_with("exec:") {
                t.success
            } else {
                t.muted // compact:
            };
            let mut label_style = Style::new().fg(kind_color);
            if e.is_active {
                label_style = label_style.add_modifier(Modifier::BOLD);
            }
            ListItem::new(Line::from(vec![
                Span::styled(e.prefix.clone(), tree_art),
                Span::styled(e.label.clone(), label_style),
            ]))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg).bg(t.surface))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    render_modal_help(f, help_area, t, help);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(
            f,
            track,
            state.offset(),
            inner.height as usize,
            total,
            t.subtle,
            t.muted,
        );
    }
}
/// Shell-policy permission dialog, inspired by Crush's explicit action
/// chooser: the requested command sits in a distinct content panel and Allow
/// / Deny are real selectable buttons. Only Enter or an action key resolves
/// the request; unrelated keys leave it open.
pub(super) fn render_confirm_modal(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::layout::Alignment;
    let t = app.theme;
    let Some(req) = app.pending_confirms.first() else {
        return;
    };

    // Dim the application beneath the dialog. This changes only the rendered
    // frame; the transcript and its semantic styles remain untouched.
    f.buffer_mut()
        .set_style(area, Style::new().fg(t.subtle).bg(t.panel_bg));

    let desired_w = area.width.saturating_mul(3).saturating_div(5).max(52);
    let w = desired_w.min(100).min(area.width);
    let content_w = w.saturating_sub(6).max(1) as usize;
    let mut cmd_lines = prim::wrap(&req.command, content_w);
    // Header, intro, label, command panel, and buttons use seven fixed rows
    // around the command body. The separate help row is reserved outside the
    // frame on short terminals.
    let max_command_rows = area.height.saturating_sub(9).max(1) as usize;
    if cmd_lines.len() > max_command_rows {
        cmd_lines.truncate(max_command_rows);
        if let Some(last) = cmd_lines.last_mut() {
            *last = prim::truncate(last, content_w.saturating_sub(1));
            last.push('…');
        }
    }
    let desired_frame_h = u16::try_from(cmd_lines.len() + 8).unwrap_or(u16::MAX);
    let (popup, help_area) = centered_modal_with_help(area, w, desired_frame_h);
    f.render_widget(Clear, popup);

    let key = Style::new().fg(t.fg).add_modifier(Modifier::BOLD);
    let help = Line::from(vec![
        Span::styled("←/→", key),
        Span::styled(" choose  ", Style::new().fg(t.subtle)),
        Span::styled("enter", key),
        Span::styled(" confirm  ", Style::new().fg(t.subtle)),
        Span::styled("esc", key),
        Span::styled(" deny", Style::new().fg(t.subtle)),
    ]);

    let queue_count = app.pending_confirms.len();
    let counter = if queue_count > 1 {
        format!("  1/{queue_count}")
    } else {
        String::new()
    };
    let title = format!(" Permission Required{counter} ");
    let block = rich_modal_block(
        t,
        Line::from(Span::styled(
            title,
            Style::new().fg(t.warn).add_modifier(Modifier::BOLD),
        )),
    );
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    if inner.height == 0 {
        render_modal_help_line(f, help_area, help);
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(1), // explanation
        Constraint::Length(1), // gap
        Constraint::Length(1), // content label
        Constraint::Min(1),    // command panel
        Constraint::Length(1), // gap
        Constraint::Length(1), // actions
        Constraint::Length(1), // gap
    ])
    .split(inner);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Shell", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)),
            Span::styled(" wants to run this command", Style::new().fg(t.muted)),
        ])),
        rows[0],
    );
    f.render_widget(
        Paragraph::new(Span::styled(
            "Command",
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        )),
        rows[2],
    );

    let command_panel = WidgetBlock::default()
        .style(Style::new().fg(t.fg).bg(t.panel_bg))
        .padding(Padding::horizontal(1));
    let command_inner = command_panel.inner(rows[3]);
    f.render_widget(command_panel, rows[3]);
    let body_lines: Vec<Line> = cmd_lines
        .into_iter()
        .map(|line| Line::from(Span::styled(line, Style::new().fg(t.fg).bg(t.panel_bg))))
        .collect();
    f.render_widget(Paragraph::new(body_lines), command_inner);

    let button = |label: &'static str, selected: bool| {
        let style = if selected {
            focus_style(t)
        } else {
            Style::new().fg(t.muted).bg(t.panel_bg)
        };
        Span::styled(format!(" {label} "), style)
    };
    let actions = Line::from(vec![
        button("Allow", app.confirm_selected == 0),
        Span::raw("  "),
        button("Deny", app.confirm_selected == 1),
    ])
    .alignment(Alignment::Right);
    f.render_widget(Paragraph::new(actions), rows[5]);

    render_modal_help_line(f, help_area, help);
}
