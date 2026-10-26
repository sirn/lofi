use super::*;

#[test]
fn multi_line_input_editing() {
    let mut a = app();
    a.insert_char('a');
    a.insert_newline();
    a.insert_char('b');
    assert_eq!(a.input, "a\nb");
    assert_eq!(a.cursor_row_col(), (1, 1));
    a.move_up();
    assert_eq!(a.cursor_row_col().0, 0);
    a.move_down();
    assert_eq!(a.cursor_row_col().0, 1);
}

#[test]
fn input_lines_counts_logical_lines() {
    let mut a = app();
    assert_eq!(a.input_lines(120), 1);
    a.insert_newline();
    a.insert_newline();
    assert_eq!(a.input_lines(120), 3);
}

#[test]
fn input_wraps_at_word_boundaries() {
    let mut a = app();
    a.set_input("hello world foo".to_string());
    // Word-wrap at width 7: each row ends after the space that precedes a
    // word too long to fit (a trailing blank cell), so the next word starts
    // the following row. Character wrapping would have split "hello w".
    let rows = a.input_select_rows(7);
    assert_eq!(rows, vec!["hello ", "world ", "foo"]);
}

#[test]
fn input_hard_breaks_unbreakable_token() {
    let mut a = app();
    a.set_input("supercalifragilistic".to_string());
    let rows = a.input_select_rows(7);
    assert_eq!(rows, vec!["superca", "lifragi", "listic"]);
}

#[test]
fn input_cursor_tracks_word_wrap_past_cursor() {
    let mut a = app();
    a.set_input("hello world".to_string());
    // "hello world" at width 7 wraps to ["hello ", "world"]: the word
    // "world" doesn't fit after "hello ", so 'w' starts row 1. The cursor
    // after the space (col 6) sits at the end of row 0; after 'w' (col 7)
    // it is on row 1. A prefix-only wrap would misplace the cursor on row 0
    // because it cannot see that "world" wraps.
    a.input_cursor = "hello ".len();
    assert_eq!(a.input_cursor_pos(7), (0, 6));
    a.input_cursor = "hello w".len();
    assert_eq!(a.input_cursor_pos(7), (1, 1));
    a.input_cursor = "hello world".len();
    assert_eq!(a.input_cursor_pos(7), (1, 5));
}

#[test]
fn cursor_up_recalls_at_first_cell() {
    let mut a = app();
    a.history_nav.push("first".to_string());
    a.history_nav.push("second".to_string());
    a.set_input("Hello".to_string());
    a.cursor_up();
    assert_eq!(a.input, "Hello");
    assert_eq!(a.input_cursor, 0);
    a.cursor_up();
    assert_eq!(a.input, "second");
    assert_eq!(a.input_cursor, 0);
    a.cursor_up();
    assert_eq!(a.input, "first");
    assert_eq!(a.input_cursor, 0);
    a.cursor_up();
    assert_eq!(a.input, "first");
}

#[test]
fn cursor_down_recalls_at_last_cell() {
    let mut a = app();
    a.history_nav.push("first".to_string());
    a.history_nav.push("second".to_string());
    a.set_input("Hello".to_string());
    a.cursor_up();
    a.cursor_up();
    a.cursor_up();
    a.cursor_up();
    assert_eq!(a.input, "first");
    a.cursor_down();
    assert_eq!(a.input, "first");
    assert_eq!(a.input_cursor, "first".len());
    a.cursor_down();
    assert_eq!(a.input, "second");
    assert_eq!(a.input_cursor, "second".len());
    a.cursor_down();
    assert_eq!(a.input, "Hello");
    assert_eq!(a.input_cursor, "Hello".len());
}

#[test]
fn cursor_up_down_navigate_multiline() {
    let mut a = app();
    a.set_input("line1\nline2\nline3".to_string());
    a.cursor_up();
    assert_eq!(a.cursor_row_col().0, 1);
    a.cursor_up();
    assert_eq!(a.cursor_row_col().0, 0);
    a.cursor_up();
    assert_eq!(a.cursor_row_col(), (0, 0));
    a.cursor_down();
    assert_eq!(a.cursor_row_col().0, 1);
    a.cursor_down();
    assert_eq!(a.cursor_row_col().0, 2);
}

#[test]
fn history_recall_round_trip() {
    let mut a = app();
    a.set_input("first".to_string());
    a.history_nav.push("first".to_string());
    a.set_input("second".to_string());
    a.history_nav.push("second".to_string());
    a.clear_input();
    a.recall_prev();
    assert_eq!(a.input, "second");
    a.recall_prev();
    assert_eq!(a.input, "first");
    a.recall_next();
    assert_eq!(a.input, "second");
    a.recall_next();
    assert_eq!(a.input, "");
}

