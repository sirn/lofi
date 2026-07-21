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
        let mut out = Vec::new();
        let mut first = true;
        for child in &self.children {
            let lines = child.lines(cx);
            if lines.is_empty() {
                continue;
            }
            if !first {
                out.push(prim::rblank());
            }
            out.extend(lines);
            first = false;
        }
        out
    }
}