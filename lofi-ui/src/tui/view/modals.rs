//! Modal and popover overlays: the resume picker, tree picker, info modal,
//! and slash-complete popover. The scrollbar itself lives in [`prim`].

#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn render_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{Block as WidgetBlock, BorderType, ListState};
    let Some(picker) = &app.picker else {
        return;
    };
    let t = app.theme;
    let total = picker.entries.len();
    let title = " Resume a session ";
    let items: Vec<ListItem> = picker
        .entries
        .iter()
        .map(|e| {
            let id = e.id();
            ListItem::new(format!("{}  ({} msgs, {})", id, e.message_count, e.meta.model.label()))
        })
        .collect();
    let content_w = picker
        .entries
        .iter()
        .map(|e| {
            prim::width(&format!(
                "{}  ({} msgs, {})",
                e.id(),
                e.message_count,
                e.meta.model.label()
            ))
        })
        .max()
        .unwrap_or(0);
    let w = u16::try_from(content_w.max(prim::width(title)) + 2)
        .unwrap_or(40)
        .min(area.width);
    let h = u16::try_from(total.min(12) + 2)
        .unwrap_or(14)
        .min(area.height);
    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(h), Constraint::Min(0)])
        .split(area);
    let horiz =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w), Constraint::Min(0)])
            .split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        ));
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
    let list = List::new(items)
        .style(Style::default().fg(t.fg))
        .highlight_style(Style::default().bg(t.selection).fg(t.fg));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(f, track, state.offset(), inner.height as usize, total, t.subtle, t.muted);
    }
}

/// Slash-command autocomplete popover: a popup listing commands that
/// start with the current input, anchored just above the prompt cursor.
/// `↑/↓` or `j`/`k` move; `Tab` accepts; `Esc` dismisses.
pub(super) fn render_slash_complete(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{Block as WidgetBlock, BorderType, ListState};
    let Some(sc) = &app.slash_complete else { return; };
    let t = app.theme;
    let n_max = sc.candidates.len().min(8);
    // Width: longest "cmd  desc" plus borders, capped to the screen.
    let w = u16::try_from(
        SLASH_COMMANDS
            .iter()
            .map(|(cmd, desc)| cmd.len() + 2 + desc.len())
            .max()
            .unwrap_or(20)
            + 4,
    )
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
    // The popover sits above the cursor; cap its height to the rows
    // available there so it never overflows the screen on short terminals.
    // If not even a bordered single row fits, skip rendering entirely.
    let avail_above = cursor_screen_y.min(area.height) as usize;
    let n = n_max.min(avail_above.saturating_sub(2));
    if n == 0 {
        return;
    }
    let h = u16::try_from(n + 2).unwrap_or(10);
    // Bottom of the popover = the row just above the cursor's row.
    let bottom_y = cursor_screen_y;
    let popup_y = bottom_y.saturating_sub(h);
    let popup_x = cursor_screen_x.min(area.width.saturating_sub(w));
    let popup = Rect::new(popup_x, popup_y, w, h);
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            " Commands ",
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        ));
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
        .style(Style::default().fg(t.fg))
        .highlight_style(Style::default().bg(t.selection).fg(t.fg));
    let mut state = ListState::default().with_selected(Some(sc.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(f, track, state.offset(), inner.height as usize, total, t.subtle, t.muted);
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
    use ratatui::widgets::{Block as WidgetBlock, BorderType};
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
    let max_w = area.width.saturating_sub(2) as usize;
    let line_w = |l: &Line| l.spans.iter().map(|s| prim::width(s.content.as_ref())).sum::<usize>();
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
    let h = u16::try_from(view_h + 3).unwrap_or(10).min(area.height);
    // Publish scroll geometry for the key handler; clamp any stale offset.
    info.total = total;
    info.view_h = view_h;
    if info.scroll > info.max_scroll() {
        info.scroll = info.max_scroll();
    }
    let scroll = info.scroll;

    let w = u16::try_from(inner_w + 2)
        .unwrap_or(40)
        .min(area.width);

    let vert =
        Layout::vertical([Constraint::Min(0), Constraint::Length(h), Constraint::Min(0)])
            .split(area);
    let horiz =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w), Constraint::Min(0)])
            .split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    let body_w = u16::try_from(body_w).unwrap_or(inner.width);
    let body_rect = Rect {
        width: body_w,
        height: inner.height.saturating_sub(1),
        ..inner
    };
    let hint_rect = Rect {
        y: body_rect.bottom(),
        width: body_w,
        height: 1,
        ..inner
    };
    f.render_widget(block, popup);

    let body = Paragraph::new(wrapped)
        .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
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
    f.render_widget(Paragraph::new(Line::from(hint_spans)), hint_rect);

    if need_sb {
        let track = Rect::new(
            inner.right().saturating_sub(1),
            inner.y,
            1,
            inner.height.saturating_sub(1),
        );
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
    use ratatui::widgets::{Block as WidgetBlock, BorderType, ListState};
    let Some(picker) = &app.model_picker else { return; };
    let t = app.theme;
    let total = picker.choices.len();
    let title = " Switch model  ↑/↓ j/k enter esc ";
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
    let w = u16::try_from(content_w.max(prim::width(title)) + 2)
        .unwrap_or(40)
        .min(area.width);
    let rows = total.min(20);
    let h = u16::try_from(rows + 2).unwrap_or(22).min(area.height);
    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(h), Constraint::Min(0)])
        .split(area);
    let horiz =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w), Constraint::Min(0)])
            .split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        ));
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
                if is_active { active_style } else { inactive_style },
            ))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg))
        .highlight_style(Style::default().bg(t.selection).fg(t.fg));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(f, track, state.offset(), inner.height as usize, total, t.subtle, t.muted);
    }
}