#[test]
fn slash_clear_help_session_resume_new() {
    let mut a = app();
    push_turn(&mut a);
    assert!(a.slash_command("/clear"));
    assert!(a.transcript.turns.is_empty());

    a.slash_command("/help");
    assert!(a.transcript.turns.is_empty());
    assert!(a.info.is_some());
    a.info = None;

    // Unknown commands notify on the rule line instead of pushing a turn.
    assert!(a.slash_command("/nope"));
    assert!(a.transcript.turns.is_empty());
    let (msg, kind) = a.notify_badge().expect("unknown command notified");
    assert_eq!(kind, NotifyKind::Error);
    assert!(msg.contains("unknown command"));

    assert!(a.slash_command("/resume"));
    assert!(a.picker.is_none());
    let (msg, kind) = a.notify_badge().expect("/resume notified");
    assert_eq!(kind, NotifyKind::Warn);
    assert!(msg.contains("disabled"));

    assert!(a.slash_command("/new"));
    assert!(a.transcript.turns.is_empty());
    assert!(a.session.cursor.is_none());
    assert!(a.status_usage.is_none());
    assert_eq!(a.cost, 0.0);
    assert_eq!(a.turn_cost, 0.0);
    assert!(!a.turn_has_round_usage);

    assert!(a.slash_command("/quit"));
    assert!(a.should_quit);
}

#[test]
fn session_info_opens_modal() {
    let mut a = app();
    assert!(a.slash_command("/session"));
    assert!(a.transcript.turns.is_empty());
    let info = a.info.as_ref().expect("modal opened");
    assert_eq!(info.title, "Session");
    let body = info
        .lines
        .iter()
        .map(|line| match line {
            InfoLine::Section(label) => label.clone(),
            InfoLine::Text(line) => line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
        })
        .collect::<String>();
    assert!(body.contains("(none)"));
    assert!(body.contains("No session file"));
}

#[test]
fn session_info_sections_use_subtle_horizontal_rules() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut a = app();
    assert!(a.slash_command("/session"));
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    let buffer = term.backend().buffer();
    let row = |y| {
        (0..buffer.area.width)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let section_y = (0..buffer.area.height)
        .find(|&y| row(y).contains("Session ─"))
        .expect("session section row");
    let label = (0..buffer.area.width)
        .map(|x| &buffer[(x, section_y)])
        .find(|cell| cell.symbol() == "S" && cell.modifier.contains(Modifier::BOLD))
        .expect("bold section label");
    let rule = (0..buffer.area.width)
        .map(|x| &buffer[(x, section_y)])
        .find(|cell| cell.symbol() == "─")
        .expect("section rule");

    assert_eq!(label.fg, a.theme.fg);
    assert_eq!(rule.fg, a.theme.subtle);
}

#[test]
fn session_info_modal_shows_id_when_path_set() {
    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::new(
        std::path::PathBuf::from("/tmp/sessions/abc123.jsonl"),
        None,
    ));
    assert!(a.slash_command("/session"));
    let info = a.info.as_ref().expect("modal opened");
    let body = info
        .lines
        .iter()
        .map(|line| match line {
            InfoLine::Section(label) => label.clone(),
            InfoLine::Text(line) => line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
        })
        .collect::<String>();
    assert!(
        body.contains("abc123"),
        "body should contain the session id: {body}"
    );
    assert!(
        body.contains("Workspace"),
        "body should have a workspace section: {body}"
    );
    assert!(
        body.contains("Model"),
        "body should have a model section: {body}"
    );
}

#[test]
fn paste_blocked_while_modal_open() {
    let mut a = app();
    let mut run = None;
    a.slash_command("/help");
    assert!(a.modal_open());
    handle_event(&Event::Paste("pasted".to_string()), &mut a, None, &mut run);
    assert_eq!(a.input, "");
    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
    handle_event(&Event::Paste("pasted".to_string()), &mut a, None, &mut run);
    assert_eq!(a.input, "pasted");
}

#[test]
fn the_debounce_clock_starts_with_the_prompt_and_clears_with_it() {
    // The terminal report counts from here, so a prompt the user answers at
    // once never reports `blocked`.
    let mut a = app();
    let (req, _response) = confirm_request("touch queued.txt");
    a.queue_confirmation(req);
    a.refresh_confirmations();
    assert!(a.confirm_waiting.is_some());

    a.pending_confirms.clear();
    a.refresh_confirmations();
    assert!(a.confirm_waiting.is_none());
}

#[test]
fn auto_evaluation_is_deferred_until_the_ui_grace_expires() {
    let mut a = app();
    let (mut req, _response) = confirm_request("rm generated.txt");
    req.reason = std::sync::Arc::new(std::sync::Mutex::new(
        lofi_core::ConfirmReason::AutoEvaluating {
            started_at: std::time::Instant::now(),
        },
    ));
    let reason = req.reason.clone();

    a.queue_confirmation(req);
    assert!(a.pending_confirms.is_empty());
    assert_eq!(a.deferred_confirms.len(), 1);
    assert!(!a.modal_open());

    *reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = lofi_core::ConfirmReason::AutoAsk {
        reason: "command deletes a file".to_string(),
    };
    assert!(a.refresh_confirmations());
    assert_eq!(a.pending_confirms.len(), 1);
    assert!(a.deferred_confirms.is_empty());
    assert!(a.modal_open());
}

#[test]
fn elapsed_auto_evaluation_is_shown_by_the_ui() {
    let mut a = app();
    let (mut req, _response) = confirm_request("rm generated.txt");
    req.reason = std::sync::Arc::new(std::sync::Mutex::new(
        lofi_core::ConfirmReason::AutoEvaluating {
            started_at: std::time::Instant::now()
                .checked_sub(AUTO_MODE_UI_GRACE)
                .unwrap(),
        },
    ));

    a.queue_confirmation(req);

    assert_eq!(a.pending_confirms.len(), 1);
    assert!(a.deferred_confirms.is_empty());
}

