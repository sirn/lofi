use super::*;

/// App with a file-backed session whose turns claim far more bytes than they
/// render, so `ensure_frozen` seeds heights from estimates that then converge
/// on the tick loop. That is the shape of a resumed transcript: the byte range
/// still holds the released exec results the collapsed view never shows.
fn resumed_with_estimates(turns: usize) -> App {
    use lofi_core::session::store::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let cursor = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap();
    let path = cursor.path().to_path_buf();
    let mut evs = Vec::new();
    for i in 0..turns {
        evs.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(user(&format!("prompt {i}"))),
        });
        evs.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(assistant(&format!("answer {i}"))),
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
    for range in &mut a.transcript.turn_byte_ranges {
        *range = Some((0, 20_000));
    }
    // Production startup replaces the replayed transcript before the first
    // draw (run.rs).
    a.bump_render_epoch();
    a
}

fn draw(a: &mut App) -> (String, String) {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, a)).unwrap();
    let buf = term.backend().buffer();
    let mut rows = String::new();
    let mut gutter = String::new();
    for y in 0..buf.area.height {
        let mut row = String::new();
        // The gutter is compared on its own: the scrollbar is allowed to hold
        // an approximate extent while the frame's content must not move.
        for x in 0..buf.area.width - 1 {
            row.push_str(buf[(x, y)].symbol());
        }
        gutter.push_str(buf[(buf.area.width - 1, y)].symbol());
        rows.push_str(row.trim_end());
        rows.push('\n');
    }
    (rows, gutter)
}

/// The reported bug: scrolling right after startup, while estimated heights
/// are still being replaced by exact ones, must move the view by exactly the
/// requested amount. Convergence may not slide the content under the viewport
/// or breathe the scrollbar, because nothing on screen depends on the
/// off-screen prefix's estimate once the window is anchored to its content.
#[test]
fn startup_convergence_keeps_a_scrolled_view_and_its_thumb_still() {
    let mut a = resumed_with_estimates(60);
    let _ = draw(&mut a);
    assert!(
        a.layout.frozen_heights_estimated.iter().any(|&e| e),
        "estimates pending after the first frame"
    );
    let estimate_total = a.log_total;

    a.mode = Mode::Input;
    a.page_up();
    let (screen, gutter) = draw(&mut a);
    assert!(!a.pinned, "page up leaves the tail");
    assert!(
        a.log_off + a.log_view_h < estimate_total,
        "scrolled up off the tail"
    );

    let mut steps = 0;
    while a.remeasure_heights_step(16) {
        steps += 1;
        let (screen_now, gutter_now) = draw(&mut a);
        assert_eq!(
            screen_now, screen,
            "step {steps} moved the content on screen without input"
        );
        if a.layout.height_remeasure_from.is_some() {
            assert_eq!(
                gutter_now, gutter,
                "step {steps} moved the scrollbar while the extent was still settling"
            );
        }
    }
    assert!(steps > 1, "the test needs several convergence steps");
    assert_ne!(
        a.log_total, estimate_total,
        "the estimate was replaced by exact heights"
    );
    // Once the extent is exact the gauge takes it, and the thumb then means
    // what it says.
    assert_eq!(a.layout.height_remeasure_from, None);
    let _ = draw(&mut a);
    assert_eq!(a.scroll_gauge, Some((a.log_off, a.log_total)));
}

/// A submitted prompt changes the content, so the thumb must take the new
/// extent on that frame even while the off-screen prefix is still estimated.
/// The hold is for estimate noise, not for input: it kept a submitted turn
/// from reaching the scrollbar for the whole convergence window.
#[test]
fn a_submit_moves_the_thumb_while_estimates_are_still_pending() {
    let mut a = resumed_with_estimates(60);
    let _ = draw(&mut a);
    assert!(
        a.layout.frozen_heights_estimated.iter().any(|&e| e),
        "estimates pending after the first frame"
    );
    let _ = draw(&mut a);
    let stale_total = a.scroll_gauge.map(|(_, total)| total);
    assert_eq!(stale_total, Some(a.log_total), "the gauge is current");

    a.transcript.push_turn(Turn {
        prompt: "next".into(),
        kind: lofi_types::PromptKind::User,
        blocks: Vec::new(),
    });
    a.pin_to_latest();
    let _ = draw(&mut a);

    assert_ne!(
        Some(a.log_total),
        stale_total,
        "the submitted turn grew the content"
    );
    assert_eq!(
        a.scroll_gauge,
        Some((a.log_off, a.log_total)),
        "the gauge must follow the submitted turn"
    );
}

/// A tail-following view does not store its own offset, so paging must start
/// from the base rather than from a `top_line` left over from before it
/// started following the tail.
#[test]
fn page_up_from_a_tail_following_start_moves_one_page() {
    let mut a = app();
    a.mode = Mode::Input;
    a.log_total = 40;
    a.log_view_h = 10;
    a.last_base = 30;
    a.top_line = 0;
    a.pinned = true;
    a.page_up();
    assert!(!a.pinned);
    assert_eq!(a.top_line, 20);
    assert_eq!(a.view_off(), 20);
}

