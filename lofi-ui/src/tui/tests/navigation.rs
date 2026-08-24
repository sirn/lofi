use super::*;

#[test]
fn selection_text_is_content_aware() {
    let mut a = app();
    a.log_off = 0;
    a.log_vis = vec![
        view::VisLine {
            rendered: "  hello world   ".to_string(),
            content: (2, 13),
            raw: None,
        },
        view::VisLine {
            rendered: "  │ │ lofi-core/src/agent.rs:233:pub struct Agent {     ".to_string(),
            content: (6, 51),
            raw: None,
        },
        view::VisLine {
            rendered: "      let x = 1;".to_string(),
            content: (2, 16),
            raw: None,
        },
    ];
    a.sel = Some(Selection {
        start: (0, 0),
        end: (2, 40),
    });
    assert_eq!(
        a.selection_text().as_deref(),
        Some("hello world\nlofi-core/src/agent.rs:233:pub struct Agent {\n    let x = 1;")
    );
}

#[test]
fn yank_line_returns_raw_markdown() {
    let mut a = app();
    a.log_off = 0;
    a.log_vis = vec![view::VisLine {
        rendered: "  bold".to_string(),
        content: (2, 6), // rendered "bold"
        raw: Some(view::RawLine::new(
            Arc::from("**bold**"),
            vec![0, 3, 4, 5, 8],
            true,
        )),
    }];
    a.nav_cursor = 0;
    assert_eq!(a.current_line_text().as_deref(), Some("**bold**"));
}

#[test]
fn yank_line_falls_back_to_rendered_without_raw() {
    let mut a = app();
    a.log_off = 0;
    a.log_vis = vec![view::VisLine {
        rendered: "  hello world   ".to_string(),
        content: (2, 13),
        raw: None,
    }];
    a.nav_cursor = 0;
    assert_eq!(a.current_line_text().as_deref(), Some("hello world"));
}

#[test]
fn selection_text_raw_partial_includes_markers() {
    let mut a = app();
    a.log_off = 0;
    a.log_vis = vec![view::VisLine {
        rendered: "  bold".to_string(),
        content: (2, 6),
        raw: Some(view::RawLine::new(
            Arc::from("**bold**"),
            vec![0, 3, 4, 5, 8],
            true,
        )),
    }];
    a.sel = Some(Selection {
        start: (0, 2),
        end: (0, 6),
    });
    assert_eq!(a.selection_text().as_deref(), Some("**bold**"));
}

#[test]
fn selection_text_raw_skips_softwrap_newlines() {
    let mut a = app();
    a.log_off = 0;
    let first: Arc<str> = Arc::from("hello world");
    a.log_vis = vec![
        view::VisLine {
            rendered: "  hello ".to_string(),
            content: (2, 8),
            raw: Some(view::RawLine::linear(first.clone(), 0, 6, true)),
        },
        view::VisLine {
            rendered: "  world".to_string(),
            content: (2, 7),
            raw: Some(view::RawLine::linear(first.clone(), 6, 5, false)),
        },
        view::VisLine {
            rendered: "  second line".to_string(),
            content: (2, 13),
            raw: Some(view::RawLine::linear(Arc::from("second line"), 0, 11, true)),
        },
    ];
    a.sel = Some(Selection {
        start: (0, 2),
        end: (2, 13),
    });
    assert_eq!(
        a.selection_text().as_deref(),
        Some("hello world\nsecond line")
    );
}

#[test]
fn selection_text_raw_char_level_on_continuation() {
    let mut a = app();
    a.log_off = 0;
    let first: Arc<str> = Arc::from("hello world");
    a.log_vis = vec![
        view::VisLine {
            rendered: "  hello ".to_string(),
            content: (2, 8),
            raw: Some(view::RawLine::linear(first.clone(), 0, 6, true)),
        },
        view::VisLine {
            rendered: "  world".to_string(),
            content: (2, 7),
            raw: Some(view::RawLine::linear(first.clone(), 6, 5, false)),
        },
    ];
    a.sel = Some(Selection {
        start: (1, 2),
        end: (1, 7),
    });
    assert_eq!(a.selection_text().as_deref(), Some("world"));
}

#[test]
fn ctrl_k_at_end_of_buffer_does_nothing() {
    // Regression: C-k at the end of a buffer with no trailing newline used
    // to slice one past the end and panic (observed as the app "quitting").
    let mut a = app();
    a.set_input("hello".to_string());
    a.move_line_end();
    a.kill_line_end(false);
    assert_eq!(a.input, "hello");
    assert!(a.kill_ring.is_empty());
}

#[test]
fn ctrl_k_kills_to_end_of_line_not_newline() {
    let mut a = app();
    a.set_input("hello world\nfoo".to_string());
    a.input_cursor = 0;
    a.kill_line_end(false);
    assert_eq!(a.input, "\nfoo");
    assert_eq!(a.kill_ring, "hello world");
}

