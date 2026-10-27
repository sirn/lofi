use ratatui::text::Span;

use crate::tui::ThinkingBlock;

use crate::tui::view::component::{Component, Cx};
use crate::tui::view::markdown::{
    markdown_body_height, render_markdown_body, render_markdown_body_window,
};
use crate::tui::view::prim::{self, RenderLine};

pub(super) struct UserMessage<'a> {
    pub(super) prompt: &'a str,
}

impl Component for UserMessage<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        let mark = t.user_marker_style();
        render_markdown_body(
            self.prompt.trim(),
            t,
            w,
            content_w,
            t.text_style(),
            move |_| vec![Span::styled("▌ ", mark)],
        )
    }

    fn height(&self, cx: &Cx) -> usize {
        markdown_body_height(self.prompt.trim(), cx.width.saturating_sub(2))
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = t.user_marker_style();
        render_markdown_body_window(
            self.prompt.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            t.text_style(),
            move |_| vec![Span::styled("▌ ", mark)],
            range,
        )
    }
}

pub(super) struct NoticeMessage<'a> {
    pub(super) prompt: &'a str,
}

impl Component for NoticeMessage<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let w = cx.width;
        let content_w = w.saturating_sub(2);
        // Notices (job wake-ups, future automation) use the thinner left
        // quarter-block bar and stay de-emphasized so an app-injected prompt
        // never competes visually with typed input: muted color for marker
        // and body, italic body.
        let mark = t.muted_style();
        let body_style = t.annotation_style();
        render_markdown_body(self.prompt.trim(), t, w, content_w, body_style, move |_| {
            vec![Span::styled("▎ ", mark)]
        })
    }

    fn height(&self, cx: &Cx) -> usize {
        markdown_body_height(self.prompt.trim(), cx.width.saturating_sub(2))
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let t = cx.theme;
        let mark = t.muted_style();
        let body_style = t.annotation_style();
        render_markdown_body_window(
            self.prompt.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            body_style,
            move |_| vec![Span::styled("▎ ", mark)],
            range,
        )
    }
}

pub(super) struct AssistantText<'a> {
    pub(super) text: &'a str,
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
        let mark = t.agent_marker_style();
        render_markdown_body(text, t, w, content_w, t.text_style(), move |_| {
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
        let mark = t.agent_marker_style();
        render_markdown_body_window(
            self.text.trim(),
            t,
            cx.width,
            cx.width.saturating_sub(2),
            t.text_style(),
            move |_| vec![Span::styled("▌ ", mark)],
            range,
        )
    }
}

pub(super) struct Thinking<'a> {
    pub(super) block: &'a ThinkingBlock,
}

impl Component for Thinking<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        let t = cx.theme;
        let body = t.annotation_style();
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
                let thought_lead = vec![Span::raw("  "), Span::styled("◇ ", t.subtle_style())];
                out.push(prim::rline(
                    thought_lead,
                    vec![Span::styled(
                        format!("Thought for {}", prim::fmt_duration(d)),
                        t.subtle_style(),
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
pub(super) fn trim_reasoning_summary(text: &str) -> String {
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