#[test]
fn page_down_from_a_tail_following_start_stays_at_the_tail() {
    let mut a = app();
    a.mode = Mode::Input;
    a.log_total = 40;
    a.log_view_h = 10;
    a.last_base = 30;
    a.pinned = true;
    a.page_down();
    assert!(a.pinned);
    assert_eq!(a.view_off(), 30);
}

/// The last re-measure batch changes heights and then reports "no work left".
/// The caller must still redraw, or the frame keeps a total that no longer
/// matches the heights it draws.
#[test]
fn final_height_batch_still_requests_a_redraw() {
    let mut a = resumed_with_estimates(60);
    let _ = draw(&mut a);
    let mut steps = 0;
    loop {
        // The tick loop's shape: redraw only when the step reports a change.
        if !a.remeasure_heights_step(16) {
            break;
        }
        steps += 1;
        let _ = draw(&mut a);
    }
    assert!(a.layout.height_remeasure_from.is_none(), "converged");
    let exact = a.layout.frozen_heights.iter().sum::<usize>()
        + a.last_turn_height
        + a.transcript.turns.len().saturating_sub(1);
    assert_eq!(
        a.log_total, exact,
        "the last batch needs a frame of its own ({steps} steps)"
    );
}

/// Every frame publishes geometry derived from the heights it draws with, so a
/// single frame never mixes an old total with newly measured heights.
#[test]
fn the_first_frame_publishes_measured_geometry() {
    let mut a = resumed_with_estimates(60);
    let (screen, _) = draw(&mut a);
    assert!(
        screen.contains("prompt 59"),
        "the first frame lands on the tail, not on a window sliced by stale estimates"
    );
    let published = a.layout.frozen_heights.iter().sum::<usize>()
        + a.last_turn_height
        + a.transcript.turns.len().saturating_sub(1);
    assert_eq!(a.log_total, published);
    assert_eq!(a.log_off, a.view_off());
}

/// Re-anchoring keeps the same turn row at the top when a turn above the
/// viewport is measured.
#[test]
fn reanchoring_follows_the_content_above_the_viewport() {
    let mut a = app();
    for _ in 0..4 {
        push_turn(&mut a);
    }
    a.layout.frozen_heights = vec![10, 10];
    a.layout.frozen_heights_estimated = vec![false, false];
    a.last_turn_height = 10;
    a.pinned = false;
    a.log_off = 25;
    a.top_line = 25;
    a.remember_view_anchor();
    // The first turn was overestimated: it is really 4 rows, so everything
    // below it moves up 6.
    a.layout.frozen_heights[0] = 4;
    a.reanchor_view_top();
    assert_eq!(a.top_line, 19);
}

/// The anchor is a position, not a lock: a `top_line` the user changed after
/// the anchor was recorded wins.
#[test]
fn reanchoring_leaves_a_newly_requested_position_alone() {
    let mut a = app();
    for _ in 0..4 {
        push_turn(&mut a);
    }
    a.layout.frozen_heights = vec![10, 10];
    a.layout.frozen_heights_estimated = vec![false, false];
    a.last_turn_height = 10;
    a.pinned = false;
    a.log_off = 25;
    a.top_line = 25;
    a.remember_view_anchor();
    a.top_line = 3;
    a.layout.frozen_heights[0] = 4;
    a.reanchor_view_top();
    assert_eq!(a.top_line, 3);
}

/// Absolute line numbers shift whenever an estimated frozen height is
/// replaced by an exact one, so the (turn, row) anchor must be exact for every
/// line — including the blank separator between two turns. A separator that
/// re-derives to the row above it moves the view (and the cursor) up one line
/// per frame.
#[test]
fn the_content_anchor_round_trips_every_line() {
    let mut a = app();
    for _ in 0..4 {
        push_turn(&mut a);
    }
    a.layout.frozen_heights = vec![10, 10];
    a.layout.frozen_heights_estimated = vec![false, false];
    a.last_turn_height = 10;
    assert_eq!(a.turn_start_line(1), 11);
    let total = 10 + 10 + 10 + 3;
    for line in 0..total {
        a.pinned = false;
        a.log_off = line;
        a.top_line = line;
        a.remember_view_anchor();
        a.reanchor_view_top();
        assert_eq!(a.top_line, line, "line {line} did not survive its anchor");
    }
    // Line 10 is turn 0's separator: it belongs to that turn one row past its
    // last content row, so it follows the turn when the turn shrinks.
    a.pinned = false;
    a.log_off = 10;
    a.top_line = 10;
    a.remember_view_anchor();
    a.layout.frozen_heights[0] = 4;
    a.reanchor_view_top();
    assert_eq!(a.top_line, 4, "the separator follows the turn it ends");
}

