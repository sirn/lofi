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
/// - `primary` / `secondary` — brand accents (wordmark, user indicator).
/// - `success` / `warn` / `error` / `info` — status semantics.
/// - `fg` / `muted` / `subtle` — base, dimmed, and faint text.
/// - `surface` — filled background for user messages and code tiles.
/// - `inline_bg` — background for inline code spans.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub primary: Color,
    pub secondary: Color,
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
    /// Exec-block tile backgrounds, one per terminal state.
    pub exec_running_bg: Color,
    pub exec_success_bg: Color,
    pub exec_error_bg: Color,
}

/// User-message left indicator (distinct from the assistant tone).
pub(crate) fn user_indicator(t: Theme) -> Color {
    t.secondary
}
/// Active (streaming / running) left indicator.
pub(crate) fn active_indicator(t: Theme) -> Color {
    t.warn
}

impl Theme {
    /// Dark background, xterm 256-color palette.
    pub(crate) fn dark() -> Self {
        Self {
            primary: Color::Indexed(44),   // teal (modus-vivendi accent)
            secondary: Color::Indexed(44), // teal — user indicator
            success: Color::Indexed(77),   // green
            warn: Color::Indexed(178),     // amber
            error: Color::Indexed(203),    // red
            info: Color::Indexed(75),      // blue
            fg: Color::Indexed(255),       // white
            muted: Color::Indexed(244),    // mid gray
            subtle: Color::Indexed(241),   // outline gray
            surface: Color::Indexed(235),  // user-message / input bg
            inline_bg: Color::Indexed(236),
            panel_bg: Color::Indexed(232), // near-black footer panel

            selection: Color::Indexed(238),   // one step above surface
            cursor_line: Color::Indexed(234), // faint bar under the nav cursor
            select_cursor: Color::Indexed(60), // slate marker on the select cursor
            exec_running_bg: Color::Indexed(236), // gray tile
            exec_success_bg: Color::Indexed(22), // dark green tile
            exec_error_bg: Color::Indexed(52), // dark red tile
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