#[test]
fn ctrl_k_kills_trailing_newline_on_empty_remainder() {
    let mut a = app();
    a.set_input("foo\nbar".to_string());
    a.input_cursor = 0;
    a.move_line_end();
    a.kill_line_end(false);
    assert_eq!(a.input, "foobar");
}

#[test]
fn ctrl_c_clears_nonempty_prompt() {
    let mut a = app();
    a.set_input("a draft".to_string());
    let mut run = None;
    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
    assert_eq!(a.input, "");
    assert!(!a.should_quit);
}

#[test]
fn ctrl_c_double_press_on_empty_quits() {
    let mut a = app();
    let mut run = None;
    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
    assert!(!a.should_quit, "first C-c must not quit");
    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
    assert!(a.should_quit, "second C-c must quit");
}

#[test]
fn ctrl_c_single_press_on_empty_does_not_quit() {
    let mut a = app();
    let mut run = None;
    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
    assert!(!a.should_quit);
}

#[test]
fn scroll_up_enters_nav_and_clamps_cursor_to_bottom_edge() {
    let mut a = app();
    a.mode = Mode::Input;
    a.log_total = 20;
    a.log_view_h = 5;
    a.last_base = 15; // base = total - height
    a.pinned = true; // sitting at the bottom
    a.nav_cursor = 19; // bottom line
    a.scroll_nav(-3);
    assert_eq!(a.mode, Mode::Navigate);
    assert_eq!(a.view_off(), 12);
    assert_eq!(a.nav_cursor, 16);
}

#[test]
fn scroll_down_clamps_cursor_to_top_edge() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 20;
    a.log_view_h = 5;
    a.last_base = 15;
    a.pinned = false;
    a.top_line = 10;
    a.nav_cursor = 10; // top of the viewport
    a.scroll_nav(3);
    assert_eq!(a.view_off(), 13);
    assert_eq!(a.nav_cursor, 13);
}

#[test]
fn scroll_keeps_cursor_when_still_visible() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 20;
    a.log_view_h = 5;
    a.last_base = 15;
    a.pinned = false;
    a.top_line = 10;
    a.nav_cursor = 12; // middle of [10, 14]
    a.scroll_nav(1);
    assert_eq!(a.view_off(), 11);
    assert_eq!(a.nav_cursor, 12); // still inside [11, 15]
}

#[test]
fn navigating_to_latest_line_pins_before_settle_reflows_log() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 20;
    a.log_view_h = 5;
    a.last_base = 15;
    a.log_off = 10;
    a.top_line = 10;
    a.nav_cursor = 14;
    a.pinned = false;

    // Reaching the final line means follow the tail. This must be recorded
    // before the next render because run settlement can change the last
    // turn's height in between.
    a.nav_bottom();
    assert!(a.pinned);
    assert_eq!(a.top_line, 15);

    a.last_base = 18;
    assert_eq!(a.view_off(), 18);
}

#[test]
fn scroll_at_boundary_leaves_mode_untouched() {
    let mut a = app();
    a.mode = Mode::Input;
    a.log_total = 20;
    a.log_view_h = 5;
    a.last_base = 15;
    a.pinned = true; // already at the bottom
    a.scroll_nav(3); // scrolling down does nothing
    assert_eq!(a.mode, Mode::Input);
}

#[test]
fn ctrl_c_on_empty_shows_quit_badge() {
    let mut a = app();
    let mut run = None;
    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
    assert_eq!(a.quit_badge(), Some("Press Ctrl-C again to quit"));
    a.set_input("draft".to_string());
    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);
    assert_eq!(a.quit_badge(), None);
}

#[test]
fn ctrl_d_deletes_char_or_quits_on_empty() {
    let mut a = app();
    a.set_input("ab".to_string());
    a.input_cursor = 0;
    let mut run = None;
    handle_event(&ctrl_key(KeyCode::Char('d')), &mut a, None, &mut run);
    assert_eq!(a.input, "b");
    assert!(!a.should_quit);
    let mut b = app();
    handle_event(&ctrl_key(KeyCode::Char('d')), &mut b, None, &mut run);
    assert!(b.should_quit);
}

#[tokio::test(flavor = "current_thread")]
async fn ctrl_d_on_empty_cancels_run_without_aborting() {
    let mut a = app();
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    });

    handle_event(&ctrl_key(KeyCode::Char('d')), &mut a, None, &mut run);

    assert!(a.should_quit);
    assert!(cancel.load(Ordering::Relaxed));
    assert!(run.is_some(), "quit must not abort the handle");
    run.take().unwrap().handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn settle_run_for_quit_waits_for_flush_then_joins() {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let started = cancel.clone();
    let handle = tokio::spawn(async move {
        while !started.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        drop(tx);
    });
    let run = RunHandle {
        handle,
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    };

    settle_run_for_quit(run, Duration::from_secs(1)).await;
    assert!(cancel.load(Ordering::Relaxed));
}

