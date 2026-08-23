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
    assert!(a.turns.is_empty());

    a.slash_command("/help");
    assert!(a.turns.is_empty());
    assert!(a.info.is_some());
    a.info = None;

    // Unknown commands notify on the rule line instead of pushing a turn.
    assert!(a.slash_command("/nope"));
    assert!(a.turns.is_empty());
    let (msg, kind) = a.notify_badge().expect("unknown command notified");
    assert_eq!(kind, NotifyKind::Error);
    assert!(msg.contains("unknown command"));

    assert!(a.slash_command("/resume"));
    assert!(a.picker.is_none());
    let (msg, kind) = a.notify_badge().expect("/resume notified");
    assert_eq!(kind, NotifyKind::Warn);
    assert!(msg.contains("disabled"));

    assert!(a.slash_command("/new"));
    assert!(a.turns.is_empty());
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
    assert!(a.turns.is_empty());
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
    let rule_y = (0..20)
        .find(|&y| buf[(0, y)].symbol() == "╱")
        .expect("prompt rule");
    for y in rule_y + 1..20 {
        assert_eq!(buf[(0, y)].symbol(), "▌", "panel row {y}");
        assert_eq!(buf[(0, y)].fg, a.theme.user, "focused rail row {y}");
    }

    a.mode = Mode::Navigate;
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    for y in rule_y + 1..20 {
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
    assert!(a.turns.is_empty());
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
