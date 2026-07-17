//! Modal and popover overlays: the resume picker, tree picker, info modal,
//! and slash-complete popover. The scrollbar itself lives in [`prim`].

#[allow(clippy::wildcard_imports)]
use super::*;
use crate::tui::Theme;

use ratatui::widgets::{Block as WidgetBlock, BorderType, Padding};

/// Shared modal chrome: the terminal's default background, one-cell
/// horizontal breathing room, and a primary border so the active overlay is
/// immediately obvious.
fn rich_modal_block(t: Theme) -> WidgetBlock<'static> {
    WidgetBlock::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(t.primary))
        .padding(Padding::horizontal(1))
        .style(Style::new().fg(t.fg))
}

fn modal_title(t: Theme, title: impl Into<String>) -> Line<'static> {
    let title = title.into();
    Line::from(Span::styled(
        title.trim().to_string(),
        Style::new().fg(t.primary).add_modifier(Modifier::BOLD),
    ))
}

/// Build a modal footer whose action keys stand out from their descriptions.
/// Hint groups use the conventional `"key action  key action"` shape.
fn modal_help(t: Theme, text: impl Into<String>) -> Line<'static> {
    let text = text.into();
    let leading = text.starts_with(' ');
    let trailing = text.ends_with(' ');
    let groups: Vec<&str> = text.trim().split("  ").filter(|s| !s.is_empty()).collect();
    let key_style = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
    let description_style = Style::new().fg(t.subtle);
    let mut spans = Vec::new();
    if leading {
        spans.push(Span::styled(" ", description_style));
    }
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", description_style));
        }
        let (key, description) = group.split_once(' ').unwrap_or((group, ""));
        spans.push(Span::styled(key.to_string(), key_style));
        if !description.is_empty() {
            spans.push(Span::styled(format!(" {description}"), description_style));
        }
    }
    if trailing {
        spans.push(Span::styled(" ", description_style));
    }
    Line::from(spans)
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

/// Extend a modal's content rect through its existing right padding cell,
/// then split that span into content plus the universal scrollbar gutter.
/// This preserves the modal's one-cell breathing room while ensuring list or
/// body text can never occupy the scrollbar column.
fn modal_scroll_area(content: Rect) -> prim::ScrollArea {
    prim::scroll_area(Rect::new(
        content.x,
        content.y,
        content.width.saturating_add(1),
        content.height,
    ))
}

fn focus_style(t: Theme) -> Style {
    Style::new()
        .fg(t.panel_bg)
        .bg(t.primary)
        .add_modifier(Modifier::BOLD)
}

fn relative_age(then: std::time::SystemTime) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(then)
        .unwrap_or_default()
        .as_secs();
    match seconds {
        0..60 => "just now".to_string(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..172_800 => format!("{}h ago", seconds / 3600),
        172_800..31_536_000 => format!("{}d ago", seconds / 86_400),
        _ => format!("{}y ago", seconds / 31_536_000),
    }
}

fn resume_metadata(message_count: usize, last_active: std::time::SystemTime) -> String {
    let messages = if message_count == 1 { "msg" } else { "msgs" };
    format!("{message_count} {messages} · {}", relative_age(last_active))
}

