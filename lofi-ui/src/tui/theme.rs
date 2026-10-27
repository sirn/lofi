use ratatui::style::{Color, Modifier, Style};

mod manager;
pub(crate) use manager::{ThemeEvent, ThemeManager, ThemeQueries};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ColorScheme {
    Dark,
    Light,
}

/// The resolved color palette. Colors are ANSI 256 values so the UI works
/// in any terminal that advertises 256-color support without depending on
/// truecolor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    light: bool,
}

/// The grayscale ramp and color cube can have independent directions.
#[derive(Default)]
struct TerminalPalette {
    gray: [Option<crate::tui::terminal_bg::Rgb>; 2],
    cube: [Option<crate::tui::terminal_bg::Rgb>; 2],
    pub gray_reversed: bool,
    pub cube_reversed: bool,
}

impl TerminalPalette {
    pub(crate) fn begin_query(&mut self) {
        self.gray = [None; 2];
        self.cube = [None; 2];
    }

    pub(crate) fn observe(&mut self, index: u8, rgb: crate::tui::terminal_bg::Rgb) {
        let (pair, reversed, endpoint) = match index {
            232 => (&mut self.gray, &mut self.gray_reversed, 0),
            255 => (&mut self.gray, &mut self.gray_reversed, 1),
            16 => (&mut self.cube, &mut self.cube_reversed, 0),
            231 => (&mut self.cube, &mut self.cube_reversed, 1),
            _ => return,
        };
        pair[endpoint] = Some(rgb);
        if let [Some(first), Some(last)] = *pair {
            let first = first.luminance();
            let last = last.luminance();
            if first > last {
                *reversed = true;
            } else if first < last {
                *reversed = false;
            }
        }
    }

    pub(crate) fn apply(&self, mut theme: Theme) -> Theme {
        for color in [
            &mut theme.fg,
            &mut theme.muted,
            &mut theme.subtle,
            &mut theme.surface,
            &mut theme.inline_bg,
            &mut theme.panel_bg,
            &mut theme.selection,
            &mut theme.cursor_line,
            &mut theme.select_cursor,
        ] {
            let Color::Indexed(index) = *color else {
                continue;
            };
            *color = match index {
                232..=255 if self.gray_reversed => Color::Indexed(232 + (255 - index)),
                16..=231 if self.cube_reversed => {
                    let value = i16::from(index - 16);
                    let (r, g, b) = (value / 36, value / 6 % 6, value % 6);
                    // Reflect lightness without turning blue into yellow.
                    let offset = 5 - r.max(g).max(b) - r.min(g).min(b);
                    let mapped = 16 + (r + offset) * 36 + (g + offset) * 6 + b + offset;
                    Color::Indexed(u8::try_from(mapped).unwrap_or(index))
                }
                _ => *color,
            };
        }
        theme
    }
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
// has configured in their terminal. Extended indices are adjusted to the
// reported grayscale and color-cube directions.
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
    pub(crate) fn text_style(self) -> Style {
        Style::new().fg(self.fg)
    }

    pub(crate) fn muted_style(self) -> Style {
        Style::new().fg(self.muted)
    }

    pub(crate) fn subtle_style(self) -> Style {
        Style::new().fg(self.subtle)
    }

    pub(crate) fn annotation_style(self) -> Style {
        self.muted_style().add_modifier(Modifier::ITALIC)
    }

    pub(crate) fn heading_style(self) -> Style {
        self.text_style().add_modifier(Modifier::BOLD)
    }

    pub(crate) fn accent_heading_style(self) -> Style {
        Style::new().fg(self.primary).add_modifier(Modifier::BOLD)
    }

    pub(crate) fn key_hint_style(self) -> Style {
        self.accent_heading_style()
    }

    /// Background only, so panels preserve their content's text styles.
    pub(crate) fn panel_style(self) -> Style {
        Style::new().bg(self.panel_bg)
    }

    pub(crate) fn badge_style(self, accent: Color) -> Style {
        Style::new().fg(self.on_accent_text()).bg(accent)
    }

    pub(crate) fn selected_item_style(self) -> Style {
        self.badge_style(self.primary).add_modifier(Modifier::BOLD)
    }

    pub(crate) fn user_marker_style(self) -> Style {
        Style::new().fg(self.user)
    }

    pub(crate) fn agent_marker_style(self) -> Style {
        Style::new().fg(self.agent)
    }

    /// Foreground for chips/badges drawn *on* an accent background.
    /// `panel_bg` instead of `fg` because base-16 accents are picked to
    /// read against the panel; pairing accent fg with accent bg inverts
    /// polarity under either theme (dark panel ≈ black on bright accents,
    /// light panel ≈ white on mid-tone accents) and lands with high contrast
    /// on both.
    pub(crate) fn on_accent_text(&self) -> Color {
        self.panel_bg
    }

    pub(crate) fn is_light(self) -> bool {
        self.light
    }