#[test]
fn topmost_permission_dialog_handles_input_before_tree_picker() {
    let mut a = app();
    let mut run = None;
    a.tree_picker = Some(TreePickerState {
        entries: vec![TreeEntry {
            branch_point: "root".into(),
            label: "user: earlier prompt".into(),
            prefix: "- ".into(),
            prefill: String::new(),
            is_active: true,
            source_index: 0,
            source_offset: 0,
            source_kind: store::IndexKind::UserPrompt,
            hydrated: true,
        }],
        selected: 0,
        generation: 0,
        loading: false,
    });
    let (req, mut response) = confirm_request("rm generated.txt");
    a.pending_confirms.push(req);

    handle_event(&plain_key(KeyCode::Char('d')), &mut a, None, &mut run);

    assert_eq!(response.try_recv(), Ok(false));
    assert!(a.pending_confirms.is_empty());
    assert!(a.tree_picker.is_some());

    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
    assert!(a.tree_picker.is_none());
}

#[test]
fn permission_dialog_requires_an_explicit_choice() {
    let mut a = app();
    let (req, mut response) = confirm_request("rm -rf build");
    a.pending_confirms.push(req);

    assert!(a.handle_confirm_key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE,)));
    assert_eq!(a.pending_confirms.len(), 1);
    assert!(matches!(
        response.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));

    a.handle_confirm_key(&KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    assert_eq!(a.confirm_selected, 1);
    a.handle_confirm_key(&KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    assert_eq!(a.confirm_selected, 0);
    a.handle_confirm_key(&KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE));
    assert_eq!(a.confirm_selected, 1);
    a.handle_confirm_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(a.pending_confirms.is_empty());
    assert_eq!(response.try_recv(), Ok(false));
}

#[test]
fn permission_dialog_action_keys_resolve_directly() {
    for (key, expected) in [('a', true), ('y', true), ('d', false), ('n', false)] {
        let mut a = app();
        let (req, mut response) = confirm_request("dangerous command");
        a.pending_confirms.push(req);
        a.handle_confirm_key(&KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE));
        assert_eq!(response.try_recv(), Ok(expected), "key {key}");
    }
}

#[test]
fn permission_dialog_renders_command_and_selectable_actions() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    let (req, _response) = confirm_request("sudo systemctl restart lofi");
    a.pending_confirms.push(req);
    let mut term = Terminal::new(TestBackend::new(90, 28)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let screen = (0..28)
        .map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(screen.contains("Permission Required"), "{screen}");
    assert!(screen.contains("sudo systemctl restart lofi"), "{screen}");
    assert!(
        screen.contains("Allow") && screen.contains("Deny"),
        "{screen}"
    );
    let command_cell = (0..28)
        .flat_map(|y| (0..90).map(move |x| (x, y)))
        .find(|&(x, y)| buf[(x, y)].symbol() == "s" && buf[(x, y)].bg == a.theme.panel_bg)
        .expect("policy command should use a distinct panel background");
    assert!(command_cell.0 > 0);
    let allow = (0..28)
        .flat_map(|y| (0..90).map(move |x| (x, y)))
        .find(|&(x, y)| buf[(x, y)].symbol() == "A" && buf[(x, y)].bg == a.theme.primary)
        .expect("selected Allow button should use primary background");
    assert!(
        allow.0 > 45,
        "actions should sit toward the right: {allow:?}"
    );
}

#[test]
fn prompt_panel_uses_full_height_user_rail() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut a = app();
    a.input = "hello".to_string();
    a.input_cursor = a.input.len();
    let mut term = Terminal::new(TestBackend::new(60, 20)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let panel_top = 16;
    for y in panel_top..20 {
        assert_eq!(buf[(0, y)].symbol(), "▌", "panel row {y}");
        assert_eq!(buf[(0, y)].fg, a.theme.user, "focused rail row {y}");
    }

    a.mode = Mode::Navigate;
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    for y in panel_top..20 {
        assert_eq!(buf[(0, y)].symbol(), "▌", "panel row {y}");
        assert_eq!(buf[(0, y)].fg, a.theme.subtle, "unfocused rail row {y}");
    }
}

