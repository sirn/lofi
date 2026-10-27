use super::*;

#[test]
fn jobs_modal_opens_only_with_registry() {
    let mut a = app();
    a.open_jobs_modal();
    assert!(a.jobs_modal.is_none());
    a.jobs = Some(lofi_core::JobRegistry::new());
    a.open_jobs_modal();
    assert!(a.jobs_modal.is_some());
    assert!(a.modal_open());
}

#[test]
fn jobs_modal_esc_closes() {
    let mut a = app();
    a.jobs = Some(lofi_core::JobRegistry::new());
    a.open_jobs_modal();
    let esc = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    );
    a.handle_modal_key(&esc);
    assert!(a.jobs_modal.is_none());
}

#[test]
fn jobs_modal_empty_list_no_underflow() {
    let mut a = app();
    a.jobs = Some(lofi_core::JobRegistry::new());
    a.open_jobs_modal();
    let down = crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Down,
        crossterm::event::KeyModifiers::NONE,
    );
    a.handle_modal_key(&down);
    assert_eq!(a.jobs_modal.as_ref().map(|m| m.selected), Some(0));
}

#[test]
fn jobs_terminal_view_clips_to_the_available_viewport() {
    use ratatui::Terminal;

    let mut a = app();
    a.jobs_modal = Some(JobsModalState {
        selected: 0,
        viewing: Some(JobOutputView {
            id: 7,
            content: JobViewContent::Terminal(lofi_core::JobScreen {
                cols: 80,
                rows: 30,
                lines: (0..30)
                    .map(|row| lofi_core::JobScreenLine {
                        spans: vec![lofi_core::JobSpan {
                            text: format!("row-{row:02}"),
                            style: lofi_core::JobStyle::default(),
                        }],
                    })
                    .collect(),
            }),
        }),
        confirm_kill: None,
    });
    let mut term = Terminal::new(TestBackend::new(32, 12)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let screen = (0..12)
        .map(|y| (0..32).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join(
            "
",
        );

    assert!(screen.contains("job 7 terminal"), "{screen}");
    assert!(screen.contains("row-00"), "{screen}");
    assert!(!screen.contains("row-20"), "{screen}");
}

#[test]
fn jobs_terminal_view_preserves_cell_colors_and_attributes() {
    use ratatui::{style::Color, Terminal};

    let mut a = app();
    a.jobs_modal = Some(JobsModalState {
        selected: 0,
        viewing: Some(JobOutputView {
            id: 8,
            content: JobViewContent::Terminal(lofi_core::JobScreen {
                cols: 1,
                rows: 1,
                lines: vec![lofi_core::JobScreenLine {
                    spans: vec![lofi_core::JobSpan {
                        text: "X".to_string(),
                        style: lofi_core::JobStyle {
                            foreground: lofi_core::JobColor::Rgb(1, 2, 3),
                            background: lofi_core::JobColor::Indexed(25),
                            attributes: lofi_core::JobAttributes::BOLD
                                | lofi_core::JobAttributes::ITALIC
                                | lofi_core::JobAttributes::UNDERLINE
                                | lofi_core::JobAttributes::INVERSE,
                        },
                    }],
                }],
            }),
        }),
        confirm_kill: None,
    });
    let mut term = Terminal::new(TestBackend::new(24, 8)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let cell = term
        .backend()
        .buffer()
        .content
        .iter()
        .find(|cell| cell.symbol() == "X")
        .unwrap();

    assert_eq!(cell.fg, Color::Rgb(1, 2, 3));
    assert_eq!(cell.bg, Color::Indexed(25));
    assert!(cell.modifier.contains(Modifier::BOLD));
    assert!(cell.modifier.contains(Modifier::ITALIC));
    assert!(cell.modifier.contains(Modifier::UNDERLINED));
    assert!(cell.modifier.contains(Modifier::REVERSED));
}

#[test]
fn theme_picker_opens_preselected_on_current_mode() {
    use lofi_types::ThemeMode;
    let mut a = app();
    a.update_theme(theme::ThemeEvent::SetMode(ThemeMode::Dark));
    a.open_theme_picker();
    let picker = a.theme_picker.as_ref().unwrap();
    assert_eq!(picker.modes.len(), 3);
    assert_eq!(picker.modes[picker.selected], ThemeMode::Dark);
    assert!(a.modal_open());
}

#[test]
fn theme_picker_confirm_updates_mode_and_clears_frozen_cache() {
    use lofi_types::ThemeMode;
    let mut a = app();
    a.update_theme(theme::ThemeEvent::SetMode(ThemeMode::Dark));
    // Seed the frozen cache so we can prove confirm clears it.
    a.layout.frozen_render.insert(0, Vec::new());
    assert!(a.layout.frozen_render.contains(0));
    a.open_theme_picker();
    // Move from Dark (idx 2) to Light (idx 1).
    a.theme_picker.as_mut().unwrap().selected = 1;
    a.theme_picker_confirm();
    assert!(a.theme_picker.is_none());
    assert_eq!(a.theme_manager.mode(), ThemeMode::Light);
    assert!(!a.layout.frozen_render.contains(0));
}

#[test]
fn theme_update_clears_frozen_only_on_change() {
    let mut a = app();
    a.layout.frozen_render.insert(0, Vec::new());
    assert!(!a.update_theme(theme::ThemeEvent::SetMode(lofi_types::ThemeMode::Dark)));
    assert!(a.layout.frozen_render.contains(0));
    assert!(a.update_theme(theme::ThemeEvent::SetMode(lofi_types::ThemeMode::Light)));
    assert_eq!(a.theme, Theme::light());
    assert!(!a.layout.frozen_render.contains(0));
}

#[test]
fn apply_color_scheme_applies_only_in_auto() {
    use crate::tui::theme::ColorScheme;
    use lofi_types::ThemeMode;
    let mut a = app();
    a.update_theme(theme::ThemeEvent::SetMode(ThemeMode::Auto));
    assert!(a.update_theme(theme::ThemeEvent::ColorScheme(ColorScheme::Light)));
    assert_eq!(a.theme, Theme::light());
    a.update_theme(theme::ThemeEvent::SetMode(ThemeMode::Dark));
    assert!(!a.update_theme(theme::ThemeEvent::ColorScheme(ColorScheme::Light)));
}

#[test]
fn apply_background_applies_only_in_auto() {
    use crate::tui::terminal_bg::Rgb;
    use lofi_types::ThemeMode;
    let mut a = app();
    a.update_theme(theme::ThemeEvent::SetMode(ThemeMode::Auto));
    assert!(a.update_theme(theme::ThemeEvent::Background(Rgb {
        r: 255,
        g: 255,
        b: 255,
    })));
    assert_eq!(a.theme, Theme::light());
    a.update_theme(theme::ThemeEvent::SetMode(ThemeMode::Light));
    assert!(!a.update_theme(theme::ThemeEvent::Background(Rgb { r: 0, g: 0, b: 0 })));
}

#[test]
fn inverted_palette_updates_input_and_cached_transcript() {
    use crate::tui::terminal_bg::Rgb;
    use ratatui::{style::Color, Terminal};

    let mut a = app();
    a.update_theme(theme::ThemeEvent::SetMode(lofi_types::ThemeMode::Light));
    a.insert_str("input");
    a.begin_prompt_turn("prompt".into(), lofi_types::PromptKind::User);
    a.layout.frozen_render.insert(0, Vec::new());
    let white = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    let black = Rgb { r: 0, g: 0, b: 0 };
    assert!(!a.update_theme(theme::ThemeEvent::Palette(255, black)));
    assert!(a.update_theme(theme::ThemeEvent::Palette(232, white)));
    assert!(!a.layout.frozen_render.contains(0));
    assert_eq!(a.theme.panel_bg, Color::Indexed(232));
    assert!(!a.update_theme(theme::ThemeEvent::Palette(231, black)));
    assert!(a.update_theme(theme::ThemeEvent::Palette(16, white)));
    assert_eq!(a.theme.fg, Color::Indexed(231));
    assert_eq!(a.theme.select_cursor, Color::Indexed(66));

    let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
    terminal
        .draw(|f| crate::tui::view::render(f, &mut a))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let input = &buffer[(a.input_rect.x, a.input_rect.y)];
    assert_eq!(input.symbol(), "i");
    assert_eq!(input.bg, Color::Indexed(232));
    assert_eq!(input.fg, Color::Indexed(231));
    assert!(buffer
        .content
        .iter()
        .any(|cell| cell.symbol() == "p" && cell.fg == Color::Indexed(231)));
}

#[test]
fn palette_direction_is_independent_of_theme_mode_and_reply_order() {
    use crate::tui::terminal_bg::Rgb;
    use crate::tui::theme::ColorScheme;
    use lofi_types::ThemeMode;
    use ratatui::style::Color;

    let white = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    let black = Rgb { r: 0, g: 0, b: 0 };
    for mode in [ThemeMode::Auto, ThemeMode::Light, ThemeMode::Dark] {
        let mut a = app();
        a.update_theme(theme::ThemeEvent::SetMode(mode));
        let light = a.theme.is_light();
        a.update_theme(theme::ThemeEvent::Palette(16, white));
        a.update_theme(theme::ThemeEvent::Palette(231, black));
        a.update_theme(theme::ThemeEvent::Palette(232, white));
        a.update_theme(theme::ThemeEvent::Palette(255, black));
        assert_eq!(a.theme.is_light(), light);
        assert_eq!(
            a.theme.panel_bg,
            Color::Indexed(if light { 232 } else { 255 })
        );
        if mode == ThemeMode::Auto {
            a.update_theme(theme::ThemeEvent::ColorScheme(ColorScheme::Light));
            assert_eq!(a.theme.panel_bg, Color::Indexed(232));
            assert_eq!(a.theme.fg, Color::Indexed(231));
        }
        // A partial refresh must not combine a new endpoint with an old one.
        a.update_theme(theme::ThemeEvent::Refresh);
        let previous = a.theme;
        a.update_theme(theme::ThemeEvent::Palette(232, black));
        assert_eq!(a.theme, previous);
        a.update_theme(theme::ThemeEvent::Palette(255, white));
        a.update_theme(theme::ThemeEvent::Palette(231, white));
        a.update_theme(theme::ThemeEvent::Palette(16, black));
        assert_eq!(
            a.theme,
            if a.theme.is_light() {
                Theme::light()
            } else {
                Theme::dark()
            }
        );
    }
}

#[test]
fn missing_or_equal_palette_endpoints_keep_existing_colors() {
    use crate::tui::terminal_bg::Rgb;
    let mut a = app();
    a.update_theme(theme::ThemeEvent::SetMode(lofi_types::ThemeMode::Light));
    let before = a.theme;
    assert!(!a.update_theme(theme::ThemeEvent::Palette(
        232,
        Rgb {
            r: 128,
            g: 128,
            b: 128
        }
    )));
    assert_eq!(a.theme, before);
    assert!(!a.update_theme(theme::ThemeEvent::Palette(
        255,
        Rgb {
            r: 128,
            g: 128,
            b: 128
        }
    )));
    assert_eq!(a.theme, before);
    assert!(!a.update_theme(theme::ThemeEvent::Palette(
        42,
        Rgb {
            r: 255,
            g: 255,
            b: 255
        }
    )));
}

#[test]
fn selected_picker_item_preserves_contrast_after_palette_changes() {
    use crate::tui::terminal_bg::Rgb;
    use lofi_types::ThemeMode;
    use ratatui::Terminal;

    let black = Rgb { r: 0, g: 0, b: 0 };
    let white = Rgb {
        r: 255,
        g: 255,
        b: 255,
    };
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        for reversed in [false, true] {
            let mut a = app();
            a.update_theme(theme::ThemeEvent::SetMode(mode));
            a.update_theme(theme::ThemeEvent::Palette(
                232,
                if reversed { white } else { black },
            ));
            a.update_theme(theme::ThemeEvent::Palette(
                255,
                if reversed { black } else { white },
            ));
            a.open_theme_picker();
            // Select an inactive row so its text does not set an accent color.
            let (selected, label) = if mode == ThemeMode::Light {
                (2, "d")
            } else {
                (1, "l")
            };
            a.theme_picker.as_mut().unwrap().selected = selected;
            let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
            terminal
                .draw(|f| crate::tui::view::render(f, &mut a))
                .unwrap();
            let cell = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .find(|cell| cell.symbol() == label && cell.bg == a.theme.primary)
                .unwrap();
            assert_eq!(cell.fg, a.theme.panel_bg);
            assert!(cell.modifier.contains(Modifier::BOLD));
        }
    }
}

#[test]
fn prompt_event_confirms_a_pre_pushed_prompt() {
    let mut a = app();
    push_turn(&mut a);
    a.begin_prompt_turn("hi".to_string(), lofi_types::PromptKind::User);
    assert_eq!(a.transcript.turns.len(), 2);
    assert_eq!(a.transcript.turns[1].prompt, "hi");
    assert!(a.transcript.turns[1].blocks.is_empty());
    a.apply_event(AgentEvent::Prompt {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    assert_eq!(a.transcript.turns.len(), 2);
    assert!(!a.pending_prompt_start);
}

#[test]
fn prompt_event_pushes_when_no_prompt_is_awaited() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.apply_event(AgentEvent::Prompt {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    assert_eq!(a.transcript.turns.len(), 2);
}

#[test]
fn run_finished_disarms_a_pending_prompt_start() {
    let mut a = app();
    a.begin_prompt_turn("hi".to_string(), lofi_types::PromptKind::User);
    a.run_finished();
    assert!(!a.pending_prompt_start);
    a.apply_event(AgentEvent::Prompt {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    assert_eq!(a.transcript.turns.len(), 2);
}
