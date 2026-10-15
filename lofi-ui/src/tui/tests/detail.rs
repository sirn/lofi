use super::*;

#[test]
fn log_scrollbar_thumb_tracks_scroll_input_ownership() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut a = app();
    for n in 0..40 {
        a.apply_event(AgentEvent::Prompt {
            kind: lofi_types::PromptKind::User,
            prompt: format!("prompt {n}"),
        });
        a.apply_event(AgentEvent::Text(format!("line {n}")));
    }

    let thumb_fg = |a: &mut App| {
        let mut term = Terminal::new(TestBackend::new(40, 16)).unwrap();
        term.draw(|f| crate::tui::view::render(f, a)).unwrap();
        let buf = term.backend().buffer();
        (0..16u16)
            .filter(|y| buf[(39, *y)].symbol() == "┃")
            .map(|y| buf[(39, y)].fg)
            .next()
    };
    assert_eq!(
        thumb_fg(&mut a),
        Some(a.theme.muted),
        "Input mode: the transcript scrollbar is passive"
    );
    a.mode = Mode::Navigate;
    assert_eq!(
        thumb_fg(&mut a),
        Some(a.theme.user),
        "Navigate mode: the transcript scrollbar owns scroll input"
    );
}

#[test]
fn focused_user_shell_detail_accentuates_its_scrollbar() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.transcript.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::UserShell {
            id: 0,
            command: "seq 1 30".to_string(),
            output: (1..=30)
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
            exit_code: Some(0),
            signal: None,
            duration: Duration::from_millis(1),
            truncated: false,
            cancelled: false,
            running: false,
            exclude_from_context: false,
        }],
    });
    let key = DetailKey::UserShell(0);
    a.expanded_details.insert(
        key.clone(),
        DetailState {
            turn: 0,
            scroll: None,
        },
    );

    let thumb_fg = |a: &App| {
        let cx = Cx {
            app: a,
            theme: a.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, &a.transcript.turns[0])
            .iter()
            .flat_map(|line| line.line.spans.iter())
            .find(|span| span.content.as_ref() == "┃")
            .and_then(|span| span.style.fg)
    };

    assert_eq!(thumb_fg(&a), Some(a.theme.muted));
    a.detail_focus = Some(crate::tui::DetailFocus {
        key,
        cursor: 0,
        col: 0,
    });
    assert_eq!(thumb_fg(&a), Some(a.theme.user));
}

#[test]
fn short_detail_box_omits_the_scrollbar_line() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.transcript.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::UserShell {
            id: 0,
            command: "printf".to_string(),
            output: "one\ntwo".to_string(),
            exit_code: Some(0),
            signal: None,
            duration: Duration::from_millis(1),
            truncated: false,
            cancelled: false,
            running: false,
            exclude_from_context: false,
        }],
    });
    a.expanded_details.insert(
        DetailKey::UserShell(0),
        DetailState {
            turn: 0,
            scroll: None,
        },
    );

    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let spans = render_turn_lines(&cx, &a.transcript.turns[0])
        .iter()
        .flat_map(|line| line.line.spans.iter())
        .filter(|span| {
            let s = span.content.as_ref();
            s == "│" || s == "┃"
        })
        .count();
    assert_eq!(
        spans, 0,
        "a fully visible detail box draws no scrollbar line"
    );
}