#[test]
fn scrollbars_use_a_gutter_outside_transcript_and_prompt_text() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(
        (0..30)
            .map(|_| "a transcript line that reaches the right edge")
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    a.input = (0..12).map(|_| "prompt row").collect::<Vec<_>>().join("\n");
    a.input_cursor = a.input.len();

    let mut term = Terminal::new(TestBackend::new(60, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();

    assert_eq!(a.log_rect.right(), 59);
    assert_eq!(a.input_rect.right(), 59);
    let log_gutter_x = a.log_rect.right();
    let input_gutter_x = a.input_rect.right();
    assert!(
        (a.log_rect.y..a.log_rect.bottom())
            .any(|y| matches!(buf[(log_gutter_x, y)].symbol(), "┃" | "│")),
        "transcript scrollbar should be in its own gutter"
    );
    assert!(
        (a.input_rect.y..a.input_rect.bottom())
            .any(|y| matches!(buf[(input_gutter_x, y)].symbol(), "┃" | "│")),
        "prompt scrollbar should be in its own gutter"
    );
}

#[test]
fn autocomplete_uses_primary_focus_default_background_and_no_header() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut a = app();
    a.input = "/".to_string();
    a.input_cursor = 1;
    a.refresh_slash_complete();
    let mut term = Terminal::new(TestBackend::new(90, 28)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();

    let (item_x, item_y) = (0..28)
        .flat_map(|y| (0..90).map(move |x| (x, y)))
        .find(|&(x, y)| buf[(x, y)].symbol() == "/" && buf[(x, y)].bg == a.theme.primary)
        .expect("selected autocomplete row should use primary background");
    let border_x = (0..item_x)
        .rev()
        .find(|&x| buf[(x, item_y)].symbol() == "│")
        .expect("autocomplete left border");
    assert_eq!(
        item_x - border_x,
        2,
        "one padding cell should separate the border and item"
    );
    assert_eq!(
        buf[(border_x + 1, item_y)].bg,
        ratatui::style::Color::Reset,
        "popover padding should keep the terminal's default background"
    );
    let right_border_x = (item_x + 1..90)
        .find(|&x| buf[(x, item_y)].symbol() == "│" && buf[(x, item_y)].fg == a.theme.primary)
        .expect("autocomplete right border");
    assert!(
        matches!(buf[(right_border_x - 1, item_y)].symbol(), "┃" | "│"),
        "scrollbar should share the right padding gutter next to the border"
    );
    let screen = (0..28)
        .map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !screen.contains("Commands"),
        "autocomplete has no header: {screen}"
    );
    assert!(
        screen.contains("navigate") && screen.contains("complete"),
        "{screen}"
    );
}

#[test]
fn tree_picker_is_centered() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    a.tree_picker = Some(TreePickerState {
        entries: vec![
            TreeEntry {
                branch_point: "x".into(),
                label: "agent: hi".into(),
                prefix: "`- ".into(),
                prefill: String::new(),
                is_active: true,
                source_index: 0,
                source_offset: 0,
                source_kind: store::IndexKind::TurnEnd,
                hydrated: true,
            },
            TreeEntry {
                branch_point: "y".into(),
                label: "user: yo".into(),
                prefix: "|- ".into(),
                prefill: String::new(),
                is_active: false,
                source_index: 1,
                source_offset: 0,
                source_kind: store::IndexKind::UserPrompt,
                hydrated: true,
            },
        ],
        selected: 0,
        generation: 0,
        loading: false,
    });
    let backend = TestBackend::new(80, 22);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let row = |y: u16| {
        (0..80)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect::<String>()
    };
    let title_y = (0..22)
        .find(|&y| row(y).contains("Roll back to a turn"))
        .expect("tree modal title found");
    let top_y = title_y.saturating_sub(1);

    // The border stays uninterrupted; the title gets its own first interior
    // row rather than being docked into the top border.
    assert!(row(top_y).contains('╭') && row(top_y).contains('╮'));
    assert!(!row(top_y).contains("Roll back"));
    assert!(row(title_y).contains('│'));

    let help_y = (title_y + 1..22)
        .find(|&y| row(y).contains("navigate") && row(y).contains("restore"))
        .expect("tree modal bottom hint found");
    assert!(row(help_y + 1).contains('╰') && row(help_y + 1).contains('╯'));

    // The picker remains centered rather than bottom-anchored.
    assert!(
        top_y < 15,
        "tree modal should be centered, got top_y={top_y}"
    );
}

#[test]
fn help_modal_scrolls_and_dismisses() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    let mut run = None;
    assert!(a.slash_command("/help"));
    assert!(a.transcript.turns.is_empty());
    // Render once so the modal publishes its scroll geometry
    // (total/view_h) for the key handler.
    let backend = TestBackend::new(64, 18);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let total = a.info.as_ref().unwrap().total;
    let view_h = a.info.as_ref().unwrap().view_h;
    assert!(total > view_h, "help should overflow the viewport");
    handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    assert_eq!(a.info.as_ref().unwrap().scroll, 1);
    for _ in 0..5 {
        handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    }
    assert_eq!(a.info.as_ref().unwrap().scroll, 6);
    handle_event(&plain_key(KeyCode::Char('k')), &mut a, None, &mut run);
    assert_eq!(a.info.as_ref().unwrap().scroll, 5);
    for _ in 0..total {
        handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    }
    assert_eq!(a.info.as_ref().unwrap().scroll, total - view_h);
    handle_event(&plain_key(KeyCode::Char('q')), &mut a, None, &mut run);
    assert!(a.info.is_none());
}

#[test]
fn pasted_image_path_inserts_literal_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.png");
    std::fs::write(&path, b"image bytes").unwrap();
    let pasted = path.display().to_string();
    let mut a = app();

    handle_event(&Event::Paste(pasted.clone()), &mut a, None, &mut None);

    assert_eq!(a.input, pasted);
}