/// Color only the semantic role label; keep the colon and transcript preview
/// in the normal foreground so the row remains easy to scan.
fn resume_preview_spans(t: Theme, preview: String) -> Vec<Span<'static>> {
    let role = if preview.starts_with("user:") {
        Some(("user", t.user))
    } else if preview.starts_with("agent:") {
        Some(("agent", t.agent))
    } else {
        None
    };
    let Some((role, color)) = role else {
        return vec![Span::styled(preview, Style::new().fg(t.fg))];
    };
    let rest = preview[role.len()..].to_string();
    vec![
        Span::styled(
            role.to_string(),
            Style::new().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(rest, Style::new().fg(t.fg)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_preview_highlights_role_labels() {
        let t = Theme::dark();
        let user = resume_preview_spans(t, "user: hello".to_string());
        assert_eq!(user[0].content, "user");
        assert_eq!(user[0].style.fg, Some(t.user));
        assert_eq!(user[1].content, ": hello");
        assert_eq!(user[1].style.fg, Some(t.fg));

        let agent = resume_preview_spans(t, "agent: hi".to_string());
        assert_eq!(agent[0].content, "agent");
        assert_eq!(agent[0].style.fg, Some(t.agent));
        assert_eq!(agent[1].content, ": hi");
    }
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
    let desired_rows = total.max(1).min(12);
    let desired_frame_h = u16::try_from(desired_rows + 4).unwrap_or(16);

    // Size from only the selected viewport. Progressive enrichment of an old
    // off-screen row must not make every frame walk and allocate the full list.
    let preliminary_start = picker
        .selected
        .saturating_sub(desired_rows.saturating_sub(1))
        .min(total.saturating_sub(desired_rows));
    let row_data = |row: &crate::tui::PickerEntry| match &row.details {
        Some(entry) => (
            entry.last_message.clone(),
            resume_metadata(entry.message_count, entry.last_active),
        ),
        None => (
            row.preview
                .clone()
                .unwrap_or_else(|| "loading…".to_string()),
            relative_age(row.file.last_active()),
        ),
    };
    let content_w = picker.entries
        [preliminary_start..(preliminary_start + desired_rows.min(total))]
        .iter()
        .map(|row| {
            let (preview, meta) = row_data(row);
            prim::width(&preview) + 2 + prim::width(&meta)
        })
        .max()
        .unwrap_or(0);
    let chrome_w = prim::width(title).max(prim::width(help));
    let w = u16::try_from(content_w.max(chrome_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    let popup = centered_modal(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let rows = render_modal_frame(f, popup, t, modal_title(t, title), modal_help(t, help));
    let scroll_area = modal_scroll_area(rows.content);
    let content = scroll_area.content;
    let view_h = content.height as usize;
    let start = picker
        .selected
        .saturating_sub(view_h.saturating_sub(1))
        .min(total.saturating_sub(view_h));
    let end = (start + view_h).min(total);
    let row_width = content.width as usize;
    let items: Vec<ListItem> = picker.entries[start..end]
        .iter()
        .map(|row| {
            let (preview, meta) = row_data(row);
            let meta_w = prim::width(&meta);
            let preview_w = row_width.saturating_sub(meta_w.saturating_add(2));
            let preview = prim::truncate(&preview, preview_w);
            ListItem::new(
                HStack::new(row_width)
                    .left(resume_preview_spans(t, preview))
                    .right([Span::styled(meta, Style::new().fg(t.muted))])
                    .build(),
            )
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg))
        .highlight_style(focus_style(t));
    let selected = (picker.selected < total).then_some(picker.selected.saturating_sub(start));
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(list, content, &mut state);
    if total > view_h {
        prim::render_scrollbar(
            f,
            scroll_area.gutter,
            start,
            view_h,
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
    // Border and bottom hint consume three rows; autocomplete deliberately
    // has no title so the first interior row is immediately useful.
    let avail_above = cursor_screen_y.saturating_sub(area.y) as usize;
    let n = n_max.min(avail_above.saturating_sub(3));
    if n == 0 {
        return;
    }
    let frame_h = u16::try_from(n + 3).unwrap_or(11);
    let popup_y = cursor_screen_y.saturating_sub(frame_h);
    let popup_x = cursor_screen_x.min(area.right().saturating_sub(w));
    let popup = Rect::new(popup_x, popup_y, w, frame_h);
    f.render_widget(Clear, popup);
    let block = rich_modal_block(t);
    let inner = block.inner(popup);
    let help_h = inner.height.min(1);
    let content_rows = Rect::new(
        inner.x,
        inner.y,
        inner.width,
        inner.height.saturating_sub(help_h),
    );
    let help = Rect::new(
        inner.x,
        inner.bottom().saturating_sub(help_h),
        inner.width,
        help_h,
    );
    f.render_widget(block, popup);
    render_modal_help_line(
        f,
        help,
        modal_help(t, " ↑/↓ navigate  enter complete  esc close "),
    );
    let total = sc.candidates.len();
    let need_sb = total > content_rows.height as usize;
    let scroll_area = modal_scroll_area(content_rows);
    let content = scroll_area.content;
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
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(sc.selected));
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        prim::render_scrollbar(
            f,
            scroll_area.gutter,
            state.offset(),
            content_rows.height as usize,
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
    let max_body_h = (area.height as usize).saturating_sub(4);
    let need_sb = wrap_info_lines_styled(&info.lines, inner_w.max(1)).len() > max_body_h;
    // The modal's existing right padding cell is the dedicated scrollbar
    // gutter, leaving the full measured body width available for text.
    let body_w = inner_w;
    let wrapped = wrap_info_lines_styled(&info.lines, body_w);
    let total = wrapped.len();
    let view_h = total.min(max_body_h);
    let desired_frame_h = u16::try_from(view_h + 4).unwrap_or(12);

    let key_style = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
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

    let w = u16::try_from(inner_w + 4).unwrap_or(40).min(area.width);
    let popup = centered_modal(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let rows = render_modal_frame(f, popup, t, modal_title(t, title), Line::from(hint_spans));
    let actual_view_h = rows.content.height as usize;
    info.total = total;
    info.view_h = actual_view_h;
    if info.scroll > info.max_scroll() {
        info.scroll = info.max_scroll();
    }
    let scroll = info.scroll;
    let body_rect = Rect {
        width: u16::try_from(body_w)
            .unwrap_or(rows.content.width)
            .min(rows.content.width),
        ..rows.content
    };
    let body = Paragraph::new(wrapped).scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
    f.render_widget(body, body_rect);

    if need_sb {
        let scroll_area = modal_scroll_area(rows.content);
        prim::render_scrollbar(
            f,
            scroll_area.gutter,
            scroll,
            actual_view_h,
            total,
            t.subtle,
            t.muted,
        );
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
    let visible_rows = total.min(20);
    let desired_frame_h = u16::try_from(visible_rows + 4).unwrap_or(24);
    let popup = centered_modal(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let rows = render_modal_frame(f, popup, t, modal_title(t, title), modal_help(t, help));
    let need_sb = total > rows.content.height as usize;
    let scroll_area = modal_scroll_area(rows.content);
    let content = scroll_area.content;
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
        .style(Style::default().fg(t.fg))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        prim::render_scrollbar(
            f,
            scroll_area.gutter,
            state.offset(),
            rows.content.height as usize,
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
    let visible_rows = total.min(20);
    let desired_frame_h = u16::try_from(visible_rows + 4).unwrap_or(24);
    let popup = centered_modal(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let rows = render_modal_frame(f, popup, t, modal_title(t, title), modal_help(t, help));
    let need_sb = total > rows.content.height as usize;
    let scroll_area = modal_scroll_area(rows.content);
    let content = scroll_area.content;
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
        .style(Style::default().fg(t.fg))
        .highlight_style(focus_style(t));
    let mut state = ListState::default().with_selected(Some(picker.selected));
    f.render_stateful_widget(list, content, &mut state);
    if need_sb {
        prim::render_scrollbar(
            f,
            scroll_area.gutter,
            state.offset(),
            rows.content.height as usize,
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
    let title = if picker.loading {
        " Loading session tree… "
    } else {
        " Roll back to a turn "
    };
    let help = " ↑/↓ navigate  enter restore  esc close ";
    let desired_rows = total.max(1).min(20);
    let preliminary_start = picker
        .selected
        .saturating_sub(desired_rows.saturating_sub(1))
        .min(total.saturating_sub(desired_rows));
    let content_w = picker.entries
        [preliminary_start..(preliminary_start + desired_rows.min(total))]
        .iter()
        .map(|entry| prim::width(&entry.prefix) + prim::width(&entry.label))
        .max()
        .unwrap_or(0);
    let chrome_w = prim::width(title).max(prim::width(help));
    let w = u16::try_from(content_w.max(chrome_w) + 4)
        .unwrap_or(40)
        .min(area.width);
    let desired_frame_h = u16::try_from(desired_rows + 4).unwrap_or(24);
    let popup = centered_modal(area, w, desired_frame_h);
    f.render_widget(Clear, popup);
    let rows = render_modal_frame(f, popup, t, modal_title(t, title), modal_help(t, help));
    let scroll_area = modal_scroll_area(rows.content);
    let content = scroll_area.content;
    let view_h = content.height as usize;
    let start = picker
        .selected
        .saturating_sub(view_h.saturating_sub(1))
        .min(total.saturating_sub(view_h));
    let end = (start + view_h).min(total);
    let tree_art = Style::new().fg(t.subtle);
    let items: Vec<ListItem> = picker.entries[start..end]
        .iter()
        .map(|entry| {
            let kind_color = if entry.label.starts_with("user:") {
                t.user
            } else if entry.label.starts_with("agent:") {
                t.agent
            } else if entry.label.starts_with("tool:") || entry.label.starts_with("exec:") {
                t.success
            } else {
                t.muted
            };
            let mut label_style = Style::new().fg(kind_color);
            if entry.is_active {
                label_style = label_style.add_modifier(Modifier::BOLD);
            }
            ListItem::new(Line::from(vec![
                Span::styled(entry.prefix.clone(), tree_art),
                Span::styled(entry.label.clone(), label_style),
            ]))
        })
        .collect();
    let list = List::new(items)
        .style(Style::default().fg(t.fg))
        .highlight_style(focus_style(t));
    let selected = (picker.selected < total).then_some(picker.selected.saturating_sub(start));
    let mut state = ListState::default().with_selected(selected);
    f.render_stateful_widget(list, content, &mut state);
    if total > view_h {
        prim::render_scrollbar(
            f,
            scroll_area.gutter,
            start,
            view_h,
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
pub(super) fn render_confirm_modal(f: &mut Frame, area: Rect, app: &mut App) {
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
    let reason = req
        .reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let explanation = match reason {
        lofi_core::ConfirmReason::Policy => "Shell wants to run this command".to_string(),
        lofi_core::ConfirmReason::AutoEvaluating { started_at } => {
            format!("Auto evaluation for {}s...", started_at.elapsed().as_secs())
        }
        lofi_core::ConfirmReason::AutoAsk { reason } => {
            if reason.trim().is_empty() {
                "Auto evaluation asks for your approval.".to_string()
            } else {
                format!("Auto evaluation asks: {reason}")
            }
        }
        lofi_core::ConfirmReason::AutoFailed { reason } => {
            format!("Auto evaluation could not decide: {reason}")
        }
    };
    let explanation_lines = prim::wrap(&explanation, content_w);
    // Commands are preformatted source: preserve leading blank lines and
    // indentation instead of treating them as flow text.
    let cmd_lines = prim::wrap_pre(&req.command, content_w);
    // Keep the dialog compact even on a tall terminal. The command panel is
    // a viewport rather than a truncation point, so every wrapped row remains
    // reachable with the scrolling keys.
    const MAX_COMMAND_ROWS: usize = 12;
    let explanation_rows = explanation_lines.len().max(1);
    let non_command_rows = explanation_rows + 9;
    let available_command_rows = area
        .height
        .saturating_sub(u16::try_from(non_command_rows).unwrap_or(u16::MAX))
        .max(1) as usize;
    let command_rows = cmd_lines
        .len()
        .max(1)
        .min(MAX_COMMAND_ROWS)
        .min(available_command_rows);
    let desired_frame_h = u16::try_from(command_rows + non_command_rows).unwrap_or(u16::MAX);
    let popup = centered_modal(area, w, desired_frame_h);
    f.render_widget(Clear, popup);

    let key = Style::new().fg(t.primary).add_modifier(Modifier::BOLD);
    let help = Line::from(vec![
        Span::styled("↑/↓", key),
        Span::styled(" scroll  ", Style::new().fg(t.subtle)),
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
    let title = format!("Permission Required{counter}");
    let chrome = render_modal_frame(
        f,
        popup,
        t,
        Line::from(Span::styled(
            title,
            Style::new().fg(t.warn).add_modifier(Modifier::BOLD),
        )),
        help,
    );
    if chrome.content.height == 0 {
        return;
    }

    let rows = Layout::vertical([
        Constraint::Length(u16::try_from(explanation_rows).unwrap_or(u16::MAX)),
        Constraint::Length(1), // gap
        Constraint::Length(1), // content label
        Constraint::Min(1),    // command panel
        Constraint::Length(1), // gap
        Constraint::Length(1), // actions
        Constraint::Length(1), // gap
    ])
    .split(chrome.content);
    f.render_widget(
        Paragraph::new(explanation_lines.join(
            "
",
        ))
        .style(Style::new().fg(t.muted)),
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
    let command_scroll_area = modal_scroll_area(command_inner);
    let command_content = command_scroll_area.content;
    f.render_widget(command_panel, rows[3]);
    app.confirm_total = cmd_lines.len();
    app.confirm_view_h = command_content.height as usize;
    let max_scroll = app.confirm_total.saturating_sub(app.confirm_view_h);
    app.confirm_scroll = app.confirm_scroll.min(max_scroll);
    let body_lines: Vec<Line> = cmd_lines
        .iter()
        .skip(app.confirm_scroll)
        .take(app.confirm_view_h)
        .map(|line| {
            Line::from(Span::styled(
                line.clone(),
                Style::new().fg(t.fg).bg(t.panel_bg),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(body_lines), command_content);
    if app.confirm_total > app.confirm_view_h {
        prim::render_scrollbar(
            f,
            command_scroll_area.gutter,
            app.confirm_scroll,
            app.confirm_view_h,
            app.confirm_total,
            t.subtle,
            t.muted,
        );
    }

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
}
