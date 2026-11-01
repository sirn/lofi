use super::*;
use crate::tui::app_render::counter_text;

fn app_with_timers() -> App {
    let mut a = app();
    a.timers = Some(lofi_core::TimerRegistry::new());
    a
}

/// Draw one frame and return its text, one line per row.
fn screen(a: &mut App) -> String {
    use ratatui::Terminal;
    let mut term = Terminal::new(TestBackend::new(100, 30)).unwrap();
    term.draw(|f| crate::tui::view::render(f, a)).unwrap();
    let buf = term.backend().buffer();
    let area = buf.area;
    (area.top()..area.bottom())
        .map(|y| {
            (area.left()..area.right())
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_counter_counts_both_kinds_of_running_work() {
    let a = app();
    assert_eq!(a.background_counter(true), None, "nothing runs yet");
    assert_eq!(
        counter_text(2, 1, false, true).as_deref(),
        Some("2 jobs · 1 subagent")
    );
    assert_eq!(counter_text(1, 0, false, true).as_deref(), Some("1 job"));
    assert_eq!(
        counter_text(0, 3, false, true).as_deref(),
        Some("3 subagents")
    );
    // A narrow footer keeps the counts and drops the words.
    assert_eq!(counter_text(2, 1, false, false).as_deref(), Some("2j · 1s"));
    // A timer only waits, so it never reaches this count.
    assert_eq!(counter_text(0, 0, false, true), None);
}

#[test]
fn a_question_or_an_unread_notice_adds_the_dot() {
    assert_eq!(
        counter_text(1, 0, true, true).as_deref(),
        Some("1 job ●"),
        "the dot marks work waiting on the user"
    );
}

#[test]
fn the_counter_keeps_one_row_on_a_narrow_footer() {
    let a = app();
    // Nothing running: the row is the usage detail alone, and a terminal
    // too narrow for it stays empty instead of wrapping.
    let wide: String = a
        .render_footer_left(80)
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(!wide.contains('●'), "{wide}");
    let tiny: String = a
        .render_footer_left(1)
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert!(tiny.is_empty(), "{tiny}");
}

#[test]
fn a_short_footer_drops_the_notice_before_the_counter() {
    use crate::tui::view::footer_rows;
    assert_eq!(footer_rows(6), (3, 1));
    assert_eq!(footer_rows(4), (1, 1), "the smallest footer with a notice");
    assert_eq!(
        footer_rows(3),
        (1, 0),
        "one row short: the notice goes, the counter and the prompt stay"
    );
    assert_eq!(footer_rows(2), (0, 0), "the counter row keeps its own row");
}

#[test]
fn the_dialog_opens_on_the_tab_its_command_names() {
    let mut a = app_with_timers();
    assert!(a.slash_command("/job"));
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Jobs)
    );
    assert!(a.slash_command("/subagent"));
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Subagents)
    );
    assert!(a.slash_command("/timer"));
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Timers)
    );
}

#[test]
fn tab_keys_cycle_and_numbers_jump() {
    let mut a = app_with_timers();
    a.open_background_modal(BackgroundTab::Subagents);
    let mut run = None;
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Jobs)
    );
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Timers)
    );
    // Wrapping past the last tab returns to the first.
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Subagents)
    );
    handle_event(&plain_key(KeyCode::Char('t')), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Timers)
    );
}

#[test]
fn switching_tabs_keeps_each_tabs_selection() {
    let mut a = app_with_timers();
    a.open_background_modal(BackgroundTab::Timers);
    let timers = a.timers.clone().unwrap();
    for _ in 0..3 {
        timers.schedule_every(None, std::time::Duration::from_mins(1), None);
    }
    a.refresh_background_modal();
    let mut run = None;
    handle_event(&plain_key(KeyCode::Down), &mut a, None, &mut run);
    handle_event(&plain_key(KeyCode::Down), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal
            .as_ref()
            .map(|modal| modal.timers.selected),
        Some(2)
    );
    // A letter jumps to its tab; the row list keeps only the arrows, so a
    // job row below the first one stays reachable.
    handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.tab),
        Some(BackgroundTab::Jobs)
    );
    assert_eq!(
        a.background_modal.as_ref().map(|modal| modal.jobs.selected),
        Some(0)
    );
    handle_event(&plain_key(KeyCode::Char('t')), &mut a, None, &mut run);
    assert_eq!(
        a.background_modal
            .as_ref()
            .map(|modal| modal.timers.selected),
        Some(2)
    );
}