#[test]
fn pasted_multiline_text_inserts_literal_text() {
    let mut a = app();

    handle_event(
        &Event::Paste("line one\nline two".to_string()),
        &mut a,
        None,
        &mut None,
    );

    assert_eq!(a.input, "line one\nline two");
}

#[test]
fn slash_complete_filters_and_accepts() {
    let mut a = app();
    a.input = "/".to_string();
    a.refresh_slash_complete();
    let sc = a.slash_complete.as_ref().expect("popover open");
    assert_eq!(sc.candidates.len(), SLASH_COMMANDS.len());
    a.input = "/tr".to_string();
    a.refresh_slash_complete();
    let sc = a.slash_complete.as_ref().expect("popover open");
    // `/tr` matches only /tree; resolve its index rather than hardcoding it so
    // adding a command does not shift the assertion.
    let tree_idx = SLASH_COMMANDS
        .iter()
        .position(|(c, _)| *c == "/tree")
        .unwrap();
    assert_eq!(sc.candidates, vec![tree_idx]);
    a.input = "/tree".to_string();
    a.refresh_slash_complete();
    assert!(a.slash_complete.is_none());
    a.input = "hello".to_string();
    a.refresh_slash_complete();
    assert!(a.slash_complete.is_none());
    a.input = "/".to_string();
    a.refresh_slash_complete();
    a.slash_complete_down(); // index 1 = /compact
    a.slash_complete_down(); // index 2 = /debug
    a.slash_complete_down(); // index 3 = /exit
    a.slash_complete_down(); // index 4 = /help
    a.slash_complete_accept();
    assert_eq!(a.input, "/help");
    assert_eq!(a.input_cursor, a.input.len());
    assert!(a.slash_complete.is_none());
}

#[test]
fn slash_complete_enter_accepts_tab_wraps() {
    let mut a = app();
    let mut run = None;
    a.input = "/".to_string();
    a.refresh_slash_complete();
    let len = a.slash_complete.as_ref().unwrap().candidates.len();
    for _ in 0..len {
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    }
    assert_eq!(a.slash_complete.as_ref().unwrap().selected, 0);
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);
    assert_eq!(a.input, "/clear");
    assert!(a.slash_complete.is_none());
}

#[test]
fn slash_complete_single_item_tab_accepts() {
    // "/tr" matches only /tree, so Tab/Shift+Tab accept it outright
    // instead of cycling (a no-op on a single candidate).
    let mut a = app();
    let mut run = None;
    a.input = "/tr".to_string();
    a.refresh_slash_complete();
    assert_eq!(a.slash_complete.as_ref().unwrap().candidates.len(), 1);
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.input, "/tree");
    assert!(a.slash_complete.is_none());
    a.input = "/tr".to_string();
    a.refresh_slash_complete();
    handle_event(&plain_key(KeyCode::BackTab), &mut a, None, &mut run);
    assert_eq!(a.input, "/tree");
    assert!(a.slash_complete.is_none());
}

#[test]
fn subagent_command_opens_empty_modal_without_a_session() {
    let mut a = app();
    assert!(a.slash_command("/subagent"));
    // The test app has no session sink: the modal opens with live registry
    // rows only, and there are none.
    let modal = a.subagents_modal.as_ref().expect("subagents modal");
    assert!(modal.rows.is_empty());
}

#[test]
fn subagent_modal_esc_closes() {
    let mut a = app();
    assert!(a.slash_command("/subagent"));
    assert!(a.subagents_modal.is_some());
    let mut run = None;
    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
    assert!(a.subagents_modal.is_none());
}

#[test]
fn subagent_transcript_view_builds_main_transcript_turns() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let mut events = blank_events([
        msg(user("inspect the repo")),
        msg(Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "looking".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                },
            ],
            kind: PromptKind::default(),
        }),
        msg(Message {
            origin: None,
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "file body".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "openai/gpt-4o".into(),
            elapsed_ms: 1000,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    ]);
    sub.append_events(&mut events).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    assert!(a.slash_command("/subagent"));
    let mut run = None;
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);

    let modal = a.subagents_modal.as_ref().expect("subagents modal");
    let view = modal.viewing.as_ref().expect("transcript view");
    assert!(view.load_error.is_none());
    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.turns[0].prompt, "inspect the repo");
    assert!(
        view.turns[0]
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Tool(tool) if tool.done)),
        "expected a settled tool block, got {:?}",
        view.turns[0].blocks
    );
}

#[test]
fn subagent_transcript_view_esc_returns_to_list() {
    let mut a = app();
    assert!(a.slash_command("/subagent"));
    a.subagents_modal.as_mut().unwrap().viewing = Some(SubagentTranscriptView {
        id: "run-1".to_string(),
        file: None,
        path: None,
        turns: Vec::new(),
        load_error: Some("transcript not found".to_string()),
        live: false,
        reloaded: false,
        heights: Vec::new(),
        signatures: Vec::new(),
        prior_heights: Vec::new(),
        parsed_len: None,
        width: 0,
        total: 0,
        scroll: 0,
        last_reload: std::time::Instant::now(),
        view_h: 10,
    });
    let mut run = None;
    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
    let modal = a.subagents_modal.as_ref().expect("subagents modal");
    assert!(modal.viewing.is_none());
}