/// The reported bug: after a resume, a Navigate keypress sits between ticks
/// that replace estimated heights with exact ones. The cursor is an absolute
/// line and the viewport follows it, so a step that moves the cursor takes the
/// whole transcript with it — with no input between the frames.
#[test]
fn nav_keystrokes_hold_the_content_while_estimates_settle() {
    let mut a = resumed_with_estimates(600);
    let _ = draw(&mut a);
    a.enter_nav();
    let mut ticks = 0;
    for _ in 0..8 {
        let before = a.nav_cursor;
        a.nav_move(-1);
        assert_eq!(
            a.nav_cursor,
            before - 1,
            "one press moves the cursor one line"
        );
        let (rows, _) = draw(&mut a);
        if a.remeasure_heights_step(16) {
            ticks += 1;
            let row = a.nav_cursor - a.log_off;
            let (after, _) = draw(&mut a);
            assert_eq!(
                after, rows,
                "convergence tick {ticks} moved the transcript with no input"
            );
            assert_eq!(
                a.nav_cursor - a.log_off,
                row,
                "convergence tick {ticks} moved the cursor off its screen row"
            );
        }
    }
    assert!(ticks > 3, "the test needs several convergence ticks");
}

/// The selection's free end is an absolute line too. Left behind by a settling
/// height, it highlights text the user never selected.
#[test]
fn the_selection_end_holds_its_content_while_estimates_settle() {
    let mut a = resumed_with_estimates(600);
    let _ = draw(&mut a);
    a.enter_nav();
    a.mode = Mode::Select;
    let anchor_turn = 590;
    let cursor_turn = 598;
    a.select_anchor = (a.turn_start_line(anchor_turn), 0);
    a.nav_cursor = a.turn_start_line(cursor_turn);
    a.sel = Some(a.select_sel());
    let _ = draw(&mut a);
    let mut ticks = 0;
    while a.remeasure_heights_step(16) {
        ticks += 1;
        let _ = draw(&mut a);
        assert_eq!(
            a.select_anchor.0,
            a.turn_start_line(anchor_turn),
            "tick {ticks} moved the selection's free end off its content"
        );
        assert_eq!(
            a.nav_cursor,
            a.turn_start_line(cursor_turn),
            "tick {ticks} moved the cursor off its content"
        );
    }
    assert!(ticks > 1, "the test needs several convergence ticks");
}

/// `[` and `]` move one turn. The jump reads `nav_cursor` and the turn
/// heights, and the tick loop replaces estimated heights between one frame and
/// the next, so a key that lands in that gap must still move one turn from the
/// turn the cursor is on — not from the absolute line of an older frame, which
/// the new heights point somewhere else entirely.
#[test]
fn a_turn_jump_moves_one_turn_while_estimates_settle() {
    let mut a = resumed_with_estimates(600);
    let _ = draw(&mut a);
    a.enter_nav();
    let _ = draw(&mut a);
    // Park the cursor mid-log, where a stale line and a converged one land on
    // different turns.
    a.nav_cursor = a.turn_start_line(300);
    let _ = draw(&mut a);
    let start = turn_of(&a, a.nav_cursor);
    assert_eq!(start, 300);

    // The heights the cursor and the turn starts were measured with are
    // replaced, and no frame runs before the key that follows.
    let mut ticks = 0;
    while a.remeasure_heights_step(16) {
        ticks += 1;
    }
    assert!(
        ticks > 0,
        "the fixture needs a height change before the press"
    );

    let mut run = None;
    handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
    assert_eq!(
        turn_of(&a, a.nav_cursor),
        start - 1,
        "the press must move one turn back from the turn the cursor is on"
    );
    let _ = draw(&mut a);
    assert_eq!(
        turn_of(&a, a.nav_cursor),
        start - 1,
        "the frame that followed moved the cursor off the turn it was sent to"
    );
    assert_eq!(a.nav_cursor, a.turn_start_line(start - 1));

    handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
    let _ = draw(&mut a);
    assert_eq!(
        turn_of(&a, a.nav_cursor),
        start,
        "`]` must return to the next turn's start"
    );
}

/// The turn holding `line`, walking the heights the drawing uses.
fn turn_of(a: &App, line: usize) -> usize {
    let mut cur = 0;
    for (i, _) in a.transcript.turns.iter().enumerate() {
        if line < a.turn_start_line(i) {
            break;
        }
        cur = i;
    }
    cur
}

/// The ticket: a submit from a scrolled-up view left the new content below the
/// viewport, so the user did not see what they had just submitted.
#[test]
fn a_submitted_prompt_is_on_screen_right_away() {
    let mut a = resumed_with_estimates(40);
    let _ = draw(&mut a);
    a.pinned = false;
    a.top_line = 0;
    let _ = draw(&mut a);
    assert!(
        a.log_off + a.log_view_h < a.log_total,
        "the fixture must start scrolled up"
    );

    a.set_input("submitted right now".to_string());
    let mut run = None;
    handle_event(&plain_key(KeyCode::Enter), &mut a, None, &mut run);

    let (rows, _) = draw(&mut a);
    assert!(
        rows.contains("submitted right now"),
        "the submitted prompt must be on screen:\n{rows}"
    );
}
