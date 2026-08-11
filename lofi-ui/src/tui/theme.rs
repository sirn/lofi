use ratatui::style::Color;

use lofi_types::ThemeMode;

/// The resolved color palette. Colors are ANSI 256 values so the UI works
/// in any terminal that advertises 256-color support without depending on
/// truecolor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub primary: Color,
    pub user: Color,
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
    pub panel_bg: Color,

    pub selection: Color,
    pub cursor_line: Color,
    pub select_cursor: Color,
}

pub(crate) fn user_indicator(t: Theme) -> Color {
    t.user
}
pub(crate) fn agent_indicator(t: Theme) -> Color {
    t.agent
}
pub(crate) fn active_indicator(t: Theme) -> Color {
    t.warn
}

struct Base16Accents {
    primary: Color,
    user: Color,
    agent: Color,
    success: Color,
    warn: Color,
    error: Color,
    info: Color,
}

// Accents use ANSI base-16 so indicators track whatever palette the user
// has configured in their terminal; the rest of the palette pins explicit
// 256-index values so multi-shade surfaces stay under our control.
const BASE16: Base16Accents = Base16Accents {
    primary: Color::Cyan,
    user: Color::Cyan,
    agent: Color::Magenta,
    success: Color::Green,
    warn: Color::Yellow,
    error: Color::Red,
    info: Color::Blue,
};

impl Theme {
    pub(crate) fn dark() -> Self {
        Self {
            primary: BASE16.primary,
            user: BASE16.user,
            agent: BASE16.agent,
            success: BASE16.success,
            warn: BASE16.warn,
            error: BASE16.error,
            info: BASE16.info,
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

    pub(crate) fn light() -> Self {
        Self {
            primary: BASE16.primary,
            user: BASE16.user,
            agent: BASE16.agent,
            success: BASE16.success,
            warn: BASE16.warn,
            error: BASE16.error,
            info: BASE16.info,
            fg: Color::Indexed(16),       // pure black from the colour cube
            muted: Color::Indexed(240),   // operandi fg_dim #595959
            subtle: Color::Indexed(246),  // operandi border #919191
            surface: Color::Indexed(255), // operandi bg_dim #f2f2f2
            inline_bg: Color::Indexed(254),
            // Dark sinks the footer one step below surface; on the light
            // end that shift is impossible (surface is already at the
            // brightest grey), so panel shares the surface grey.
            panel_bg: Color::Indexed(255),

            selection: Color::Indexed(252),
            cursor_line: Color::Indexed(254),
            select_cursor: Color::Indexed(152), // operandi bg_hover #b2e4dc
        }
    }

    /// `Auto` falls back to `dark()` on a failed probe: dark text on an
    /// unknown background is more likely to read than washed-out light.
    pub(crate) fn resolve(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
            ThemeMode::Auto => {
                let bg = crate::tui::terminal_bg::query_background(
                    std::time::Duration::from_millis(150),
                );
                match bg {
                    Some(rgb) if rgb.luminance() > 0.5 => Self::light(),
                    _ => Self::dark(),
                }
            }
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
