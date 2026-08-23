use super::*;

/// Streamed text must keep all content that was visible in the prior frame.
#[test]
fn streaming_long_text_never_drops_visible_lines() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let area = term.backend().buffer().area;
        let buf = term.backend().buffer();
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    let mut a = app();
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    a.apply_event(AgentEvent::TurnStart {
        kind: lofi_types::PromptKind::User,
        prompt: "stream a long reply".to_string(),
    });
    // A thinking phase first, finalized before text streams: mirrors a real
    // reasoning turn where the "Thinking..." block precedes the body.
    a.apply_event(AgentEvent::Thinking("working through it".to_string()));
    a.apply_event(AgentEvent::ThinkingEnd { elapsed_ms: 1200 });

    // A long multi-paragraph body streamed in chunks.
    let full: String = (0..24)
        .map(|i| format!("paragraph {i} with enough words to wrap onto several visual rows in a narrow viewport"))
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut term = Terminal::new(TestBackend::new(50, 18)).unwrap();
    let mut prev: Vec<String> = Vec::new();
    let chunk = full.len() / 24;
    let mut idx = 0usize;
    while idx < full.len() {
        let end = (idx + chunk).min(full.len());
        let delta = full[idx..end].to_string();
        a.apply_event(AgentEvent::Text(delta));
        term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
        let cur = rows(&term);
        // Every wrapped body row visible last frame must still be visible
        // this frame; order may shift, presence must not.
        for line in &prev {
            let t = line.trim();
            if t.is_empty() || !t.starts_with("paragraph") {
                continue;
            }
            let still = cur.iter().any(|l| l.trim() == t);
            assert!(
                still,
                "line popped out between frames: {t:?}\nprev:\n{}\ncur:\n{}",
                prev.join("\n"),
                cur.join("\n")
            );
        }
        prev = cur;
        idx = end;
    }
    a.run = None;
}