#[test]
fn subagent_transcript_view_keeps_turns_across_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let turn_end = || SessionEventKind::TurnEnd {
        model: "openai/gpt-4o".into(),
        elapsed_ms: 1000,
        cost: 0.0,
        usage: Usage::default(),
        stop_reason: None,
    };
    let mut first = blank_events([
        msg(user("first prompt")),
        msg(assistant("first answer")),
        turn_end(),
    ]);
    test_append_events(sub.path(), &mut first, None).unwrap();
    test_append_compaction(
        sub.path(),
        &[assistant("kept tail")],
        None,
        "summary",
        &[first[0].id.clone(), first[2].id.clone()],
        store::CompactionCounts {
            summarized: 3,
            represented: 3,
            kept: 1,
        },
    )
    .unwrap();
    let mut second = blank_events([
        msg(user("second prompt")),
        msg(assistant("second answer")),
        turn_end(),
    ]);
    test_append_events(sub.path(), &mut second, None).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    assert!(a.slash_command("/subagent"));
    let mut run = None;
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);

    let modal = a.subagents_modal.as_ref().expect("subagents modal");
    let view = modal.viewing.as_ref().expect("transcript view");
    let prompts: Vec<&str> = view.turns.iter().map(|turn| turn.prompt.as_str()).collect();
    // The compaction marker's parent is a hidden checkpoint copy; a replay
    // that walked parent links would stop there and drop the first turn.
    assert_eq!(prompts, ["first prompt", "second prompt"]);
}

fn subagent_screen(term: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
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

/// Open the drill-in for the only subagent row and scroll up ten rows.
/// The first render measures the transcript; only then can the key handler
/// clamp the scroll against the real total.
fn open_scrolled_subagent_view(
    a: &mut App,
    term: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
) {
    assert!(a.slash_command("/subagent"));
    let mut run = None;
    handle_event(&plain_key(KeyCode::Enter), a, None, &mut run);
    term.draw(|frame| crate::tui::view::render(frame, a))
        .unwrap();
    for _ in 0..10 {
        handle_event(&plain_key(KeyCode::Up), a, None, &mut run);
    }
    term.draw(|frame| crate::tui::view::render(frame, a))
        .unwrap();
}

#[test]
fn subagent_transcript_view_renders_bottom_anchored_and_scrolls_to_top() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let body = (0..40)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut events = blank_events([
        msg(user("inspect the repo")),
        msg(assistant(&body)),
        SessionEventKind::TurnEnd {
            model: "openai/gpt-4o".into(),
            elapsed_ms: 1000,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    ]);
    sub.append_events(&mut events).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    assert!(a.slash_command("/subagent"));
    let mut run = None;
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    let bottom = subagent_screen(&term);
    assert!(bottom.contains("line 39"), "bottom frame:\n{bottom}");
    assert!(!bottom.contains("line 00"), "bottom frame:\n{bottom}");

    handle_event(&plain_key(KeyCode::Char('g')), &mut a, None, &mut run);
    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    let top = subagent_screen(&term);
    assert!(top.contains("inspect the repo"), "top frame:\n{top}");
}

#[test]
fn refresh_subagent_view_skips_reload_when_the_run_is_not_live() {
    let mut a = app();
    assert!(a.slash_command("/subagent"));
    a.subagents_modal.as_mut().unwrap().viewing = Some(SubagentTranscriptView {
        id: "run-1".to_string(),
        file: None,
        path: None,
        turns: vec![Turn {
            kind: lofi_types::PromptKind::User,
            prompt: "p".to_string(),
            blocks: Vec::new(),
        }],
        load_error: None,
        heights: vec![Some(3)],
        signatures: Vec::new(),
        prior_heights: Vec::new(),
        parsed_len: None,
        width: 80,
        total: 3,
        scroll: 0,
        // Long past the throttle interval: only liveness gates the reload.
        last_reload: std::time::Instant::now()
            .checked_sub(std::time::Duration::from_mins(1))
            .unwrap(),
        live: false,
        reloaded: false,
        view_h: 10,
    });
    a.refresh_subagent_view();
    // The test app has no registry, so the run is not live: no reload.
    let view = a
        .subagents_modal
        .as_ref()
        .and_then(|modal| modal.viewing.as_ref())
        .unwrap();
    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.heights, vec![Some(3)]);
}

#[test]
fn subagent_transcript_view_watermark_stops_at_a_torn_trailing_line() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let mut events = blank_events([msg(user("complete answer"))]);
    sub.append_events(&mut events).unwrap();
    let complete_len = std::fs::metadata(sub.path()).unwrap().len();
    // A live writer can leave a partial line at the end of the file.
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(sub.path())
        .unwrap();
    file.write_all(b"{\"partial\":").unwrap();
    drop(file);
    assert!(std::fs::metadata(sub.path()).unwrap().len() > complete_len);

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    assert!(a.slash_command("/subagent"));
    let mut run = None;
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);

    let view = a
        .subagents_modal
        .as_ref()
        .and_then(|modal| modal.viewing.as_ref())
        .unwrap();
    // The watermark covers only the parsed bytes, so the torn tail still
    // counts as unseen and earns a later reload.
    assert_eq!(view.parsed_len, Some(complete_len));

    let modal = a.subagents_modal.as_mut().unwrap();
    let view = modal.viewing.as_mut().unwrap();
    view.live = true;
    view.last_reload = std::time::Instant::now()
        .checked_sub(std::time::Duration::from_mins(1))
        .unwrap();
    a.refresh_subagent_view();
    let view = a
        .subagents_modal
        .as_ref()
        .and_then(|modal| modal.viewing.as_ref())
        .unwrap();
    // The file is longer than the watermark, so the settle reload ran.
    assert!(view.reloaded);
    assert_eq!(view.parsed_len, Some(complete_len));
}

