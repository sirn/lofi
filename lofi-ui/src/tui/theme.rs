//! Semantic theme tokens backed by ANSI 256 colors.
//!
//! Everything in the UI reads from a [`Theme`] via semantic names —
//! `primary`, `success`, `muted`, `surface`, … — never a raw color, so a
//! different palette (dark vs light, 256 vs 16-color) is just a different
//! `Theme` value. [`Theme::dark`] is the default and uses the xterm 256-color
//! cube; [`Theme::light`] inverts the text tones for light backgrounds.

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
    /// Mouse-selection highlight background in the log.
    pub selection: Color,
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
            secondary: Color::Indexed(44),  // teal — user indicator
            success: Color::Indexed(77),    // green
            warn: Color::Indexed(178),      // amber
            error: Color::Indexed(203),     // red
            info: Color::Indexed(75),       // blue
            fg: Color::Indexed(255),        // white
            muted: Color::Indexed(244),     // mid gray
            subtle: Color::Indexed(241),    // outline gray
            surface: Color::Indexed(235),   // user-message / input bg
            inline_bg: Color::Indexed(236),
            
            selection: Color::Indexed(238),        // one step above surface
        }
    }

    /// Light background variant: text tones inverted, accents retained.
    #[allow(dead_code)]
    pub(crate) fn light() -> Self {
        Self {
            primary: Color::Indexed(55),    // deep purple
            secondary: Color::Indexed(162), // magenta
            success: Color::Indexed(29),    // forest green
            warn: Color::Indexed(130),      // amber
            error: Color::Indexed(124),     // red
            info: Color::Indexed(25),       // blue
            fg: Color::Indexed(235),        // near-black
            muted: Color::Indexed(242),     // mid gray
            subtle: Color::Indexed(248),    // light gray
            surface: Color::Indexed(253),   // pale tile
            inline_bg: Color::Indexed(252),
            
            selection: Color::Indexed(248),        // light gray
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}