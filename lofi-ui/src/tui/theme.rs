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

impl Theme {
    /// Light variant — modus-operandi mapped to ANSI 256 indexes. Surface
    /// tones sit at the light end of the gray run (higher indexes); the
    /// hierarchy mirrors dark's `panel < cursor < surface < inline <
    /// selection` ordering inverted toward lighter grays.
    pub(crate) fn light() -> Self {
        Self {
            primary: Color::Indexed(24),  // operandi cyan    #005e8b
            user: Color::Indexed(24),
            agent: Color::Indexed(53),    // operandi magenta #721045
            success: Color::Indexed(22),  // operandi green   #006800
            warn: Color::Indexed(58),     // operandi yellow  #6f5500
            error: Color::Indexed(124),   // operandi red     #a60000
            info: Color::Indexed(25),     // operandi blue    #0031a9
            fg: Color::Indexed(16),       // pure black from the colour cube
            muted: Color::Indexed(240),   // operandi fg_dim  #595959
            subtle: Color::Indexed(246),  // operandi border  #919191
            surface: Color::Indexed(255), // operandi bg_dim  #f2f2f2 ≈ #eeeeee
            inline_bg: Color::Indexed(254),
            panel_bg: Color::Indexed(252), // slight-gray footer

            selection: Color::Indexed(253), // one step below inline_bg
            cursor_line: Color::Indexed(254),
            select_cursor: Color::Indexed(152), // operandi bg_hover #b2e4dc
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}