#[test]
fn x_stops_the_selected_timer_after_a_confirm() {
    let mut a = app_with_timers();
    let timers = a.timers.clone().unwrap();
    timers.schedule_every(None, std::time::Duration::from_mins(1), None);
    a.open_background_modal(BackgroundTab::Timers);
    a.refresh_background_modal();
    let mut run = None;
    handle_event(&plain_key(KeyCode::Char('x')), &mut a, None, &mut run);
    assert!(
        a.background_modal
            .as_ref()
            .is_some_and(|modal| modal.confirm.is_some()),
        "x arms a confirm"
    );
    // Anything but y cancels the armed kill.
    handle_event(&plain_key(KeyCode::Char('n')), &mut a, None, &mut run);
    assert_eq!(timers.listing().len(), 1, "n must not stop the timer");
    handle_event(&plain_key(KeyCode::Char('x')), &mut a, None, &mut run);
    handle_event(&plain_key(KeyCode::Char('y')), &mut a, None, &mut run);
    assert!(timers.listing().is_empty(), "y stops the timer");
}

#[test]
fn shift_x_asks_before_stopping_every_row_of_the_tab() {
    let mut a = app_with_timers();
    let timers = a.timers.clone().unwrap();
    timers.schedule_every(None, std::time::Duration::from_mins(1), None);
    timers.schedule_every(None, std::time::Duration::from_secs(90), None);
    a.open_background_modal(BackgroundTab::Timers);
    a.refresh_background_modal();
    let mut run = None;
    handle_event(&plain_key(KeyCode::Char('X')), &mut a, None, &mut run);
    assert_eq!(timers.listing().len(), 2, "X must ask first");
    handle_event(&plain_key(KeyCode::Char('y')), &mut a, None, &mut run);
    assert!(timers.listing().is_empty());
}

#[test]
fn ctrl_b_opens_the_dialog_only_on_an_empty_prompt() {
    let mut a = app_with_timers();
    let mut run = None;
    a.set_input("typing".to_string());
    handle_event(&ctrl_key(KeyCode::Char('b')), &mut a, None, &mut run);
    assert!(a.background_modal.is_none(), "the readline binding keeps b");
    a.set_input(String::new());
    handle_event(&ctrl_key(KeyCode::Char('b')), &mut a, None, &mut run);
    assert!(a.background_modal.is_some(), "ctrl+b opens the dialog");
}

#[test]
fn the_dialog_names_every_tab_and_lists_the_active_ones_rows() {
    let mut a = app_with_timers();
    let timers = a.timers.clone().unwrap();
    timers.schedule_at(
        None,
        std::time::SystemTime::now() + std::time::Duration::from_mins(2),
        Some("poll the build".to_string()),
    );
    a.open_background_modal(BackgroundTab::Timers);
    let screen = screen(&mut a);
    for expected in [
        "Subagents (0)",
        "Jobs (0)",
        "Timers (1)",
        "poll the build",
        "once",
    ] {
        assert!(screen.contains(expected), "missing {expected}:\n{screen}");
    }
}

#[test]
fn an_empty_tab_says_so_while_the_others_keep_their_counts() {
    let mut a = app_with_timers();
    a.timers
        .as_ref()
        .unwrap()
        .schedule_every(None, std::time::Duration::from_mins(1), None);
    a.open_background_modal(BackgroundTab::Jobs);
    let screen = screen(&mut a);
    assert!(screen.contains("no jobs"), "{screen}");
    assert!(screen.contains("Timers (1)"), "{screen}");
}
