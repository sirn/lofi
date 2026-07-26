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

/// Render context handed to every component.
pub struct Cx<'a> {
    pub app: &'a App,
    pub theme: Theme,
    pub width: usize,
    /// Whether this turn is the live, in-progress one.
    pub active_turn: bool,
}

impl Cx<'_> {
    /// Current spinner frame index, for working-status icons.
    pub fn spinner(&self) -> usize {
        self.app.spinner_frame() % crate::tui::SPINNER.len()
    }
}

/// A renderable log unit.
pub trait Component {
    /// Render this unit to an owned block of lines, each tagged with the
    /// char range of its selectable content.
    fn lines(&self, cx: &Cx) -> Vec<RenderLine>;

    /// Number of visual rows without retaining their rendered representation.
    /// Components with potentially large bodies override this together with
    /// `lines_window`; the default keeps small components simple.
    fn height(&self, cx: &Cx) -> usize {
        self.lines(cx).len()
    }

    /// Render only visual rows in `range` (component-relative).
    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        self.lines(cx)
            .into_iter()
            .skip(range.start)
            .take(range.end.saturating_sub(range.start))
            .collect()
    }
}

/// A vertical stack of components joined by a blank line; children that
/// render to zero lines are skipped, so no gap is left around them.
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
            if !first {
                total = total.saturating_add(1);
            }
            total = total.saturating_add(height);
            first = false;
        }
        total
    }

    fn lines_window(&self, cx: &Cx, range: std::ops::Range<usize>) -> Vec<RenderLine> {
        let mut out = Vec::new();
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