/// `render_turn_height` must equal the number of rows `render_turn_lines`
/// (and a full-range `render_turn_window`) actually emit for the same turn at
/// the same width. The viewport total and per-turn skip logic are computed
/// from heights; any disagreement shifts every row below and is the "popping"
/// seen during streaming.
#[test]
fn turn_height_matches_emitted_line_count() {
    use crate::tui::view::blocks::{render_turn_height, render_turn_lines, render_turn_window};
    use crate::tui::view::component::Cx;

    let long_text: String = (0..20)
        .map(|i| format!("paragraph {i} with enough words to wrap onto several visual rows in a narrow viewport"))
        .collect::<Vec<_>>()
        .join("\n\n");

    for active in [false, true] {
        for w in [28usize, 50, 90, 120] {
            for has_thinking in [false, true] {
                let a = app();
                let mut blocks = Vec::new();
                if has_thinking {
                    blocks.push(Block::Thinking(ThinkingBlock {
                        text: "reasoning about the answer".to_string(),
                        start: Instant::now(),
                        elapsed: Some(Duration::from_millis(1200)),
                    }));
                }
                blocks.push(Block::Text(long_text.clone()));
                blocks.push(Block::Tool(ToolCall {
                    id: "exec-1".to_string(),
                    name: "exec".to_string(),
                    input: "await lofi.bash({ cmd: \"seq 1 40\" })".to_string(),
                    label: None,
                    native: vec![NativeTool {
                        id: 1,
                        name: "bash".to_string(),
                        args: "seq 1 40".to_string(),
                        result: Some(
                            (1..=40)
                                .map(|i| i.to_string())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        ),
                        preview: None,
                        is_error: false,
                        done: true,
                    }],
                    result: Some(
                        (1..=40)
                            .map(|i| i.to_string())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    result_committed: false,
                    is_error: false,
                    done: true,
                    elapsed: Some(Duration::from_millis(120)),
                }));
                let turn = Turn {
                    kind: lofi_types::PromptKind::User,
                    prompt: "a long conversation turn".to_string(),
                    blocks,
                };
                let theme = a.theme;
                let cx = Cx {
                    app: &a,
                    theme,
                    width: w,
                    active_turn: active,
                };
                let h = render_turn_height(&cx, &turn);
                let n_lines = render_turn_lines(&cx, &turn).len();
                assert_eq!(
                    h, n_lines,
                    "height {h} != emitted lines {n_lines} (w={w}, active={active}, thinking={has_thinking})"
                );
                let n_window = render_turn_window(&cx, &turn, 0..h).len();
                assert_eq!(
                    n_lines, n_window,
                    "full window {n_window} != lines {n_lines} (w={w}, active={active}, thinking={has_thinking})"
                );
            }
        }
    }
}

/// Streaming inline markdown must not reflow rows that already settled. A
/// still-growing line re-wraps only by the closing marker width (a backtick
/// or `**` vanishing when a span completes), never by re-flowing settled
/// words across rows — the mid-sentence "pop" seen while streaming.
fn assert_streaming_words_never_move_rows(words: &[&str], case: &str) {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let w = 40usize;
    let a = app();
    let theme = a.theme;
    let mut text = String::new();
    let mut prev_rows: Vec<String> = Vec::new();
    for (i, word) in words.iter().enumerate() {
        if i > 0 {
            text.push(' ');
        }
        text.push_str(word);
        let cx = Cx {
            app: &a,
            theme,
            width: w,
            active_turn: true,
        };
        let turn = Turn {
            kind: lofi_types::PromptKind::User,
            prompt: String::new(),
            blocks: vec![Block::Text(text.clone())],
        };
        let rows: Vec<String> = render_turn_lines(&cx, &turn)
            .iter()
            .map(|rl| {
                rl.line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Row count may only grow as text streams in; it must never shrink.
        assert!(
            rows.len() >= prev_rows.len(),
            "[{case}] row count shrank after word {i} ({word:?})\nprev:\n{}\ncur:\n{}",
            prev_rows.join("\n"),
            rows.join("\n")
        );
        // Words on a settled (non-final) row must survive into the next
        // frame; only marker characters may vanish, whole words may not move.
        let prev_settled_rows = prev_rows.len().saturating_sub(1);
        for r in 0..prev_settled_rows {
            let row_text = rows.concat();
            for wd in prev_rows[r]
                .split_whitespace()
                .filter(|wd| *wd != "\u{258C}")
            {
                let bare = wd.trim_matches(|c: char| "`*_~[]()#".contains(c));
                if bare.is_empty() {
                    continue;
                }
                assert!(
                    row_text.contains(bare),
                    "[{case}] settled word {bare:?} vanished after word {i} ({word:?})\nprev:\n{}\ncur:\n{}",
                    prev_rows.join("\n"),
                    rows.join("\n")
                );
            }
        }
        prev_rows = rows;
    }
}

#[test]
fn streaming_inline_code_does_not_rewrap_settled_rows() {
    // A code span `alpha beta gamma` opens partway through and closes several
    // words later, straddling a wrap boundary at width 40.
    assert_streaming_words_never_move_rows(
        &[
            "the", "quick", "brown", "fox", "jumps", "over", "`alpha", "beta", "gamma`", "and",
            "keeps", "running", "toward", "the", "lazy", "dog", "without", "stopping", "for",
            "anything", "at", "all", "today",
        ],
        "code",
    );
}

#[test]
fn streaming_inline_bold_does_not_rewrap_settled_rows() {
    assert_streaming_words_never_move_rows(
        &[
            "the", "quick", "brown", "fox", "jumps", "over", "**alpha", "beta", "gamma**", "and",
            "keeps", "running", "toward", "the", "lazy", "dog", "without", "stopping", "for",
            "anything", "at", "all", "today",
        ],
        "bold",
    );
}

#[test]
fn streaming_inline_link_does_not_rewrap_settled_rows() {
    // A link opens mid-line and its destination completes several words later.
    assert_streaming_words_never_move_rows(
        &[
            "the",
            "quick",
            "brown",
            "fox",
            "jumps",
            "over",
            "[alpha",
            "beta",
            "gamma](https://example.com)",
            "and",
            "keeps",
            "running",
            "toward",
            "the",
            "lazy",
            "dog",
            "without",
            "stopping",
            "for",
            "anything",
        ],
        "link",
    );
}

/// Regression: /compact must not collapse the transcript to the last user
/// prompt. A file-backed resumed turn renders its content lazily from disk;
/// appending the compaction marker to its empty live shell used to make that
/// lone block the turn's entire live content, so the renderer stopped
/// re-reading the response from disk until the next prompt re-froze the turn.
#[test]
fn compact_keeps_file_backed_turn_content_visible() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::SessionStore::new(dir.path().join("s"));
    let cursor = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap();
    let path = cursor.path().to_path_buf();

    let mut evs = vec![SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "prompt 1".to_string(),
            }],
            kind: PromptKind::default(),
        }),
    }];
    for i in 0..6 {
        evs.push(SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: format!("agent round {i}"),
                }],
                kind: PromptKind::default(),
            }),
        });
    }
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
    test_append_events(&path, &mut evs, None).unwrap();

    let mut a = app();
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
    let c0 = a.session.cursor.as_ref().unwrap().clone();
    let snap = c0.snapshot().unwrap();
    a.restore_indexed_session(&c0, &snap.index, snap.file_size)
        .unwrap();
    a.lifecycle.restore_history(&c0, &snap.index).unwrap();

    assert!(a.compact_now());

    // The turn must still materialize its full pre-compaction content, with
    // the compaction marker appended rather than replacing it.
    let rendered = a.materialize_turn(0);
    assert!(
        rendered
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Text(t) if t.contains("agent round"))),
        "compacted turn lost its assistant content: {:?}",
        rendered.blocks.len()
    );
    assert!(
        rendered
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Compaction { .. })),
        "compaction marker missing"
    );
}

#[test]
fn notify_lines_is_one_without_notification() {
    let a = app();
    assert_eq!(a.notify_lines(80), 1);
}

#[test]
fn notify_lines_grows_with_a_long_message_and_caps_at_max() {
    let mut a = app(); // mode INPUT, verbose off; " INPUT " is 7 cells
    a.notify(
        NotifyKind::Error,
        "quite a long error message that absolutely refuses to fit on a single line of a reasonably wide terminal".to_string(),
    );
    // Width 60: avail = 60-4-7 = 49 → more than one row for the message.
    let lines_60 = a.notify_lines(60);
    assert!(lines_60 >= 2, "expected wrapping, got {lines_60}");
    // Narrow enough to need more than the cap → stops at NOTIFY_MAX_LINES.
    assert_eq!(a.notify_lines(24), NOTIFY_MAX_LINES as u16);
}