#[test]
fn tab_enters_navigate_and_esc_clears() {
    let mut a = app();
    let mut run = None;
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.mode, Mode::Navigate);
    let mut b = app();
    b.set_input("draft".to_string());
    handle_event(&plain_key(KeyCode::Esc), &mut b, None, &mut run);
    assert_eq!(b.mode, Mode::Input);
    assert_eq!(b.input, "");
}

#[tokio::test(flavor = "current_thread")]
async fn escape_interrupts_run_and_restores_queue_like_pi() {
    let mut a = app();
    a.prompt_queue = vec![
        QueuedPrompt {
            text: "steer first".to_string(),
            kind: lofi_types::PromptKind::User,
        },
        QueuedPrompt {
            text: "follow up".to_string(),
            kind: lofi_types::PromptKind::User,
        },
    ];
    a.set_input("draft".to_string());
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    });

    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);

    assert!(cancel.load(Ordering::Relaxed));
    assert!(a.prompt_queue.is_empty());
    assert_eq!(a.input, "steer first\n\nfollow up\n\ndraft");
    run.take().unwrap().handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn escape_dismisses_completion_before_interrupting_run() {
    let mut a = app();
    a.set_input("/".to_string());
    a.refresh_slash_complete();
    assert!(a.slash_complete.is_some());
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    });

    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);

    assert!(!cancel.load(Ordering::Relaxed));
    assert!(a.slash_complete.is_none());
    assert_eq!(a.input, "/");
    run.take().unwrap().handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn ctrl_c_in_nav_does_not_interrupt_run() {
    let mut a = app();
    a.enter_nav();
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    });

    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);

    assert!(!cancel.load(Ordering::Relaxed), "run must survive");
    assert_eq!(a.mode, Mode::Input);
    assert!(a.pinned);
    run.take().unwrap().handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn ctrl_c_clears_draft_without_interrupting_run() {
    let mut a = app();
    a.set_input("keep typing".to_string());
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    });

    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);

    assert!(!cancel.load(Ordering::Relaxed), "run must survive");
    assert_eq!(a.input, "");
    run.take().unwrap().handle.abort();
}

#[tokio::test(flavor = "current_thread")]
async fn ctrl_c_on_empty_input_interrupts_run() {
    let mut a = app();
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_shell: None,
    });

    handle_event(&ctrl_key(KeyCode::Char('c')), &mut a, None, &mut run);

    assert!(cancel.load(Ordering::Relaxed), "empty prompt + run: cancel");
    run.take().unwrap().handle.abort();
}

#[test]
fn navigate_jk_moves_cursor_and_clamps() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 10;
    a.log_view_h = 4;
    a.nav_cursor = 9;
    a.nav_move(-3);
    assert_eq!(a.nav_cursor, 6);
    a.nav_move(-100);
    assert_eq!(a.nav_cursor, 0);
    a.nav_move(100);
    assert_eq!(a.nav_cursor, 9);
}

#[test]
fn select_sel_is_charwise_inclusive() {
    let mut a = app();
    a.mode = Mode::Select;
    a.select_anchor = (2, 3);
    a.nav_cursor = 5;
    a.nav_col = 6;
    let sel = a.select_sel();
    // Max end is exclusive (+1) so the cursor char is included.
    assert_eq!(sel.start, (2, 3));
    assert_eq!(sel.end, (5, 7));
    a.nav_cursor = 1;
    a.nav_col = 1;
    let sel = a.select_sel();
    assert_eq!(sel.start, (1, 1));
    assert_eq!(sel.end, (2, 4));
}

#[test]
fn v_enters_select_and_extends_selection() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 10;
    a.log_view_h = 4;
    a.nav_cursor = 3;
    let mut run = None;
    handle_event(&plain_key(KeyCode::Char('v')), &mut a, None, &mut run);
    assert_eq!(a.mode, Mode::Select);
    assert_eq!(a.sel.as_ref().unwrap().start, (3, 0));
    assert_eq!(a.sel.as_ref().unwrap().end, (3, 1));
    handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 4);
    assert_eq!(a.sel.as_ref().unwrap().start, (3, 0));
    assert_eq!(a.sel.as_ref().unwrap().end, (4, 1));
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.mode, Mode::Navigate);
    assert!(a.sel.is_none());
}

