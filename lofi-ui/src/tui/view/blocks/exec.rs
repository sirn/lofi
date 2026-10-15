use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

use crate::tui::theme::active_indicator;
use crate::tui::{DetailKey, ResultAvailability, ToolCall};

use super::detail::{detail_box, detail_row_count};
use super::native::{split_lines, ExecBlockBranch};
use super::RenderWindow;
use crate::tui::view::component::{Component, Cx};
use crate::tui::view::prim::{self, RenderLine};

pub(super) struct ExecBlock<'a> {
    pub(super) tool: &'a ToolCall,
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
