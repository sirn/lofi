use super::*;
use crate::tui::app_render::counter_text;

fn stale_job(id: u64, cmd: Option<&str>) -> lofi_core::session::replay::OutstandingJob {
    lofi_core::session::replay::OutstandingJob {
        id,
        cmd: cmd.map(str::to_owned),
    }
}

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

/// A transcript with only the header, ready to take events.
fn session_file(dir: &tempfile::TempDir) -> PathBuf {
    let path = dir.path().join("timers.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":\"p/m\"}\n",
    )
    .unwrap();
    path
}

fn recorded_timers(path: &Path) -> Vec<lofi_types::TimerRecord> {
    let cursor = store::SessionCursor::open(path.to_path_buf()).unwrap();
    let snapshot = cursor.snapshot().unwrap();
    lofi_core::session::replay::timer_records_at(&cursor, &snapshot.index).unwrap()
}

fn timer_state_count(path: &Path) -> usize {
    store::SessionCursor::open(path.to_path_buf())
        .unwrap()
        .load_events()
        .unwrap()
        .iter()
        .filter(|event| matches!(event.kind, SessionEventKind::TimerState { .. }))
        .count()
}

#[test]
fn a_changed_timer_set_reaches_the_transcript_once() {
    use std::time::{Duration, SystemTime};
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut a = app_with_timers();
    a.session.cursor = Some(store::SessionCursor::new(path.clone(), None));
    a.timers.as_ref().unwrap().schedule_at(
        None,
        SystemTime::now() + Duration::from_mins(1),
        Some("check the build".into()),
    );
    a.record_timer_state();
    a.record_timer_state();
    assert_eq!(
        timer_state_count(&path),
        1,
        "an unchanged set records nothing new"
    );
    let records = recorded_timers(&path);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].message.as_deref(), Some("check the build"));
}

#[test]
fn a_repeating_timer_does_not_record_every_fire() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut a = app_with_timers();
    a.session.cursor = Some(store::SessionCursor::new(path.clone(), None));
    a.timers
        .as_ref()
        .unwrap()
        .restore(&[lofi_types::TimerRecord {
            id: 2,
            due_ms: 1,
            every_ms: Some(1_000),
            message: Some("tick".into()),
            owner: None,
        }]);
    a.record_timer_state();
    assert_eq!(
        a.take_timer_notices().len(),
        1,
        "the missed interval fires once"
    );
    a.record_timer_state();
    assert_eq!(
        timer_state_count(&path),
        1,
        "an advanced next fire alone is not worth a record"
    );
}

#[test]
fn the_jobs_tab_names_a_job_the_previous_run_left() {
    let mut a = app();
    a.stale_jobs = vec![stale_job(7, Some("npm run dev"))];
    a.open_background_modal(BackgroundTab::Jobs);
    let screen = screen(&mut a);
    assert!(screen.contains("stale"), "{screen}");
    assert!(screen.contains("npm run dev"), "{screen}");
    assert!(
        !screen.contains("no command recorded"),
        "the row names what it ran: {screen}"
    );
    assert!(!screen.contains("no jobs"), "{screen}");
}

#[test]
fn a_stale_row_without_a_recorded_command_says_so() {
    let mut a = app();
    a.stale_jobs = vec![stale_job(7, None)];
    a.open_background_modal(BackgroundTab::Jobs);
    let screen = screen(&mut a);
    assert!(screen.contains("(no command recorded)"), "{screen}");
}

#[test]
fn a_new_session_drops_the_stale_jobs() {
    let mut a = app_with_timers();
    a.stale_jobs = vec![stale_job(7, None)];
    a.reset_session_jobs();
    assert!(a.jobs_rows().is_empty());
}