#[test]
fn esc_discards_select_back_to_nav() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 10;
    a.log_view_h = 4;
    a.nav_cursor = 3;
    let mut run = None;
    handle_event(&plain_key(KeyCode::Char('v')), &mut a, None, &mut run);
    assert_eq!(a.mode, Mode::Select);
    handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    assert!(a.sel.is_some());
    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
    assert_eq!(a.mode, Mode::Navigate);
    assert!(a.sel.is_none());
}

#[test]
fn turn_start_line_accounts_for_blanks() {
    let mut a = app();
    a.frozen_heights = vec![3, 2];
    a.last_turn_height = 4;
    a.turns.clear();
    for _ in 0..3 {
        push_turn(&mut a);
    }
    assert_eq!(a.turn_start_line(0), 0);
    assert_eq!(a.turn_start_line(1), 4);
    assert_eq!(a.turn_start_line(2), 7);
}

#[test]
fn bracket_jumps_between_turns() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.frozen_heights = vec![3, 2];
    a.last_turn_height = 4;
    a.log_total = 3 + 1 + 2 + 1 + 4;
    a.log_view_h = 10;
    a.turns.clear();
    for _ in 0..3 {
        push_turn(&mut a);
    }
    let mut run = None;
    a.nav_cursor = 1;
    handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 4);
    handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 7);
    handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 7);
    handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 4);
    handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 0);
    handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 0);
}

#[test]
fn page_keys_move_cursor_in_nav() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.log_total = 40;
    a.log_view_h = 10;
    a.nav_cursor = 20;
    let mut run = None;
    handle_event(&plain_key(KeyCode::PageDown), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 30);
    handle_event(&plain_key(KeyCode::PageUp), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 20);
}

#[test]
fn page_down_in_input_pins_at_bottom() {
    let mut a = app();
    a.mode = Mode::Input;
    a.log_total = 40;
    a.log_view_h = 10;
    a.last_base = 30;
    a.top_line = 0;
    let mut run = None;
    handle_event(&plain_key(KeyCode::PageDown), &mut a, None, &mut run);
    assert_eq!(a.top_line, 10);
    assert!(!a.pinned);
    for _ in 0..3 {
        handle_event(&plain_key(KeyCode::PageDown), &mut a, None, &mut run);
    }
    assert!(a.pinned);
    handle_event(&plain_key(KeyCode::PageUp), &mut a, None, &mut run);
    assert!(!a.pinned);
}

#[test]
fn ctrl_c_in_nav_returns_to_input_pinned() {
    let mut a = app();
    a.mode = Mode::Navigate;
    a.last_base = 42;
    a.top_line = 5;
    a.pinned = false;
    let mut run = None;
    handle_event(
        &Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        )),
        &mut a,
        None,
        &mut run,
    );
    assert_eq!(a.mode, Mode::Input);
    assert!(a.pinned);
    assert_eq!(a.top_line, 42);
}

#[test]
fn detail_keys_toggle_and_scroll_only_the_selected_row() {
    let mut a = app();
    push_turn(&mut a);
    a.mode = Mode::Navigate;
    a.log_off = 0;
    a.log_total = 1;
    a.log_view_h = 1;
    a.nav_cursor = 0;
    let key = DetailKey::NativeTool {
        parent: 0,
        id: 7,
    };
    a.log_details = vec![Some(view::DetailTarget {
        key: key.clone(),
        total: 12,
        tail: false,
    })];
    let mut run = None;

    handle_event(&plain_key(KeyCode::Right), &mut a, None, &mut run);
    assert!(a.expanded_details.contains_key(&key));
    handle_event(
        &Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Down,
            KeyModifiers::SHIFT,
            KeyEventKind::Press,
        )),
        &mut a,
        None,
        &mut run,
    );
    assert_eq!(a.expanded_details[&key].scroll, Some(1));
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);
    assert!(!a.expanded_details.contains_key(&key));
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);
    assert!(a.expanded_details.contains_key(&key));
    handle_event(&plain_key(KeyCode::Left), &mut a, None, &mut run);
    assert!(!a.expanded_details.contains_key(&key));
}

#[test]
fn mouse_wheel_scrolls_the_inline_detail_under_the_pointer() {
    let mut a = app();
    push_turn(&mut a);
    a.mode = Mode::Navigate;
    a.log_rect = ratatui::layout::Rect::new(0, 0, 40, 5);
    a.log_off = 0;
    a.log_total = 1;
    a.log_view_h = 5;
    let key = DetailKey::NativeTool {
        parent: 0,
        id: 8,
    };
    a.log_details = vec![Some(view::DetailTarget {
        key: key.clone(),
        total: 12,
        tail: false,
    })];
    a.expanded_details.insert(
        key.clone(),
        DetailState {
            turn: 0,
            scroll: None,
        },
    );

    handle_mouse(
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
        &mut a,
    );

    assert_eq!(a.expanded_details[&key].scroll, Some(1));
}
