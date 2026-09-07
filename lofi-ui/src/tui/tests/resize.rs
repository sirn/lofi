use super::*;

#[test]
fn frozen_cache_invalidates_on_width_change() {
    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: "word ".repeat(30),
        blocks: Vec::new(),
    });
    push_turn(&mut a);

    a.ensure_frozen(20);
    a.sync_frozen_cache_for_viewport(0, 24, 20);
    let h_narrow = a.frozen_heights[0];
    assert!(a.frozen_render.get(0).is_some());

    a.ensure_frozen(100);
    a.sync_frozen_cache_for_viewport(0, 24, 100);
    // A width-only resize defers the re-measure to the tick loop; drain it
    // here so the height reflects the new width.
    while a.remeasure_heights_step(16) {}
    let h_wide = a.frozen_heights[0];
    assert!(a.frozen_render.get(0).is_some());
    assert!(
        h_wide < h_narrow,
        "frozen cache should re-wrap at the new width: narrow={h_narrow} wide={h_wide}"
    );
}

#[test]
fn resize_defers_height_remeasure_off_the_frame() {
    let mut a = app();
    for _ in 0..40 {
        a.turns.push(Turn {
            kind: lofi_types::PromptKind::User,
            prompt: "word ".repeat(40),
            blocks: Vec::new(),
        });
        push_turn(&mut a);
    }

    a.ensure_frozen(24);
    let narrow: Vec<usize> = a.frozen_heights.clone();
    let frozen_n = narrow.len();
    // The live last turn is not frozen, so the frozen prefix is one shorter.
    assert_eq!(frozen_n, a.turns.len() - 1);

    // A width-only resize keeps the (stale) heights so the frame draws
    // immediately, and schedules an incremental re-measure instead of
    // recomputing all turns synchronously.
    a.ensure_frozen(100);
    assert_eq!(
        a.frozen_heights, narrow,
        "resize keeps prior heights until the tick loop re-measures them"
    );
    assert!(a.height_remeasure_from.is_some());

    // Re-measure works back-to-front: the visible bottom turns are exact
    // after the first step while the off-screen prefix is still pending.
    a.remeasure_heights_step(8);
    let last = frozen_n - 1;
    assert!(
        a.frozen_heights[last] <= narrow[last],
        "widening should not grow the bottom turn height: {:?} vs {:?}",
        a.frozen_heights[last],
        narrow[last]
    );
    assert!(a.height_remeasure_from.is_some(), "prefix still pending");

    // Draining converges every height to the wide measurement.
    while a.remeasure_heights_step(8) {}
    assert!(a.height_remeasure_from.is_none());
    for (idx, (wide, narrow)) in a.frozen_heights.iter().zip(&narrow).enumerate() {
        assert!(
            wide <= narrow,
            "turn {idx} should re-wrap no taller at the wider width: {wide} vs {narrow}"
        );
    }
}

#[test]
fn height_remeasure_does_not_populate_the_collapsed_cache() {
    use lofi_core::session::store::SessionStore;
    // File-backed resumed turns: exact heights are seeded from byte ranges
    // and converged by the tick-loop re-measure. That pass walks every turn;
    // it must not claim collapsed-cache slots — the cache is for turns the
    // user actually views, not a mirror of the whole transcript.
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let cursor = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap();
    let path = cursor.path().to_path_buf();
    let mut evs = Vec::new();
    for i in 0..3 {
        evs.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                origin: None,
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: format!("prompt {i}"),
                }],
                kind: PromptKind::default(),
            }),
        });
        evs.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                origin: None,
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: format!("answer {i}"),
                }],
                kind: PromptKind::default(),
            }),
        });
        evs.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        });
    }
    test_append_events(&path, &mut evs, None).unwrap();

    let mut a = app();
    attach_session_sink(
        &mut a,
        store,
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
    let c0 = a.session.cursor.as_ref().unwrap().clone();
    let snap = c0.snapshot().unwrap();
    a.restore_indexed_session(
        &c0,
        &snap.index,
        snap.file_size,
        snap.history_start,
        snap.contiguous,
    )
    .unwrap();

    a.ensure_frozen(40);
    assert!(
        a.height_remeasure_from.is_some(),
        "estimated heights pending exact re-measure"
    );
    while a.remeasure_heights_step(16) {}
    assert!(
        a.collapsed_turns.borrow().map.is_empty(),
        "re-measure must not retain collapsed turns"
    );

    // Viewport materialization still caches what the user looks at.
    let _ = a.materialize_turn(0);
    assert_eq!(a.collapsed_turns.borrow().map.len(), 1);
}

