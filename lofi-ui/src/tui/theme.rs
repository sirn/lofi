use ratatui::style::Color;

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

impl Theme {
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
