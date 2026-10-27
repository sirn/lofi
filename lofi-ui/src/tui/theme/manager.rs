use lofi_types::ThemeMode;

use super::{ColorScheme, TerminalPalette, Theme};
use crate::tui::terminal_bg::Rgb;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ThemeEvent {
    Refresh,
    SetMode(ThemeMode),
    ColorScheme(ColorScheme),
    Background(Rgb),
    Palette(u8, Rgb),
}

#[derive(Clone, Copy, Default)]
pub(crate) struct ThemeQueries {
    pub reports: Option<bool>,
    pub appearance: bool,
    pub palette: bool,
}

/// Owns detection and palette state without terminal I/O or rendering caches.
#[derive(Default)]
pub(crate) struct ThemeManager {
    mode: ThemeMode,
    scheme: Option<ColorScheme>,
    background: Option<Rgb>,
    palette: TerminalPalette,
}

impl ThemeManager {
    pub(crate) fn mode(&self) -> ThemeMode {
        self.mode
    }

    pub(crate) fn theme(&self) -> Theme {
        let base = match self.mode {
            ThemeMode::Light => Theme::light(),
            ThemeMode::Dark => Theme::dark(),
            // An explicit preference takes priority over background luminance,
            // so a late background reply cannot undo a scheme notification.
            ThemeMode::Auto => self.scheme.map_or_else(
                || {
                    self.background
                        .map_or_else(Theme::dark, Theme::from_background)
                },
                Theme::from_scheme,
            ),
        };
        self.palette.apply(base)
    }

    pub(crate) fn palette_directions(&self) -> (bool, bool) {
        (self.palette.gray_reversed, self.palette.cube_reversed)
    }

    pub(crate) fn update(&mut self, event: ThemeEvent) -> ThemeQueries {
        let mut queries = ThemeQueries::default();
        match event {
            ThemeEvent::SetMode(mode) => {
                self.mode = mode;
                queries.reports = Some(mode == ThemeMode::Auto);
                queries.appearance = mode == ThemeMode::Auto;
                queries.palette = true;
            }
            ThemeEvent::Refresh => {
                queries.appearance = self.mode == ThemeMode::Auto;
                queries.palette = true;
            }
            ThemeEvent::ColorScheme(scheme) => {
                self.scheme = Some(scheme);
                queries.palette = true;
            }
            ThemeEvent::Background(rgb) => self.background = Some(rgb),
            ThemeEvent::Palette(index, rgb) => self.palette.observe(index, rgb),
        }
        if queries.palette {
            self.palette.begin_query();
        }
        queries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };
    const WHITE: Rgb = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };

    #[test]
    fn auto_uses_dark_until_background_or_scheme_is_known() {
        let mut manager = ThemeManager::default();
        assert_eq!(manager.mode(), ThemeMode::Auto);
        assert_eq!(manager.theme(), Theme::dark());
        manager.update(ThemeEvent::Background(WHITE));
        assert_eq!(manager.theme(), Theme::light());
        manager.update(ThemeEvent::Background(BLACK));
        assert_eq!(manager.theme(), Theme::dark());
    }

    #[test]
    fn scheme_takes_priority_in_either_reply_order() {
        for scheme in [ColorScheme::Light, ColorScheme::Dark] {
            for scheme_first in [false, true] {
                let mut manager = ThemeManager::default();
                let background = if scheme == ColorScheme::Light {
                    BLACK
                } else {
                    WHITE
                };
                let events = [
                    ThemeEvent::ColorScheme(scheme),
                    ThemeEvent::Background(background),
                ];
                for event in if scheme_first {
                    events
                } else {
                    [events[1], events[0]]
                } {
                    manager.update(event);
                }
                assert_eq!(manager.theme(), Theme::from_scheme(scheme));
                manager.update(ThemeEvent::Refresh);
                manager.update(ThemeEvent::Background(background));
                assert_eq!(manager.theme(), Theme::from_scheme(scheme));
                let next = if scheme == ColorScheme::Light {
                    ColorScheme::Dark
                } else {
                    ColorScheme::Light
                };
                manager.update(ThemeEvent::ColorScheme(next));
                assert_eq!(manager.theme(), Theme::from_scheme(next));
            }
        }
    }

    #[test]
    fn forced_modes_keep_detection_for_return_to_auto() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let mut manager = ThemeManager::default();
            manager.update(ThemeEvent::SetMode(mode));
            let forced = manager.theme();
            let scheme = if mode == ThemeMode::Light {
                ColorScheme::Dark
            } else {
                ColorScheme::Light
            };
            manager.update(ThemeEvent::ColorScheme(scheme));
            manager.update(ThemeEvent::Background(BLACK));
            assert_eq!(manager.theme(), forced);
            manager.update(ThemeEvent::SetMode(ThemeMode::Auto));
            assert_eq!(manager.theme(), Theme::from_scheme(scheme));
        }
    }

    #[test]
    fn query_requests_follow_mode_and_event() {
        let mut manager = ThemeManager::default();
        for mode in [ThemeMode::Auto, ThemeMode::Light, ThemeMode::Dark] {
            let queries = manager.update(ThemeEvent::SetMode(mode));
            assert_eq!(queries.reports, Some(mode == ThemeMode::Auto));
            assert_eq!(queries.appearance, mode == ThemeMode::Auto);
            assert!(queries.palette);
            let queries = manager.update(ThemeEvent::Refresh);
            assert_eq!(queries.reports, None);
            assert_eq!(queries.appearance, mode == ThemeMode::Auto);
            assert!(queries.palette);
            let queries = manager.update(ThemeEvent::ColorScheme(ColorScheme::Light));
            assert_eq!(queries.reports, None);
            assert!(!queries.appearance);
            assert!(queries.palette);
        }
        for event in [
            ThemeEvent::Background(WHITE),
            ThemeEvent::Palette(232, BLACK),
        ] {
            let queries = manager.update(event);
            assert_eq!(queries.reports, None);
            assert!(!queries.appearance);
            assert!(!queries.palette);
        }
    }
}