#[test]
fn a_failed_timer_record_is_retried() {
    use std::time::{Duration, SystemTime};
    let dir = tempfile::tempdir().unwrap();
    let mut a = app_with_timers();
    // A path whose parent is missing refuses the append.
    a.session.cursor = Some(store::SessionCursor::new(
        dir.path().join("gone").join("session.jsonl"),
        None,
    ));
    a.timers.as_ref().unwrap().schedule_at(
        None,
        SystemTime::now() + Duration::from_mins(1),
        Some("wake".into()),
    );
    a.record_timer_state();
    assert!(
        a.timers_recorded.is_empty(),
        "an append that failed is not recorded"
    );
    let path = session_file(&dir);
    a.session.cursor = Some(store::SessionCursor::new(path.clone(), None));
    a.record_timer_state();
    assert_eq!(recorded_timers(&path).len(), 1);
}

#[test]
fn a_new_session_records_the_empty_set() {
    use std::time::{Duration, SystemTime};
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let mut a = app_with_timers();
    a.session.cursor = Some(store::SessionCursor::new(path.clone(), None));
    a.timers.as_ref().unwrap().schedule_at(
        None,
        SystemTime::now() + Duration::from_mins(1),
        Some("wake".into()),
    );
    a.record_timer_state();
    a.record_dropped_timers().unwrap();
    assert!(
        recorded_timers(&path).is_empty(),
        "the old transcript drops the set this session shed"
    );
    assert_eq!(timer_state_count(&path), 2);
}

#[test]
fn a_switch_adopts_the_recorded_work() {
    let dir = tempfile::tempdir().unwrap();
    let path = session_file(&dir);
    let cursor = store::SessionCursor::open(path).unwrap();
    let mut events = [
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::JobStarted {
                job_id: 9,
                cmd: Some("cargo watch".into()),
            },
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TimerState {
                timers: vec![lofi_types::TimerRecord {
                    id: 4,
                    due_ms: 1,
                    every_ms: None,
                    message: Some("wake".into()),
                    owner: None,
                }],
            },
        },
    ];
    cursor.append_events(&mut events).unwrap();

    let snapshot = cursor.snapshot().unwrap();
    let mut a = app_with_timers();
    // A timer the outgoing session still held must not survive the switch.
    a.timers.as_ref().unwrap().schedule_at(
        None,
        std::time::SystemTime::now() + std::time::Duration::from_mins(1),
        Some("outgoing".into()),
    );
    a.reset_session_jobs();
    let stale = a.adopt_recorded_work(&cursor, &snapshot.index).unwrap();
    assert_eq!(stale, vec![stale_job(9, Some("cargo watch"))]);
    assert_eq!(a.stale_jobs, stale);
    assert_eq!(
        a.timers.as_ref().unwrap().records().len(),
        1,
        "a switch replaces the outgoing set instead of merging with it"
    );
    assert_eq!(a.timers.as_ref().unwrap().listing()[0].id, 4);
    assert_eq!(a.timers_recorded.len(), 1);
}

#[test]
fn a_handover_warns_the_model_about_stale_job_ids() {
    let mut a = app();
    a.announce_stale_jobs(&[]);
    assert!(a.startup_notices.is_empty(), "no dead ids, no notice");
    a.announce_stale_jobs(&[stale_job(9, None), stale_job(12, None)]);
    assert_eq!(a.startup_notices.len(), 1);
    assert!(
        a.startup_notices[0].contains("jobs [9, c]"),
        "{:?}",
        a.startup_notices
    );
}

#[test]
fn a_new_session_reports_a_dropped_set_it_could_not_record() {
    use std::time::{Duration, SystemTime};
    let dir = tempfile::tempdir().unwrap();
    let mut a = app_with_timers();
    a.session.cursor = Some(store::SessionCursor::new(
        dir.path().join("gone").join("session.jsonl"),
        None,
    ));
    let timers = a.timers.as_ref().unwrap();
    timers.schedule_at(
        None,
        SystemTime::now() + Duration::from_mins(1),
        Some("wake".into()),
    );
    a.timers_recorded = timers.records();
    assert!(
        a.record_dropped_timers().is_err(),
        "an append that failed is reported"
    );
    // `/new` sheds the timers either way; the warning is the transcript's.
    a.reset_session_jobs();
    assert!(a.timers.as_ref().unwrap().records().is_empty());
    assert!(a.timers_recorded.is_empty());
}