#[test]
fn refresh_subagent_view_skips_the_final_reload_when_the_file_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let mut events = blank_events([msg(user("settled answer"))]);
    sub.append_events(&mut events).unwrap();
    let parsed_len = std::fs::metadata(sub.path()).unwrap().len();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    a.subagents_modal = Some(SubagentsModalState {
        selected: 0,
        rows: Vec::new(),
        viewing: Some(SubagentTranscriptView {
            id: "run-1".to_string(),
            file: Some(
                sub.path()
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap(),
            ),
            path: Some(sub.path().to_path_buf()),
            turns: Vec::new(),
            load_error: None,
            heights: Vec::new(),
            signatures: Vec::new(),
            prior_heights: Vec::new(),
            parsed_len: Some(parsed_len),
            width: 0,
            total: 0,
            scroll: 0,
            last_reload: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_mins(1))
                .unwrap(),
            live: true,
            reloaded: false,
            view_h: 10,
        }),
    });
    a.refresh_subagent_view();

    let view = a
        .subagents_modal
        .as_ref()
        .and_then(|modal| modal.viewing.as_ref())
        .unwrap();
    // The settle would reload once, but the file length matches the parsed
    // watermark: only the liveness bookkeeping moves.
    assert!(!view.live);
    assert!(view.turns.is_empty());
    assert!(!view.reloaded);
}

#[test]
fn refresh_subagent_view_reloads_once_when_a_live_run_settles() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let file_name = sub
        .path()
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let mut events = blank_events([msg(user("late answer"))]);
    test_append_events(sub.path(), &mut events, None).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    a.subagents_modal = Some(SubagentsModalState {
        selected: 0,
        rows: Vec::new(),
        viewing: Some(SubagentTranscriptView {
            id: "run-1".to_string(),
            file: Some(file_name),
            path: None,
            turns: Vec::new(),
            load_error: None,
            heights: Vec::new(),
            signatures: Vec::new(),
            prior_heights: Vec::new(),
            parsed_len: None,
            width: 0,
            total: 0,
            scroll: 0,
            last_reload: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_mins(1))
                .unwrap(),
            // The view was live at the last load, and the test app has no
            // registry: the settle path reloads one final time.
            live: true,
            reloaded: false,
            view_h: 10,
        }),
    });
    a.refresh_subagent_view();

    let view = a
        .subagents_modal
        .as_ref()
        .and_then(|modal| modal.viewing.as_ref())
        .unwrap();
    assert!(!view.live);
    assert_eq!(view.turns.len(), 1);
    assert_eq!(view.turns[0].prompt, "late answer");
}

#[test]
fn subagent_transcript_reload_keeps_a_scrolled_up_view_on_the_same_content() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let body = (0..40)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let turn_end = || SessionEventKind::TurnEnd {
        model: "openai/gpt-4o".into(),
        elapsed_ms: 1000,
        cost: 0.0,
        usage: Usage::default(),
        stop_reason: None,
    };
    let mut events = blank_events([
        msg(user("inspect the repo")),
        msg(assistant(&body)),
        turn_end(),
    ]);
    sub.append_events(&mut events).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    open_scrolled_subagent_view(&mut a, &mut term);
    let before = subagent_screen(&term);
    assert!(before.contains("line 20"), "scrolled frame:\n{before}");
    assert!(!before.contains("line 39"), "scrolled frame:\n{before}");

    // The live run appends a turn while the user reads the scrolled-up
    // position.
    let mut second = blank_events([
        msg(user("and now")),
        msg(assistant("more output\nthat keeps\ngrowing")),
        turn_end(),
    ]);
    test_append_events(sub.path(), &mut second, None).unwrap();
    {
        let modal = a.subagents_modal.as_mut().unwrap();
        let view = modal.viewing.as_mut().unwrap();
        // No registry in the test app: the settle path does the reload.
        view.live = true;
        view.last_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_mins(1))
            .unwrap();
    }
    a.refresh_subagent_view();
    assert!(
        a.subagents_modal
            .as_ref()
            .and_then(|modal| modal.viewing.as_ref())
            .unwrap()
            .reloaded
    );

    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    let after = subagent_screen(&term);
    // The scrollbar thumb may move (the total grew); the visible content
    // window must not.
    for marker in ["line 16", "line 20", "line 31"] {
        assert!(
            after.contains(marker),
            "after frame lost {marker}:\n{after}"
        );
    }
    for marker in ["line 15", "line 32", "and now"] {
        assert!(
            !after.contains(marker),
            "after frame gained {marker}:\n{after}"
        );
    }
}