/// `/thinking` picker: a centered list of thinking levels offered for the
/// current model (`off` plus its declared levels). The current level is
/// highlighted; `↑/↓` or `j`/`k` move, `Enter` switches, `Esc`/`q` cancels.
pub(super) fn render_thinking_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{Block as WidgetBlock, BorderType, ListState};
    let Some(picker) = &app.thinking_picker else { return; };
    let t = app.theme;
    let total = picker.levels.len();
    let title = " Thinking level  ↑/↓ j/k enter esc ";
    let row_for = |l: &lofi_types::ThinkingLevel| l.as_str().to_string();
    let content_w = picker
        .levels
        .iter()
        .map(|l| prim::width(&row_for(l)))
        .max()
        .unwrap_or(0);
    let w = u16::try_from(content_w.max(prim::width(title)) + 2)
        .unwrap_or(40)
        .min(area.width);
    let rows = total.min(20);
    let h = u16::try_from(rows + 2).unwrap_or(22).min(area.height);
    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(h), Constraint::Min(0)])
        .split(area);
    let horiz =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w), Constraint::Min(0)])
            .split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        ));
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
                if is_active { active_style } else { inactive_style },
            ))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg))
        .highlight_style(Style::default().bg(t.selection).fg(t.fg));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(f, track, state.offset(), inner.height as usize, total, t.subtle, t.muted);
    }
}

pub(super) fn render_tree_picker(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{Block as WidgetBlock, BorderType, ListState};
    let Some(picker) = &app.tree_picker else { return; };
    let t = app.theme;
    let total = picker.entries.len();
    let title = " Roll back to a turn  ↑/↓ j/k enter esc ";
    let content_w = picker
        .entries
        .iter()
        .map(|e| prim::width(&e.prefix) + prim::width(&e.label))
        .max()
        .unwrap_or(0);
    let w = u16::try_from(content_w.max(prim::width(title)) + 2)
        .unwrap_or(40)
        .min(area.width);
    let rows = total.min(20);
    let h = u16::try_from(rows + 2).unwrap_or(22).min(area.height);
    let vert = Layout::vertical([Constraint::Min(0), Constraint::Length(h), Constraint::Min(0)])
        .split(area);
    let horiz =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w), Constraint::Min(0)])
            .split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
        ));
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
        .style(Style::default().fg(t.fg))
        .highlight_style(Style::default().bg(t.selection).fg(t.fg));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_widget(block, popup);
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        let track = Rect::new(inner.right().saturating_sub(1), inner.y, 1, inner.height);
        prim::render_scrollbar(f, track, state.offset(), inner.height as usize, total, t.subtle, t.muted);
    }
}
/// Shell-policy confirmation modal: a small centered popup showing the
/// command text and a `y/n` prompt. `y` allows, any other key denies.
/// If multiple requests are queued, a counter is shown.
pub(super) fn render_confirm_modal(f: &mut Frame, area: Rect, app: &App) {
    use ratatui::widgets::{Block as WidgetBlock, BorderType};
    let t = app.theme;
    let Some(req) = app.pending_confirms.first() else {
        return;
    };
    let max_w = area.width.saturating_sub(4) as usize;
    let cmd_lines = prim::wrap(&req.command, max_w.min(80));
    let body_h = cmd_lines.len();
    let queue_count = app.pending_confirms.len();
    let counter = if queue_count > 1 {
        format!(" ({}/{})", 1, queue_count)
    } else {
        String::new()
    };
    let title = format!(" Confirm{counter} ");
    let title_w = prim::width(&title);
    let inner_w = cmd_lines
        .iter()
        .map(|l| prim::width(l.as_str()))
        .max()
        .unwrap_or(0)
        .max(title_w)
        .min(max_w);
    let w = u16::try_from(inner_w + 4).unwrap_or(50).min(area.width);
    let h = u16::try_from(body_h + 4).unwrap_or(7).min(area.height);
    let vert =
        Layout::vertical([Constraint::Min(0), Constraint::Length(h), Constraint::Min(0)])
            .split(area);
    let horiz =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(w), Constraint::Min(0)])
            .split(vert[1]);
    let popup = horiz[1];
    f.render_widget(Clear, popup);
    let block = WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            title,
            Style::new().fg(t.warn).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);
    let body_lines: Vec<Line> = cmd_lines
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::new().fg(t.fg))))
        .collect();
    f.render_widget(Paragraph::new(body_lines), inner);
    let hint = Line::from(vec![
        Span::styled("y", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)),
        Span::styled(" allow · ", Style::new().fg(t.muted)),
        Span::styled("n", Style::new().fg(t.fg).add_modifier(Modifier::BOLD)),
        Span::styled(" deny", Style::new().fg(t.muted)),
    ]);
    let hint_y = inner.bottom().saturating_sub(1);
    f.render_widget(
        Paragraph::new(hint),
        Rect { y: hint_y, ..inner },
    );
}