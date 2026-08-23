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
    a.theme_mode = ThemeMode::Dark; // test-only construction defaults to Auto
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
    a.theme_mode = ThemeMode::Dark;
    // Seed the frozen cache so we can prove confirm clears it.
    a.frozen_render.insert(0, Vec::new());
    assert!(a.frozen_render.contains(0));
    a.open_theme_picker();
    // Move from Dark (idx 2) to Light (idx 1).
    a.theme_picker.as_mut().unwrap().selected = 1;
    a.theme_picker_confirm();
    assert!(a.theme_picker.is_none());
    assert_eq!(a.theme_mode, ThemeMode::Light);
    assert!(!a.frozen_render.contains(0));
}

#[test]
fn apply_resolved_theme_clears_frozen_only_on_change() {
    let mut a = app();
    a.theme = Theme::dark();
    a.frozen_render.insert(0, Vec::new());
    assert!(!a.apply_resolved_theme(Theme::dark()));
    assert!(a.frozen_render.contains(0));
    assert!(a.apply_resolved_theme(Theme::light()));
    assert_eq!(a.theme, Theme::light());
    assert!(!a.frozen_render.contains(0));
}

#[test]
fn apply_color_scheme_applies_only_in_auto() {
    use crate::tui::tty_events::ColorScheme;
    use lofi_types::ThemeMode;
    let mut a = app();
    a.theme_mode = ThemeMode::Auto;
    a.theme = Theme::dark();
    assert!(a.apply_color_scheme(ColorScheme::Light));
    assert_eq!(a.theme, Theme::light());
    a.theme_mode = ThemeMode::Dark;
    assert!(!a.apply_color_scheme(ColorScheme::Light));
}

#[test]
fn notify_lines_counts_verbose_chip_width() {
    let msg = "an error long enough to matter when the verbose chip eats ten cells off the available width of the line".to_string();
    let mut quiet = app();
    quiet.notify(NotifyKind::Error, msg.clone());
    let mut verbose = app();
    verbose.verbose = true;
    verbose.notify(NotifyKind::Error, msg);
    assert!(verbose.notify_lines(60) >= quiet.notify_lines(60));
}

#[test]
fn turn_start_confirms_a_pre_pushed_prompt() {
    let mut a = app();
    push_turn(&mut a);
    a.begin_prompt_turn("hi".to_string(), lofi_types::PromptKind::User);
    assert_eq!(a.turns.len(), 2);
    assert_eq!(a.turns[1].prompt, "hi");
    assert!(a.turns[1].blocks.is_empty());
    a.apply_event(AgentEvent::TurnStart {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    assert_eq!(a.turns.len(), 2);
    assert!(!a.pending_prompt_start);
}

#[test]
fn turn_start_pushes_when_no_prompt_is_awaited() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    a.apply_event(AgentEvent::TurnStart {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    assert_eq!(a.turns.len(), 2);
}

#[test]
fn run_finished_disarms_a_pending_prompt_start() {
    let mut a = app();
    a.begin_prompt_turn("hi".to_string(), lofi_types::PromptKind::User);
    a.run_finished();
    assert!(!a.pending_prompt_start);
    a.apply_event(AgentEvent::TurnStart {
        prompt: "hi".to_string(),
        kind: lofi_types::PromptKind::User,
    });
    assert_eq!(a.turns.len(), 2);
}