#[test]
fn subagent_transcript_reload_compensates_a_shrink_below_the_window_top() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let turn_end = || SessionEventKind::TurnEnd {
        model: "openai/gpt-4o".into(),
        elapsed_ms: 1000,
        cost: 0.0,
        usage: Usage::default(),
        stop_reason: None,
    };
    let body = |lines: &[usize]| {
        lines
            .iter()
            .map(|i| format!("line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let full: Vec<usize> = (0..40).collect();
    let mut events = blank_events([
        msg(user("inspect the repo")),
        msg(assistant(&body(&full))),
        turn_end(),
    ]);
    sub.append_events(&mut events).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    open_scrolled_subagent_view(&mut a, &mut term);
    let before = subagent_screen(&term);
    for marker in ["line 16", "line 20", "line 31"] {
        assert!(
            before.contains(marker),
            "scrolled frame lost {marker}:\n{before}"
        );
    }
    for marker in ["line 15", "line 32"] {
        assert!(
            !before.contains(marker),
            "scrolled frame gained {marker}:\n{before}"
        );
    }

    // The display budget keeps a prefix and a tail of each string, so it
    // cut a middle span of the turn below the window top. Files are
    // append-only in production; the rewrite models the re-truncated view
    // the display reader would return. The meta header must survive.
    let original = std::fs::read(sub.path()).unwrap();
    let header_end = original.iter().position(|&b| b == b'\n').unwrap() + 1;
    std::fs::remove_file(sub.path()).unwrap();
    std::fs::write(sub.path(), &original[..header_end]).unwrap();
    let cursor = store::SessionCursor::new(sub.path().to_path_buf(), None);
    // The reader keeps a prefix and a tail of about the same length.
    let cut: Vec<usize> = (0..18).chain(23..40).collect();
    let mut shorter = blank_events([
        msg(user("inspect the repo")),
        msg(assistant(&body(&cut))),
        turn_end(),
    ]);
    cursor.append_events(&mut shorter).unwrap();
    {
        let modal = a.subagents_modal.as_mut().unwrap();
        let view = modal.viewing.as_mut().unwrap();
        // No registry in the test app: the settle path does the reload.
        view.live = true;
        view.last_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_mins(1))
            .unwrap();
    }
    a.refresh_subagent_view();
    assert!(
        a.subagents_modal
            .as_ref()
            .and_then(|modal| modal.viewing.as_ref())
            .unwrap()
            .reloaded
    );

    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    let after = subagent_screen(&term);
    // The rows above the cut must not move; the kept tail slides up into
    // view below it.
    for marker in ["line 16", "line 17", "line 23", "line 28", "line 34"] {
        assert!(
            after.contains(marker),
            "after frame lost {marker}:\n{after}"
        );
    }
    for marker in ["line 11", "line 22", "line 39"] {
        assert!(
            !after.contains(marker),
            "after frame gained {marker}:\n{after}"
        );
    }
}

#[test]
fn subagent_transcript_reload_compensates_when_the_straddling_turn_grows() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let dir = tempfile::tempdir().unwrap();
    let cwd = Path::new("/workspace");
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let parent = store.create_cursor(cwd, &"openai/gpt-4o".into()).unwrap();
    let stem = parent
        .path()
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap();
    let sub = store
        .create_subagent_cursor(cwd, &"openai/gpt-4o".into(), Some(&stem), "run-1")
        .unwrap();
    let body = (0..40)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    // No TurnEnd: this turn is still in progress and keeps growing.
    let mut events = blank_events([msg(user("inspect the repo")), msg(assistant(&body))]);
    sub.append_events(&mut events).unwrap();

    let mut a = app();
    attach_session_sink(&mut a, store, cwd, parent);
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    open_scrolled_subagent_view(&mut a, &mut term);
    let before = subagent_screen(&term);
    for marker in ["line 14", "line 20", "line 29"] {
        assert!(
            before.contains(marker),
            "scrolled frame lost {marker}:\n{before}"
        );
    }
    for marker in ["line 13", "line 30", "line 39"] {
        assert!(
            !before.contains(marker),
            "scrolled frame gained {marker}:\n{before}"
        );
    }

    // The in-progress turn grows at its bottom, below the window top.
    let growth = (40..46)
        .map(|i| format!("line {i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut more = blank_events([msg(assistant(&growth))]);
    test_append_events(sub.path(), &mut more, None).unwrap();
    {
        let modal = a.subagents_modal.as_mut().unwrap();
        let view = modal.viewing.as_mut().unwrap();
        // No registry in the test app: the settle path does the reload.
        view.live = true;
        view.last_reload = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_mins(1))
            .unwrap();
    }
    a.refresh_subagent_view();
    assert!(
        a.subagents_modal
            .as_ref()
            .and_then(|modal| modal.viewing.as_ref())
            .unwrap()
            .reloaded
    );

    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    let after = subagent_screen(&term);
    // The turn straddles the window top, so its growth would slide the
    // visible content without compensation.
    for marker in ["line 14", "line 20", "line 29"] {
        assert!(
            after.contains(marker),
            "after frame lost {marker}:\n{after}"
        );
    }
    for marker in ["line 13", "line 30", "line 40"] {
        assert!(
            !after.contains(marker),
            "after frame gained {marker}:\n{after}"
        );
    }
}