#[test]
fn resize_reanchors_scrolled_up_view_instead_of_snapping_to_bottom() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    // A long prompt that wraps to many lines when narrow and far fewer when
    // wide, so the re-wrap materially shrinks `total`/`base` on resize.
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: "word ".repeat(3000),
        blocks: Vec::new(),
    });
    push_turn(&mut a);

    let mut term = Terminal::new(TestBackend::new(30, 20)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let base_narrow = a.last_base;
    assert!(
        base_narrow > 0,
        "narrow transcript should overflow the viewport"
    );
    a.pinned = false;
    a.top_line = base_narrow / 2;
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert!(!a.pinned, "scrolled up, not following the tail");
    assert_eq!(a.log_off, a.top_line);
    assert!(a.log_off < a.last_base, "scrolled up, not at the bottom");

    let mut term = Terminal::new(TestBackend::new(120, 20)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert!(
        !a.pinned,
        "resize should not snap a scrolled-up view to the bottom"
    );
    assert!(
        a.log_off < a.last_base,
        "view should remain scrolled up after resize, not pinned to the bottom"
    );
}

#[test]
fn resize_keeps_nav_cursor_on_same_content_line() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    let prompt =
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu "
            .repeat(2);
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt,
        blocks: Vec::new(),
    });
    push_turn(&mut a); // turn[1] keeps turn[0] frozen.
    a.mode = Mode::Navigate;

    let mut term = Terminal::new(TestBackend::new(28, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let (intra, c) = {
        let narrow = a.frozen_render.get(0).expect("turn 0 frozen");
        let intra = 4.min(narrow.len().saturating_sub(1));
        let c: usize = narrow
            .iter()
            .take(intra)
            .map(view::RenderLine::content_len)
            .sum();
        (intra, c)
    };
    a.nav_cursor = a.turn_start_line(0) + intra;
    a.nav_show_cursor();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();

    // Widen: line boundaries move, but the cursor must stay on the wide line
    // that contains the same content char (offset `c`).
    let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let (start, len, on_screen) = {
        let wide = a.frozen_render.get(0).expect("turn 0 frozen");
        let new_intra = a.nav_cursor - a.turn_start_line(0);
        assert!(new_intra < wide.len(), "cursor should land within turn 0");
        let start: usize = wide
            .iter()
            .take(new_intra)
            .map(view::RenderLine::content_len)
            .sum();
        let len = wide[new_intra].content_len();
        let on_screen = a.nav_cursor >= a.log_off && a.nav_cursor < a.log_off + a.log_view_h;
        (start, len, on_screen)
    };
    assert!(
        start <= c && c < start + len,
        "cursor should be on the wide line containing content offset {c}, got [{start}, {})",
        start + len
    );
    assert!(on_screen, "cursor should stay on screen after resize");
}

#[test]
fn resize_keeps_nav_cursor_cell_on_same_content_char() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    let prompt =
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu "
            .repeat(2);
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt,
        blocks: Vec::new(),
    });
    push_turn(&mut a);
    a.mode = Mode::Navigate;

    let mut term = Terminal::new(TestBackend::new(28, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let (intra, nav_col, char_pos) = {
        let narrow = a.frozen_render.get(0).expect("turn 0 frozen");
        let intra = 4.min(narrow.len().saturating_sub(1));
        let rl = &narrow[intra];
        let nav_col = rl.content.0 + (rl.content_len() / 2).max(1);
        let char_pos = app_nav::cursor_char_pos(narrow, intra, nav_col);
        (intra, nav_col, char_pos)
    };
    a.nav_cursor = a.turn_start_line(0) + intra;
    a.nav_col = nav_col;
    a.nav_show_cursor();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();

    let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let new_char_pos = {
        let wide = a.frozen_render.get(0).expect("turn 0 frozen");
        let new_intra = a.nav_cursor - a.turn_start_line(0);
        assert!(new_intra < wide.len(), "cursor should land within turn 0");
        app_nav::cursor_char_pos(wide, new_intra, a.nav_col)
    };
    assert_eq!(
        new_char_pos, char_pos,
        "cursor cell should stay on the same content char after resize"
    );
}

#[test]
fn resize_keeps_select_anchor_on_same_content_char() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    let prompt =
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu "
            .repeat(2);
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt,
        blocks: Vec::new(),
    });
    push_turn(&mut a);

    let mut term = Terminal::new(TestBackend::new(28, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let (anchor_intra, anchor_col, anchor_char) = {
        let narrow = a.frozen_render.get(0).expect("turn 0 frozen");
        let intra = 2.min(narrow.len().saturating_sub(1));
        let rl = &narrow[intra];
        let col = rl.content.0 + (rl.content_len() / 2).max(1);
        (intra, col, app_nav::cursor_char_pos(narrow, intra, col))
    };
    let (cur_intra, cur_col, cur_char) = {
        let narrow = a.frozen_render.get(0).expect("turn 0 frozen");
        let intra = 5.min(narrow.len().saturating_sub(1));
        let rl = &narrow[intra];
        let col = rl.content.0 + (rl.content_len() / 2).max(1);
        (intra, col, app_nav::cursor_char_pos(narrow, intra, col))
    };
    a.mode = Mode::Select;
    a.select_anchor = (a.turn_start_line(0) + anchor_intra, anchor_col);
    a.nav_cursor = a.turn_start_line(0) + cur_intra;
    a.nav_col = cur_col;
    a.sel = Some(a.select_sel());
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();

    let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let wide = a.frozen_render.get(0).expect("turn 0 frozen");

    let new_anchor_char = {
        let intra = a.select_anchor.0 - a.turn_start_line(0);
        assert!(intra < wide.len(), "anchor should land within turn 0");
        app_nav::cursor_char_pos(wide, intra, a.select_anchor.1)
    };
    let new_cur_char = {
        let intra = a.nav_cursor - a.turn_start_line(0);
        assert!(intra < wide.len(), "cursor should land within turn 0");
        app_nav::cursor_char_pos(wide, intra, a.nav_col)
    };
    assert_eq!(
        new_anchor_char, anchor_char,
        "select anchor should stay on the same content char after resize"
    );
    assert_eq!(
        new_cur_char, cur_char,
        "select cursor should stay on the same content char after resize"
    );
}
#[test]
fn resize_keeps_nav_cursor_at_its_viewport_row() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    // Enough turns to overflow the viewport at both widths, so the viewport
    // can scroll and the cursor's row is meaningful.
    let prompt =
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu ".to_string();
    for _ in 0..6 {
        a.turns.push(Turn {
            kind: lofi_types::PromptKind::User,
            prompt: prompt.clone(),
            blocks: Vec::new(),
        });
        push_turn(&mut a);
    }
    a.mode = Mode::Navigate;

    let mut term = Terminal::new(TestBackend::new(28, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert!(
        a.log_total > a.log_view_h,
        "transcript overflows the viewport"
    );
    // Place the cursor on a content line near the top (turn 2) and park the
    // viewport so it sits at row 4, unpinned (so the viewport can follow it).
    let k = 2;
    let intra = 2.min(a.frozen_render.get(k).unwrap().len().saturating_sub(1));
    a.nav_cursor = a.turn_start_line(k) + intra;
    a.pinned = false;
    a.top_line = a.nav_cursor.saturating_sub(4);
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let row = a.nav_cursor - a.log_off;
    assert_eq!(row, 4);

    // Widen: the intervening lines re-wrap, but the cursor stays on its
    // previous viewport row — the viewport follows it instead of drifting.
    let mut term = Terminal::new(TestBackend::new(90, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert!(a.log_total > a.log_view_h, "still overflows after widen");
    assert_eq!(
        a.nav_cursor - a.log_off,
        row,
        "cursor viewport row should be preserved across a re-wrap"
    );
}

#[test]
fn resize_clamps_nav_cursor_to_edge_on_height_shrink() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: "word ".repeat(4000),
        blocks: Vec::new(),
    });
    push_turn(&mut a);
    a.mode = Mode::Navigate;

    let mut term = Terminal::new(TestBackend::new(60, 40)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let h_tall = a.log_view_h;
    assert!(h_tall > 12, "tall viewport");
    let row = h_tall - 2;
    a.nav_cursor = a.log_off + row;
    a.nav_show_cursor();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert_eq!(a.nav_cursor - a.log_off, row);

    // Shrink the height (same width) so the old row no longer fits.
    let mut term = Terminal::new(TestBackend::new(60, 12)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert!(
        a.log_view_h <= row,
        "new viewport shorter than the old cursor row"
    );
    assert_eq!(
        a.nav_cursor,
        a.log_off + a.log_view_h - 1,
        "cursor should clamp to the bottom edge on height shrink"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn resize_keeps_nav_cursor_on_exec_header_across_wrap() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "p".to_string(),
    });
    a.apply_event(AgentEvent::Text(
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu ".repeat(3),
    ));
    // A first exec block whose indented code/result sits *before* the
    // cursor's exec header — its per-row indent must not count as content,
    // or the header's cumulative offset drifts on resize (exercises
    // `wrap_pre` + the leading-whitespace exclusion in `render`).
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "    const p = \"TODO.md\";\n    const s = await read(p);\n    return s.indexOf(\"## Sub\");".to_string(),
        label: Some("inspect".to_string()),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "  1 - Accept prompt model thinking workspace output_limit\n  2 - Default to interactive".to_string(),
        is_error: false,
        elapsed_ms: 100,
    });
    a.apply_event(AgentEvent::ToolStart {
        id: "e2".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e2".to_string(),
        code: "    const r = await run();".to_string(),
        label: Some("act".to_string()),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e2".to_string(),
        result: "done".to_string(),
        is_error: false,
        elapsed_ms: 50,
    });
    a.apply_event(AgentEvent::TurnEnd {
        model: "m · medium".into(),
        elapsed_ms: 200,
        cost: 0.0,
        usage: Usage::default(),
        stop_reason: None,
    });
    // A second turn so the first is frozen (exercises the frozen-render path).
    push_turn(&mut a);
    a.mode = Mode::Navigate;

    let line_text = |a: &App, abs: usize| -> String {
        let n = a.turns.len();
        let mut k = 0;
        for i in 0..n {
            if a.turn_start_line(i) <= abs {
                k = i;
            } else {
                break;
            }
        }
        let intra = abs - a.turn_start_line(k);
        let last = k + 1 == n;
        let text_of = |v: &Vec<view::RenderLine>| {
            v.get(intra)
                .map(|rl| {
                    let chars: Vec<char> = rl
                        .line
                        .spans
                        .iter()
                        .flat_map(|s| s.content.chars())
                        .collect();
                    let s = rl.content.0.min(chars.len());
                    let e = rl.content.1.min(chars.len());
                    chars[s..e].iter().collect::<String>()
                })
                .unwrap_or_default()
        };
        if last {
            let cx = view::component::Cx {
                app: a,
                theme: a.theme,
                width: a.frozen_width,
                active_turn: a.run_active(),
            };
            let v = view::blocks::render_turn_lines(&cx, &a.turns[k]);
            text_of(&v)
        } else {
            a.frozen_render.get(k).map(text_of).unwrap_or_default()
        }
    };

    let mut term = Terminal::new(TestBackend::new(64, 57)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let exec_idx = (0..a.log_total)
        .rev()
        .find(|&i| line_text(&a, i).starts_with("Exec"))
        .expect("an Exec header");
    a.nav_cursor = exec_idx;
    a.nav_col = 4;
    a.nav_show_cursor();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();

    // Widen: the cursor must stay on the exact same Exec header (same content
    // char), not drift to the other Exec header, the text, or the code lines.
    let before = line_text(&a, a.nav_cursor);
    let mut term = Terminal::new(TestBackend::new(98, 57)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert_eq!(
        line_text(&a, a.nav_cursor),
        before,
        "cursor drifted to: {:?}",
        line_text(&a, a.nav_cursor)
    );
}

#[test]
fn fence_renders_plain_backticks_on_full_width_tile() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::Text(
            "before\n```rust\nlet x = 1;\n```\nafter".to_string(),
        )],
    });
    let turn = &a.turns[0];
    let w = 40usize;
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: w,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let lines: Vec<String> = rls
        .iter()
        .map(|rl| rl.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let open = lines
        .iter()
        .find(|l| l.starts_with("▌ ```rust"))
        .expect("opening ```rust");
    let code = lines
        .iter()
        .find(|l| l.contains("let x = 1;"))
        .expect("code line");
    let close = lines
        .iter()
        .find(|l| l.trim_start_matches("▌ ").trim() == "```")
        .expect("closing ```");
    for l in &lines {
        assert!(!l.contains('╭'), "stray frame art: {l}");
        assert!(!l.contains('╰'), "stray frame art: {l}");
        assert!(!l.contains('│'), "stray rail: {l}");
    }
    for l in [&open.clone(), &code.clone(), &close.clone()] {
        assert_eq!(l.chars().count(), w, "line not full-width: {l:?}");
    }
    // The content range excludes the leading gutter and the trailing bg
    // padding, so the gutter and right gutter never get selected/copied.
    let open_rl = rls
        .iter()
        .find(|rl| {
            rl.line
                .spans
                .iter()
                .any(|s| s.content.starts_with("```rust"))
        })
        .expect("open rl");
    let chars: String = open_rl
        .line
        .spans
        .iter()
        .flat_map(|s| s.content.chars())
        .collect();
    let content: String = chars
        .chars()
        .skip(open_rl.content.0)
        .take(open_rl.content.1 - open_rl.content.0)
        .collect();
    assert_eq!(content, "```rust");
}

#[test]
fn permission_dialog_caps_height_and_scrolls_command_preview() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    let command = (0..30)
        .map(|n| format!("command-line-{n:02}"))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    let (req, _response) = confirm_request(&command);
    a.pending_confirms.push(req);
    let mut term = Terminal::new(TestBackend::new(90, 50)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    assert_eq!(
        (a.confirm_total, a.confirm_view_h, a.confirm_scroll),
        (30, 12, 0)
    );
    a.handle_confirm_key(&KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    a.handle_confirm_key(&KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(a.confirm_scroll, 2);
    a.handle_confirm_key(&KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(a.confirm_scroll, 18);
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    let screen = (0..50)
        .map(|y| (0..90).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert!(screen.contains("command-line-29"), "{screen}");
    assert!(!screen.contains("command-line-00"), "{screen}");
    a.handle_confirm_key(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
    assert_eq!(a.confirm_scroll, 17);
    assert_eq!(a.pending_confirms.len(), 1);
}

// Write five "old" messages, then a zero-tail compaction marker folding them
