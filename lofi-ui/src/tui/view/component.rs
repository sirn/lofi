#![allow(clippy::needless_lifetimes)]

//! The component model for the turn log.
//!
//! Every visual unit implements [`Component`], rendering to an owned block of
//! lines. [`Stack`] composes children with a single gap policy: one blank
//! line between any two non-empty neighbors. Children that render to zero
//! lines (e.g. whitespace-only text/thinking blocks the model emits between a
//! reasoning block and a tool call) are skipped entirely, so they leave no
//! gap and no doubled separator. This is the one place gap rules live,
//! replacing the scattered `push(Line::default())` calls that used to double
//! up blanks.

use crate::tui::theme::Theme;
use crate::tui::App;

use super::prim::{self, RenderLine};

pub struct Cx<'a> {
    pub app: &'a App,
    pub theme: Theme,
    pub width: usize,
    pub active_turn: bool,
}

impl Cx<'_> {
    pub fn spinner(&self) -> usize {
        self.app.spinner_frame() % crate::tui::SPINNER.len()
    }
}

pub trait Component {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine>;

    fn height(&self, cx: &Cx) -> usize {
        self.lines(cx).len()
    }

    /// Render only a component-relative visual-row window. Large components
    /// override this so the retained output is bounded by the viewport.
    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        self.lines(cx)
            .into_iter()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .collect()
    }
}

pub struct Stack<'a> {
    children: Vec<Box<dyn Component + 'a>>,
}

impl<'a> Stack<'a> {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
        }
    }

    pub fn push<C: Component + 'a>(&mut self, child: C) {
        self.children.push(Box::new(child));
    }
}

impl Component for Stack<'_> {
    fn lines(&self, cx: &Cx) -> Vec<RenderLine> {
        self.lines_window(cx, 0..usize::MAX)
    }

    fn height(&self, cx: &Cx) -> usize {
        let mut total = 0usize;
        let mut first = true;
        for child in &self.children {
            let height = child.height(cx);
            if height == 0 {
                continue;
            }
            total = total.saturating_add(height + usize::from(!first));
            first = false;
        }
        total
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let mut out = Vec::with_capacity(range.end.saturating_sub(range.start).min(256));
        let mut pos = 0usize;
        let mut first = true;
        for child in &self.children {
            let height = child.height(cx);
            if height == 0 {
                continue;
            }
            if !first {
                if range.contains(&pos) {
                    out.push(prim::rblank());
                }
                pos = pos.saturating_add(1);
            }
            let end = pos.saturating_add(height);
            if end > range.start && pos < range.end {
                let start = range.start.saturating_sub(pos);
                let stop = range.end.saturating_sub(pos).min(height);
                out.extend(child.lines_window(cx, start..stop));
            }
            pos = end;
            first = false;
            if pos >= range.end {
                break;
            }
        }
        out
    }
}