#[test]
fn successful_exec_result_can_expand_when_it_has_distinct_content() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "exec-id".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "exec-id".to_string(),
        result: "distinct result".to_string(),
        is_error: false,
        elapsed_ms: 1,
    });
    let key = DetailKey::ExecResult(detail_block_id(0, 0));
    let render = |app: &App| {
        let cx = Cx {
            app,
            theme: app.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, &app.transcript.turns[0])
            .iter()
            .map(|line| line.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!render(&a).contains("distinct result"));
    a.expanded_details.insert(
        key,
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    assert!(render(&a).contains("distinct result"));
}

#[test]
fn unloaded_result_caret_is_stable_when_another_detail_expands() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "exec-id".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "exec-id".to_string(),
        code: "return 1".to_string(),
        label: None,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "exec-id".to_string(),
        result: "durable result".to_string(),
        is_error: false,
        elapsed_ms: 1,
    });
    let Block::Tool(tool) = &mut a.transcript.turns[0].blocks[0] else {
        unreachable!();
    };
    tool.result = None;

    let render = |app: &App| {
        let cx = Cx {
            app,
            theme: app.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, &app.transcript.turns[0])
            .iter()
            .map(|line| line.line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert!(!render(&a).contains("durable result"));
    a.expanded_details.insert(
        DetailKey::Exec(detail_block_id(0, 0)),
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    let expanded = render(&a);
    assert!(expanded.contains("return 1"));
    assert!(!expanded.contains("durable result"));
}

#[test]
fn expanded_detail_height_scales_to_its_content() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "adaptive".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "adaptive".to_string(),
        code: "short detail one\nshort detail two".to_string(),
        label: None,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "adaptive".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let id = detail_block_id(0, 0);
    let collapsed_cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let collapsed = render_turn_lines(&collapsed_cx, &a.transcript.turns[0]);
    assert_eq!(
        collapsed[2].detail.as_ref().map(|detail| detail.total),
        Some(0)
    );
    a.expanded_details.insert(
        DetailKey::Exec(id),
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
    assert_eq!(lines.len(), collapsed.len() + 2);
    assert_eq!(lines[2].detail.as_ref().map(|detail| detail.total), Some(2));
    assert!(lines[3].line.to_string().contains("short detail one"));
    assert!(lines[4].line.to_string().contains("short detail two"));
}

#[test]
fn expanded_detail_caps_at_ten_rows_and_keeps_a_right_margin() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    let body = (0..15)
        .map(|row| format!("margin-detail-{row:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    a.apply_event(AgentEvent::ToolStart {
        id: "bounded".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "bounded".to_string(),
        code: body,
        label: None,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "bounded".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let key = DetailKey::Exec(detail_block_id(0, 0));
    a.expanded_details.insert(
        key.clone(),
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 40,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
    let body_rows = lines
        .iter()
        .filter(|line| {
            line.detail
                .as_ref()
                .is_some_and(|target| target.key == key && target.row.is_some())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        body_rows.len(),
        10,
        "expected only the maximum detail window"
    );
    let first = &body_rows[0];
    assert!(first.line.to_string().contains("margin-detail-00"));
    assert!(!lines
        .iter()
        .any(|line| line.line.to_string().contains("margin-detail-10")));
    let used = first
        .line
        .spans
        .iter()
        .map(|span| crate::tui::view::width(span.content.as_ref()))
        .sum::<usize>();
    assert_eq!(used, 40);
    assert_eq!(
        first.line.spans.last().map(|span| span.content.as_ref()),
        Some(" ")
    );
    assert_eq!(
        first.line.spans.last().and_then(|span| span.style.bg),
        None,
        "the right margin stays outside the raised detail box"
    );
}

#[test]
fn repeated_provider_tool_ids_get_independent_detail_keys() {
    let events = [
        AgentEvent::Prompt {
            prompt: "first".to_string(),
            kind: lofi_types::PromptKind::User,
        },
        AgentEvent::ToolStart {
            id: "reused".to_string(),
            name: "exec".to_string(),
        },
        AgentEvent::Prompt {
            prompt: "second".to_string(),
            kind: lofi_types::PromptKind::User,
        },
        AgentEvent::ToolStart {
            id: "reused".to_string(),
            name: "exec".to_string(),
        },
    ];
    let mut turns = Vec::new();
    for event in events {
        apply_event_to_turns(&mut turns, event);
    }
    let Block::Tool(first) = &turns[0].blocks[0] else {
        panic!("first tool");
    };
    let Block::Tool(second) = &turns[1].blocks[0] else {
        panic!("second tool");
    };
    assert_ne!(first.detail_id, second.detail_id);
}

#[test]
fn native_tool_details_expand_independently_in_a_fixed_viewport() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    for (id, name, result) in [
        (
            1,
            "read",
            serde_json::json!({
                "content": "head one\nhead two\nhead three\nhead four\nhead five\nhead six\nhead seven\nhead eight\nhead nine\nhead ten\nhead eleven\nhead twelve",
                "start_line": 1,
                "total_lines": 12,
                "truncated": false
            })
            .to_string(),
        ),
        (
            2,
            "bash",
            serde_json::json!({
                "output": "tail one\ntail two\ntail three\ntail four\ntail five\ntail six",
                "code": 0
            })
            .to_string(),
        ),
    ] {
        a.apply_event(AgentEvent::NativeToolStart {
            parent: "e1".to_string(),
            id,
            name: name.to_string(),
            args: String::new(),
        });
        a.apply_event(AgentEvent::NativeToolEnd {
            parent: "e1".to_string(),
            id,
            result,
            is_error: false,
        });
    }
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: String::new(),
        is_error: false,
        elapsed_ms: 1,
    });

    let render = |app: &App| {
        let cx = Cx {
            app,
            theme: app.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, &app.transcript.turns[0])
            .iter()
            .map(|line| {
                line.line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
    };
    let collapsed = render(&a).join("\n");
    assert!(!collapsed.contains("head one"));
    assert!(!collapsed.contains("tail six"));

    a.expanded_details.insert(
        DetailKey::NativeTool { parent: 0, id: 1 },
        DetailState {
            turn: 0,
            scroll: None,
        },
    );
    let expanded = render(&a);
    let text = expanded.join("\n");
    assert!(text.contains("head one"));
    assert!(
        text.contains("head six"),
        "the ten-row head view keeps row six visible"
    );
    assert!(
        !text.contains("head eleven"),
        "the ten-row head view stops before row eleven"
    );
    assert!(!text.contains("tail six"), "bash sibling remains collapsed");
    assert_eq!(
        expanded
            .iter()
            .filter(|line| line.contains("head "))
            .count(),
        10
    );
}
