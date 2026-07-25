//! Semantic theme tokens backed by ANSI 256 colors.
//!
//! Everything in the UI reads from a [`Theme`] via semantic names —
//! `primary`, `success`, `muted`, `surface`, … — never a raw color, so a
//! different palette (dark vs light, 256 vs 16-color) is just a different
//! `Theme` value. [`Theme::dark`] is the default and uses the xterm 256-color
//! cube.

use ratatui::style::Color;

/// The resolved color palette. Colors are ANSI 256 values so the UI works
/// in any terminal that advertises 256-color support without depending on
/// truecolor.
///
/// Token roles (kept deliberately small and generic):
/// - `primary` — brand/accent color.
/// - `user` / `agent` — conversation-role colors.
/// - `success` / `warn` / `error` / `info` — status semantics.
/// - `fg` / `muted` / `subtle` — base, dimmed, and faint text.
/// - `surface` — filled background for fenced code tiles.
/// - `inline_bg` — background for inline code spans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub primary: Color,
    /// User-role labels and indicators. Deliberately matches `primary`.
    pub user: Color,
    /// Agent-role labels. Kept separate from generic informational blue.
    pub agent: Color,
    pub success: Color,
    pub warn: Color,
    pub error: Color,
    pub info: Color,
    pub fg: Color,
    pub muted: Color,
    pub subtle: Color,
    pub surface: Color,
    pub inline_bg: Color,
    /// Filled background of the footer panel (prompt + stats).
    pub panel_bg: Color,

    /// Mouse-selection highlight background in the log.
    pub selection: Color,
    /// Background of the Navigate cursor line in the log.
    pub cursor_line: Color,
    /// Single-cell cursor marker in Select mode (distinct from `selection`).
    pub select_cursor: Color,
}

/// User-message left indicator (distinct from the assistant tone).
pub(crate) fn user_indicator(t: Theme) -> Color {
    t.user
}
/// Agent-response left indicator.
pub(crate) fn agent_indicator(t: Theme) -> Color {
    t.agent
}
/// Active (streaming / running) left indicator.
pub(crate) fn active_indicator(t: Theme) -> Color {
    t.warn
}

impl Theme {
    /// Dark background, xterm 256-color palette.
    pub(crate) fn dark() -> Self {
        Self {
            primary: Color::Indexed(44),  // teal (modus-vivendi accent)
            user: Color::Indexed(44),     // teal — same as primary
            agent: Color::Indexed(5),     // magenta
            success: Color::Indexed(77),  // green
            warn: Color::Indexed(178),    // amber
            error: Color::Indexed(203),   // red
            info: Color::Indexed(75),     // blue
            fg: Color::Indexed(255),      // white
            muted: Color::Indexed(244),   // mid gray
            subtle: Color::Indexed(241),  // outline gray
            surface: Color::Indexed(235), // fenced-code background
            inline_bg: Color::Indexed(236),
            panel_bg: Color::Indexed(232), // near-black footer panel

            selection: Color::Indexed(238),   // one step above surface
            cursor_line: Color::Indexed(234), // faint bar under the nav cursor
            select_cursor: Color::Indexed(60), // slate marker on the select cursor
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