    pub(crate) fn dark() -> Self {
        Self {
            light: false,
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
            light: true,
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

    fn from_scheme(scheme: ColorScheme) -> Self {
        match scheme {
            ColorScheme::Light => Self::light(),
            ColorScheme::Dark => Self::dark(),
        }
    }

    pub(crate) fn from_background(rgb: crate::tui::terminal_bg::Rgb) -> Self {
        if rgb.luminance() > 0.5 {
            Self::light()
        } else {
            Self::dark()
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::terminal_bg::Rgb;

    #[test]
    fn palette_directions_are_independent_and_preserve_accents() {
        let black = Rgb { r: 0, g: 0, b: 0 };
        let white = Rgb {
            r: 255,
            g: 255,
            b: 255,
        };
        for gray_reversed in [false, true] {
            for cube_reversed in [false, true] {
                let mut palette = TerminalPalette::default();
                palette.observe(232, if gray_reversed { white } else { black });
                palette.observe(255, if gray_reversed { black } else { white });
                palette.observe(231, if cube_reversed { black } else { white });
                palette.observe(16, if cube_reversed { white } else { black });
                let light = palette.apply(Theme::light());
                let dark = palette.apply(Theme::dark());
                assert_eq!(
                    light.panel_bg,
                    Color::Indexed(if gray_reversed { 232 } else { 255 })
                );
                assert_eq!(
                    dark.fg,
                    Color::Indexed(if gray_reversed { 232 } else { 255 })
                );
                assert_eq!(
                    light.fg,
                    Color::Indexed(if cube_reversed { 231 } else { 16 })
                );
                assert_eq!(
                    light.select_cursor,
                    Color::Indexed(if cube_reversed { 66 } else { 152 })
                );
                assert_eq!(
                    dark.select_cursor,
                    Color::Indexed(if cube_reversed { 146 } else { 60 })
                );
                assert_eq!(light.primary, Theme::light().primary);
                assert_eq!(dark.user, Theme::dark().user);
                assert_eq!(palette.apply(light), Theme::light());
                palette.begin_query();
                palette.observe(232, black);
                palette.observe(255, black);
                assert_eq!(palette.gray_reversed, gray_reversed);
            }
        }
    }

    #[test]
    fn semantic_styles_preserve_resolved_colors_and_attributes() {
        for base in [Theme::light(), Theme::dark()] {
            for gray_reversed in [false, true] {
                for cube_reversed in [false, true] {
                    let palette = TerminalPalette {
                        gray_reversed,
                        cube_reversed,
                        ..TerminalPalette::default()
                    };
                    let t = palette.apply(base);
                    let cases = [
                        (t.text_style(), t.fg, Modifier::empty()),
                        (t.muted_style(), t.muted, Modifier::empty()),
                        (t.subtle_style(), t.subtle, Modifier::empty()),
                        (t.annotation_style(), t.muted, Modifier::ITALIC),
                        (t.heading_style(), t.fg, Modifier::BOLD),
                        (t.accent_heading_style(), t.primary, Modifier::BOLD),
                        (t.key_hint_style(), t.primary, Modifier::BOLD),
                        (t.user_marker_style(), t.user, Modifier::empty()),
                        (t.agent_marker_style(), t.agent, Modifier::empty()),
                    ];
                    for (style, fg, modifiers) in cases {
                        assert_eq!(style.fg, Some(fg));
                        assert_eq!(style.bg, None);
                        assert_eq!(style.add_modifier, modifiers);
                        assert_eq!(style.sub_modifier, Modifier::empty());
                    }
                    assert_eq!(t.panel_style().fg, None);
                    assert_eq!(t.panel_style().bg, Some(t.panel_bg));
                    let annotation = t.annotation_style().patch(t.panel_style());
                    assert_eq!(annotation.fg, Some(t.muted));
                    assert_eq!(annotation.bg, Some(t.panel_bg));
                    assert_eq!(annotation.add_modifier, Modifier::ITALIC);
                    let selected = t.selected_item_style();
                    assert_eq!(selected.fg, Some(t.panel_bg));
                    assert_eq!(selected.bg, Some(t.primary));
                    assert_eq!(selected.add_modifier, Modifier::BOLD);
                    for accent in [t.primary, t.warn, t.error, t.success, t.info] {
                        let badge = t.badge_style(accent);
                        assert_eq!(badge.fg, Some(t.panel_bg));
                        assert_eq!(badge.bg, Some(accent));
                        assert_eq!(badge.add_modifier, Modifier::empty());
                    }
                }
            }
        }
    }

    #[test]
    fn from_background_splits_on_luminance() {
        assert_eq!(
            Theme::from_background(Rgb {
                r: 255,
                g: 255,
                b: 255,
            }),
            Theme::light()
        );
        assert_eq!(
            Theme::from_background(Rgb { r: 0, g: 0, b: 0 }),
            Theme::dark()
        );
    }
}
