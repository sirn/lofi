use super::*;

#[test]
fn compact_thresholds_use_the_models_actual_small_context_window() {
    let mut config = lofi_types::CompactionConfig::default();
    config.auto.context_ratio = Some(0.5);
    config.reserved_context_tokens = 20_000;
    let a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        ServiceTier::Auto,
        100_000,
        config,
        String::new(),
    );

    assert_eq!(a.lifecycle.compact_budget(), 25_000);
}

#[test]
fn failed_exec_settles_pending_native_tools() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".into(),
        name: "exec".into(),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".into(),
        id: 0,
        name: "agent".into(),
        args: "inspect".into(),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".into(),
        result: "sandbox error: timed out".into(),
        is_error: true,
        elapsed_ms: 1,
    });
    let Block::Tool(exec) = &a.turns[0].blocks[0] else {
        panic!("expected exec")
    };
    assert!(exec.native[0].done);
    assert!(exec.native[0].is_error);
}

#[test]
fn cancelled_turn_settles_open_tool_rows() {
    let mut a = app();
    a.apply_event(AgentEvent::Prompt {
        kind: lofi_types::PromptKind::User,
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::ToolStart {
        id: "exec-1".into(),
        name: "exec".into(),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "exec-1".into(),
        id: 1,
        name: "bash".into(),
        args: "sleep 10".into(),
    });
    a.apply_event(AgentEvent::TurnCancelled {
        model: "p/m".into(),
        elapsed_ms: 50,
        cost: 0.0,
        usage: Usage::default(),
    });

    let Block::Tool(tool) = &a.turns[0].blocks[0] else {
        panic!("expected tool block");
    };
    assert!(tool.done);
    assert!(tool.is_error);
    assert_eq!(tool.result.as_deref(), Some("Operation aborted"));
    assert!(tool.native[0].done);
    assert!(tool.native[0].is_error);
    assert!(matches!(
        a.turns[0].blocks.last(),
        Some(Block::TurnCancelled { .. })
    ));
}

#[test]
fn native_tool_events_nest_under_their_exec() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.bash('ls')".to_string(),
        label: Some("list files".to_string()),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "bash".to_string(),
        args: "ls".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 0,
        result: "file.txt".to_string(),
        is_error: false,
    });
    let Block::Tool(exec) = &a.turns[0].blocks[0] else {
        panic!("expected an exec tool block");
    };
    assert_eq!(exec.name, "exec");
    assert_eq!(exec.label.as_deref(), Some("list files"));
    assert_eq!(exec.input, "lofi.bash('ls')");
    assert_eq!(exec.native.len(), 1);
    let nt = &exec.native[0];
    assert_eq!(nt.name, "bash");
    assert_eq!(nt.args, "ls");
    assert!(nt.done);
    assert_eq!(nt.result.as_deref(), Some("file.txt"));
    assert!(nt.preview.is_some());
    assert!(!nt.is_error);
}

#[test]
fn cached_native_notice_remains_expandable() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "jobRead".to_string(),
        args: "{}".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 0,
        result: serde_json::json!({
            "output": "",
            "cursor": 0,
            "totalBytes": 10,
            "done": false,
        })
        .to_string(),
        is_error: false,
    });

    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: true,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);
    let header = lines
        .iter()
        .find(|line| line.line.to_string().contains("Tool jobRead"))
        .expect("native tool header");
    assert!(header.detail.is_some());
}

#[test]
fn rich_header_suffix_for_read_and_bash() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::Tool(ToolCall {
            detail_id: 0,
            id: "e1".to_string(),
            name: "exec".to_string(),
            input: String::new(),
            label: None,
            native: vec![
                NativeTool {
                    id: 0,
                    name: "read".to_string(),
                    args: "TODO.md".to_string(),
                    result: Some(
                        serde_json::json!({
                            "ok": true,
                            "content": "line one\nline two\nline three",
                            "start_line": 20,
                            "total_lines": 100,
                            "truncated": false,
                        })
                        .to_string(),
                    ),
                    preview: None,
                    is_error: false,
                    done: true,
                },
                NativeTool {
                    id: 1,
                    name: "bash".to_string(),
                    args: "ls -alh".to_string(),
                    result: Some(
                        serde_json::json!({
                            "ok": true,
                            "output": "file.txt",
                            "code": 0,
                            "duration_ms": 1500,
                            "status": "exited",
                        })
                        .to_string(),
                    ),
                    preview: None,
                    is_error: false,
                    done: true,
                },
            ],
            result: Some("{\"value\":null}".to_string()),
            result_availability: ResultAvailability::Available,
            result_committed: false,
            is_error: false,
            done: true,
            elapsed: None,
        })],
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let body: String = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        body.contains("(lines 20-22)"),
        "read header should show line range: {body}"
    );
    assert!(
        body.contains("(took 1.5s)"),
        "bash header should show duration: {body}"
    );
}

#[test]
fn user_message_uses_full_height_rail_without_tile_or_padding() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: "A long user prompt\nwith another line".to_string(),
        blocks: Vec::new(),
    });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 32,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);

    assert_eq!(lines.len(), 2, "only the two message body rows");
    // The rail spans every row so the message stays visually grouped; blank
    // continuation gutters were making multi-line prompts look disjoint.
    assert_eq!(lines[0].line.spans[0].content, "▌ ");
    assert_eq!(lines[0].line.spans[0].style.fg, Some(a.theme.user));
    assert_eq!(lines[1].line.spans[0].content, "▌ ");
    assert_eq!(lines[1].line.spans[0].style.fg, Some(a.theme.user));
    for line in &lines {
        assert!(
            line.line.spans.iter().all(|span| span.style.bg.is_none()),
            "user message has no tile background: {:?}",
            line.line
        );
    }
    let first: String = lines[0]
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    let second: String = lines[1]
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(first.starts_with("▌ A long user prompt"));
    assert!(second.starts_with("▌ with another line"));
}

#[test]
fn notice_message_uses_muted_bar_marker_and_italic_muted_body() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    use ratatui::style::Modifier;

    let a = app();
    let mut turn = Turn {
        kind: lofi_types::PromptKind::Notice,
        prompt: "job 1786 completed: sleep 1 (exit 0)".to_string(),
        blocks: Vec::new(),
    };
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 60,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &turn);
    assert!(!lines.is_empty());
    // Bar marker in muted, matching user/agent message structure.
    assert_eq!(lines[0].line.spans[0].content, "▌ ");
    assert_eq!(lines[0].line.spans[0].style.fg, Some(a.theme.muted));
    // Body is muted and italic, not the regular user fg.
    let body = &lines[0].line.spans[1];
    assert_eq!(body.style.fg, Some(a.theme.muted));
    assert!(body.style.add_modifier.contains(Modifier::ITALIC));
    // Marker repeats on wrapped rows like user/agent messages.
    turn.prompt = "a long notice text that wraps to a second row easily".to_string();
    let lines = render_turn_lines(&cx, &turn);
    assert!(lines.iter().all(|line| {
        line.line.spans[0].content == "▌ " && line.line.spans[0].style.fg == Some(a.theme.muted)
    }));
}

#[test]
fn agent_response_uses_agent_rail_but_thinking_and_tools_do_not() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Thinking("Private thought".to_string()));
    a.apply_event(AgentEvent::Text(
        "Agent response\nwith a second row".to_string(),
    ));
    a.apply_event(AgentEvent::ToolStart {
        id: "t1".to_string(),
        name: "search".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "t1".to_string(),
        code: "query".to_string(),
        label: None,
    });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 40,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);

    let response: Vec<_> = lines
        .iter()
        .filter(|line| {
            line.raw.as_ref().is_some_and(|raw| {
                raw.source.contains("Agent response") || raw.source.contains("second row")
            })
        })
        .collect();
    assert_eq!(response.len(), 2);
    for line in response {
        assert_eq!(line.line.spans[0].content, "▌ ");
        assert_eq!(line.line.spans[0].style.fg, Some(a.theme.agent));
    }

    let rendered = |line: &view::RenderLine| -> String {
        line.line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    };
    let thinking = lines
        .iter()
        .find(|line| rendered(line).contains("Thought for"))
        .expect("collapsed thinking row");
    assert_ne!(thinking.line.spans[0].content, "▌ ");
    let tool = lines
        .iter()
        .find(|line| rendered(line).contains("search"))
        .expect("tool row");
    assert_ne!(tool.line.spans[0].content, "▌ ");
}

#[test]
fn exec_keeps_left_gutter_without_tile_or_vertical_padding() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: Vec::new(),
    });
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.bash({ cmd: 'true' })".to_string(),
        label: Some("check".to_string()),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let collapsed = render_turn_lines(&cx, &a.turns[0]);
    assert_eq!(collapsed.len(), 2, "header and status start collapsed");
    assert!(collapsed[0].detail.is_some());
    a.expanded_details.insert(
        DetailKey::Exec(0),
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
    let lines = render_turn_lines(&cx, &a.turns[0]);
    assert!(lines
        .iter()
        .any(|line| line.line.to_string().contains("lofi.bash")));
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line.line.spans[0].content, "  ", "two-cell left gutter");
        if index == 0 || index + 1 == lines.len() {
            assert!(line.line.spans.iter().all(|span| span.style.bg.is_none()));
        } else {
            let spans = &line.line.spans;
            let (margin, body) = spans.split_last().expect("detail row has a margin");
            assert_eq!(margin.content.as_ref(), " ");
            assert!(margin.style.bg.is_none(), "margin stays plain");
            assert!(body
                .iter()
                .skip(2)
                .all(|span| span.style.bg == Some(a.theme.surface)));
        }
    }
}

#[test]
fn user_shell_renders_as_shell_tree_with_exit_status() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::UserShell {
            id: 0,
            command: "ps".to_string(),
            output: "PID TTY\n42 pts/3".to_string(),
            exit_code: Some(0),
            signal: None,
            duration: Duration::from_millis(1_100),
            truncated: false,
            cancelled: false,
            running: false,
            exclude_from_context: false,
        }],
    });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let collapsed = render_turn_lines(&cx, &a.turns[0]);
    assert_eq!(collapsed.len(), 2);
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
    let lines = render_turn_lines(&cx, &a.turns[0]);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();

    let text = rendered.join("\n");
    assert!(text.contains("PID TTY"));
    assert!(text.contains("42 pts/3"));
    assert!(text.contains("Exit 0, took 1.1s"));
    assert_eq!(lines[0].line.spans[1].content, "$ ");
    assert_eq!(lines[0].line.spans[1].style.fg, Some(a.theme.success));
    assert_eq!(lines[0].line.spans[2].style.fg, Some(a.theme.fg));
    assert_eq!(lines[1].line.spans[2].style.fg, Some(a.theme.success));
}

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
    a.turns.push(Turn {
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
        render_turn_lines(&cx, &a.turns[0])
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
    a.turns.push(Turn {
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
    let spans = render_turn_lines(&cx, &a.turns[0])
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
fn user_shell_nonzero_exit_is_visible_and_error_colored() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::UserShell {
            id: 0,
            command: "false".to_string(),
            output: "failed".to_string(),
            exit_code: Some(7),
            signal: None,
            duration: Duration::from_millis(900),
            truncated: false,
            cancelled: false,
            running: false,
            exclude_from_context: false,
        }],
    });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);
    let status: String = lines[1]
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert_eq!(status, "  └ ✗ Exit 7, took 0.9s");
    assert_eq!(lines[0].line.spans[1].style.fg, Some(a.theme.success));
    assert_eq!(lines[0].line.spans[2].style.fg, Some(a.theme.fg));
    assert_eq!(lines[1].line.spans[2].style.fg, Some(a.theme.error));
}

#[test]
fn compaction_renders_as_a_marker_without_expansion() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        kind: lofi_types::PromptKind::User,
        prompt: String::new(),
        blocks: vec![Block::Compaction {
            summarized: 12,
            kept: 3,
            summary: "summary detail".to_string(),
        }],
    });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].detail.is_none());
    let text = lines[0].line.to_string();
    assert!(text.contains("Compacted 12 messages · kept 3"));
    assert!(!text.contains("summary detail"));
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
        render_turn_lines(&cx, &app.turns[0])
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
    let Block::Tool(tool) = &mut a.turns[0].blocks[0] else {
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
        render_turn_lines(&cx, &app.turns[0])
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
    let collapsed = render_turn_lines(&collapsed_cx, &a.turns[0]);
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
    let lines = render_turn_lines(&cx, &a.turns[0]);
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
    let lines = render_turn_lines(&cx, &a.turns[0]);
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
fn thinking_renders_inline_without_expansion() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    use ratatui::style::{Color, Modifier};

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Thinking("styled thinking".to_string()));
    a.apply_event(AgentEvent::ThinkingEnd { elapsed_ms: 10 });
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);
    let detail = lines
        .iter()
        .find(|line| line.line.to_string().contains("styled thinking"))
        .expect("thinking transcript row");
    assert!(detail.detail.is_none());
    let text = detail
        .line
        .spans
        .iter()
        .find(|span| span.content == "styled thinking")
        .expect("thinking span");
    assert_eq!(text.style.fg, Some(Color::Indexed(244)));
    assert_eq!(text.style.bg, None);
    assert!(text.style.add_modifier.contains(Modifier::ITALIC));
    assert!(!lines.iter().any(|line| line.line.to_string().contains('▸')));
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
fn render_tree_smoke() {
    use crate::tui::view::blocks::render_turns;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text("Hello, this is Lofi.".to_string()));
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "const x = 1;\nlofi.read('a.txt')".to_string(),
        label: Some("read a".to_string()),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "read".to_string(),
        args: "a.txt".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 0,
        result: "line one\nline two".to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    // Turn is done (run not active): must not panic and must render.
    let text = render_turns(&a, 80);
    assert!(!text.lines.is_empty());
}

fn render_text_spans(markdown: &str) -> Vec<ratatui::text::Span<'static>> {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(markdown.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    render_turn_lines(&cx, turn)
        .into_iter()
        .flat_map(|rl| rl.line.spans)
        .skip_while(|s| s.content == "  ")
        .collect()
}

#[test]
fn inline_markdown_bold_italic() {
    use ratatui::style::Modifier;
    let spans = render_text_spans("**bold** *italic* _also_italic_");
    let bold = spans
        .iter()
        .find(|s| s.content == "bold")
        .expect("bold span");
    assert!(bold.style.add_modifier == Modifier::BOLD, "bold: {bold:?}");
    for content in ["italic", "also_italic"] {
        let span = spans
            .iter()
            .find(|s| s.content == content)
            .unwrap_or_else(|| panic!("{content} span"));
        assert!(
            span.style.add_modifier == Modifier::ITALIC,
            "{content}: {span:?}"
        );
    }
}

#[test]
fn inline_markdown_link_strips_syntax_and_records_target() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    use ratatui::style::Modifier;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(
        "Visit [Lofi](https://example.com/docs?q=1) now".to_string(),
    ));
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let line = render_turn_lines(&cx, &a.turns[0])
        .into_iter()
        .find(|line| !line.links.is_empty())
        .expect("linked line");
    let rendered: String = line
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert_eq!(rendered, "▌ Visit Lofi now");
    assert!(line.line.spans.iter().any(
        |span| span.content == "Lofi" && span.style.add_modifier.contains(Modifier::UNDERLINED)
    ));
    assert_eq!(line.links.len(), 1);
    let label: String = rendered
        .chars()
        .skip(line.links[0].start)
        .take(line.links[0].end - line.links[0].start)
        .collect();
    assert_eq!(label, "Lofi");
    assert_eq!(&*line.links[0].url, "https://example.com/docs?q=1");
}

#[test]
fn inline_markdown_link_wraps_and_preserves_each_row_target() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(
        "[alpha beta gamma delta](https://example.com)".to_string(),
    ));
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 13,
        active_turn: false,
    };
    let linked: Vec<_> = render_turn_lines(&cx, &a.turns[0])
        .into_iter()
        .filter(|line| !line.links.is_empty())
        .collect();

    assert!(linked.len() > 1, "link should wrap over multiple rows");
    for line in linked {
        assert_eq!(line.links.len(), 1);
        assert_eq!(&*line.links[0].url, "https://example.com");
    }
}

#[test]
fn inline_markdown_table_records_links() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let md = "| Site |
|------|
| [Lofi](https://example.com) |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let line = render_turn_lines(&cx, &a.turns[0])
        .into_iter()
        .find(|line| !line.links.is_empty())
        .expect("linked table cell");
    let rendered: String = line
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    let label: String = rendered
        .chars()
        .skip(line.links[0].start)
        .take(line.links[0].end - line.links[0].start)
        .collect();
    assert_eq!(label, "Lofi");
    assert_eq!(&*line.links[0].url, "https://example.com");
}

#[test]
fn inline_markdown_code_stays_literal() {
    let spans = render_text_spans("use `inline_spans` here");
    let code = spans
        .iter()
        .find(|s| s.content == "inline_spans")
        .expect("code span");
    assert!(code.style.bg.is_some(), "code should have bg: {code:?}");
}

#[test]
fn inline_markdown_code_wraps_across_lines() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "Run `git rebase --interactive upstream main` now";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 30,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let spans: Vec<_> = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter().cloned())
        .collect();
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        !body.contains('`'),
        "backtick markers should be stripped: {body}"
    );
    for word in ["git", "rebase", "interactive", "upstream", "main"] {
        let found = spans
            .iter()
            .any(|s| s.content.contains(word) && s.style.bg.is_some());
        assert!(
            found,
            "word {word:?} should be in a code-styled span: {body}"
        );
    }
}

#[test]
fn inline_markdown_underscore_not_inword() {
    use ratatui::style::Modifier;
    // Identifiers with underscores must NOT be parsed as emphasis.
    let spans = render_text_spans("call my_var_name here");
    let var = spans
        .iter()
        .find(|s| s.content.contains("my_var_name"))
        .expect("var span");
    assert_eq!(
        var.style.add_modifier,
        Modifier::empty(),
        "no emphasis: {var:?}"
    );
    assert!(
        !var.content.contains("**") && !var.content.contains("__"),
        "underscores should be literal: {var:?}"
    );
}

#[test]
fn inline_markdown_nested_bold_italic() {
    use ratatui::style::Modifier;
    let spans = render_text_spans("**bold *italic* bold**");
    let inner = spans
        .iter()
        .find(|s| s.content == "italic")
        .expect("nested italic");
    assert!(
        inner
            .style
            .add_modifier
            .contains(Modifier::BOLD | Modifier::ITALIC),
        "nested should be bold+italic: {inner:?}"
    );
}

#[test]
fn raw_map_snaps_to_markers_for_bold() {
    // Rendering `**bold** text` attaches a raw map that snaps display
    // positions past the stripped `**` markers to the source, so a
    // whole-row yank recovers `**bold** text` and a partial selection of
    // the visible "bold" yields the raw `**bold**`.
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "**bold** text";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let rl = rls
        .iter()
        .find(|r| {
            r.raw
                .as_ref()
                .is_some_and(|r| r.source.contains("**bold**"))
        })
        .expect("a line with a raw map");
    let raw = rl.raw.as_ref().unwrap();
    let start = *raw.map.first().unwrap();
    let end = *raw.map.last().unwrap();
    assert_eq!(&raw.source[start..end], "**bold** text");
    let s = raw.map[0];
    let e = raw.map[4];
    assert_eq!(&raw.source[s..e], "**bold**");
}

fn feed_lines(a: &mut App, rls: &[view::RenderLine]) {
    a.log_off = 0;
    a.log_vis = rls
        .iter()
        .map(|rl| view::VisLine {
            rendered: rl.line.spans.iter().map(|s| s.content.as_ref()).collect(),
            content: rl.content,
            raw: rl.raw.clone(),
        })
        .collect();
}

#[test]
fn yank_heading_includes_prefix() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "## Hello World";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let row = a
        .log_vis
        .iter()
        .position(|v| v.raw.as_ref().is_some_and(|r| r.source.contains("##")))
        .expect("heading row");
    a.nav_cursor = row;
    assert_eq!(a.current_line_text().as_deref(), Some("## Hello World"));
}

#[test]
fn yank_blockquote_includes_prefix() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "> A quoted line";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let row = a
        .log_vis
        .iter()
        .position(|v| v.raw.as_ref().is_some_and(|r| r.source.starts_with('>')))
        .expect("blockquote row");
    a.nav_cursor = row;
    assert_eq!(a.current_line_text().as_deref(), Some("> A quoted line"));
}

#[test]
fn yank_table_row_returns_markdown() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "| Name | Age |\n|------|-----|\n| Ada | 36 |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let hdr = a
        .log_vis
        .iter()
        .position(|v| {
            v.raw
                .as_ref()
                .is_some_and(|r| r.source.starts_with("| Name"))
        })
        .expect("header data row");
    a.nav_cursor = hdr;
    assert_eq!(a.current_line_text().as_deref(), Some("| Name | Age |"));
    let data = a
        .log_vis
        .iter()
        .position(|v| {
            v.raw
                .as_ref()
                .is_some_and(|r| r.source.starts_with("| Ada"))
        })
        .expect("data row");
    a.nav_cursor = data;
    assert_eq!(a.current_line_text().as_deref(), Some("| Ada | 36 |"));
}

#[test]
fn yank_table_header_separator_returns_markdown() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "| Name | Age |\n|------|-----|\n| Ada | 36 |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let sep = a
        .log_vis
        .iter()
        .position(|v| {
            v.raw
                .as_ref()
                .is_some_and(|r| r.source.starts_with("|---") || r.source.starts_with("|--"))
        })
        .expect("separator border row");
    a.nav_cursor = sep;
    assert_eq!(a.current_line_text().as_deref(), Some("|------|-----|"));
}

#[test]
fn yank_table_border_returns_empty() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "| Name | Age |\n|------|-----|\n| Ada | 36 |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let border = a
        .log_vis
        .iter()
        .position(|v| v.raw.as_ref().is_some_and(|r| r.source.is_empty()))
        .expect("border row");
    a.nav_cursor = border;
    assert_eq!(a.current_line_text(), None);
}

#[test]
fn yank_nested_list_preserves_indent() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "- Item A\n- Item B\n  - Nested item\n- Item C";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let nested = a
        .log_vis
        .iter()
        .position(|v| v.raw.as_ref().is_some_and(|r| r.source.starts_with("  -")))
        .expect("nested item row");
    a.nav_cursor = nested;
    assert_eq!(a.current_line_text().as_deref(), Some("  - Nested item"));
}

#[test]
fn yank_code_block_preserves_indent() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "```ts\nfunction greet() {\n  return 42;\n}\n```";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    feed_lines(&mut a, &rls);
    let indented = a
        .log_vis
        .iter()
        .position(|v| {
            v.raw
                .as_ref()
                .is_some_and(|r| r.source.starts_with("  return"))
        })
        .expect("indented code line");
    a.nav_cursor = indented;
    assert_eq!(a.current_line_text().as_deref(), Some("  return 42;"));
}

#[test]
fn selection_blockquote_to_text_preserves_blank() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "> Quote line.\n\nNormal text after.";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    for (i, rl) in rls.iter().enumerate() {
        let r: String = rl.line.spans.iter().map(|s| s.content.as_ref()).collect();
        eprintln!(
            "line {}: rendered={:?} content={:?} raw={:?} raw_src={:?} hard_break={:?}",
            i,
            r,
            rl.content,
            rl.raw.is_some(),
            rl.raw.as_ref().map(|r| r.source.as_ref()),
            rl.raw.as_ref().map(|r| r.hard_break)
        );
    }
    let n = rls.len();
    feed_lines(&mut a, &rls);
    a.sel = Some(Selection {
        start: (2, 0),
        end: (n - 1, a.log_vis[n - 1].rendered.chars().count()),
    });
    let text = a.selection_text().expect("selection text");
    assert_eq!(text, "> Quote line.\n\nNormal text after.");
}

#[test]
fn blockquote_renders_with_bar_and_empty_lines() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "> Line one\n>\n> Line three";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let quote_lines: Vec<String> = rls
        .iter()
        .skip(2)
        .map(|rl| rl.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert!(
        quote_lines
            .iter()
            .any(|s| s.contains("Line one") && s.contains("▎")),
        "Line one should have bar: {quote_lines:?}"
    );
    assert!(
        quote_lines
            .iter()
            .any(|s| s.trim_start_matches("▌ ").trim() == "▎"),
        "Empty quote line should render bar only: {quote_lines:?}"
    );
    assert!(
        quote_lines
            .iter()
            .any(|s| s.contains("Line three") && s.contains("▎")),
        "Line three should have bar: {quote_lines:?}"
    );
    let n = rls.len();
    feed_lines(&mut a, &rls);
    a.sel = Some(Selection {
        start: (2, 0),
        end: (n - 1, a.log_vis[n - 1].rendered.chars().count()),
    });
    let text = a.selection_text().expect("selection text");
    assert_eq!(text, "> Line one\n>\n> Line three");
}

#[test]
fn selection_blank_line_between_paragraphs_preserved() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "First paragraph.\n\nSecond paragraph.";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let n = rls.len();
    feed_lines(&mut a, &rls);
    a.sel = Some(Selection {
        start: (2, 0),
        end: (n - 1, a.log_vis[n - 1].rendered.chars().count()),
    });
    let text = a.selection_text().expect("selection text");
    assert_eq!(text, "First paragraph.\n\nSecond paragraph.");
}

#[test]
fn selection_nested_list_preserves_indent() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "- Item A\n- Item B\n  - Nested item\n- Item C";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let n = rls.len();
    feed_lines(&mut a, &rls);
    a.sel = Some(Selection {
        start: (0, 0),
        end: (n - 1, a.log_vis[n - 1].rendered.chars().count()),
    });
    let text = a.selection_text().expect("selection text");
    assert!(
        text.contains("  - Nested item"),
        "should preserve indent: {text}"
    );
}

#[test]
fn selection_table_returns_markdown_not_grid() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let md = "| Name | Age |\n|------|-----|\n| Ada | 36 |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 120,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    let n = rls.len();
    feed_lines(&mut a, &rls);
    a.sel = Some(Selection {
        start: (2, 0),
        end: (n - 1, a.log_vis[n - 1].rendered.chars().count()),
    });
    let text = a.selection_text().expect("selection text");
    assert_eq!(text, "| Name | Age |\n|------|-----|\n| Ada | 36 |");
    assert!(
        !text.contains('│') && !text.contains('─'),
        "no grid chars: {text}"
    );
}

#[test]
fn inline_markdown_header_all_levels_bold() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    use ratatui::style::Modifier;
    for md in [
        "# H1",
        "## H2",
        "### H3",
        "#### H4",
        "##### H5",
        "###### H6",
    ] {
        let mut a = app();
        push_turn(&mut a);
        a.apply_event(AgentEvent::Text(md.to_string()));
        let turn = &a.turns[0];
        let cx = Cx {
            app: &a,
            theme: a.theme,
            width: 120,
            active_turn: false,
        };
        let rls = render_turn_lines(&cx, turn);
        let all_spans: Vec<_> = rls.iter().flat_map(|rl| rl.line.spans.iter()).collect();
        assert!(
            all_spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD)),
            "header should be bold: {md} (spans: {all_spans:?})",
        );
    }
}

#[test]
fn inline_markdown_table_renders_borders() {
    let md = "| Name | Value |\n|------|------:|\n| foo  | 1    |\n| bar  | 22   |";
    let spans = render_text_spans(md);
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(body.contains('┌'), "top border: {body}");
    assert!(body.contains('┬'), "top mid: {body}");
    assert!(body.contains('┐'), "top right: {body}");
    assert!(body.contains('├'), "header sep: {body}");
    assert!(body.contains('┼'), "header mid: {body}");
    assert!(body.contains('┤'), "header right: {body}");
    assert!(body.contains('└'), "bottom border: {body}");
    assert!(body.contains('┴'), "bottom mid: {body}");
    assert!(body.contains('┘'), "bottom right: {body}");
    assert!(body.contains('│'), "vertical border: {body}");
    assert!(body.contains("Name"), "header cell: {body}");
    assert!(body.contains("Value"), "header cell: {body}");
    assert!(body.contains("foo"), "data cell: {body}");
    assert!(body.contains("bar"), "data cell: {body}");
    assert!(body.contains('1'), "data cell: {body}");
    assert!(body.contains("22"), "data cell: {body}");
}

#[test]
fn inline_markdown_table_uses_terminal_width_for_medal_emoji() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    use unicode_width::UnicodeWidthStr;

    let md = "| # | Provider | Requests | Input Tokens | Output Tokens | Cached Tokens | **Total Tokens** | **Cost** |\n|---|----------|---------:|------------:|-------------:|-------------:|----------------:|---------:|\n| 🥇 | **lilac-sub** | 26,709 | 114,626,193 | 14,627,472 | 2,034,195,392 | **2,163,449,057** | **$578.54** |\n| 🥈 | **hyper** | 17,144 | 50,531,552 | 4,954,628 | 1,228,034,243 | **1,283,520,423** | **$215.72** |\n| 🥉 | **synthetic** | 4,030 | 10,104,845 | 1,644,837 | 262,834,368 | **274,584,050** | **$0.00** |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 140,
        active_turn: false,
    };
    let lines = render_turn_lines(&cx, &a.turns[0]);
    let rendered: Vec<String> = lines
        .iter()
        .map(|line| {
            line.line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    let top_width = rendered
        .iter()
        .find(|line| line.contains('┌'))
        .map(|line| line.as_str().width())
        .expect("table top border");

    for line in rendered.iter().filter(|line| {
        line.contains('│') || line.contains('┌') || line.contains('├') || line.contains('└')
    }) {
        assert_eq!(
            line.as_str().width(),
            top_width,
            "table row and border widths differ: {line:?}"
        );
    }
}

#[test]
fn inline_markdown_table_fits_narrow_width() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    // A 3-column table with long cells rendered at a narrow width must not
    // produce any line wider than the viewport, and must not truncate cell
    // content — it wraps instead.
    let md = "| File | Status | Details |\n|------|--------|---------|\n| src/main.rs | modified | added new function |\n| README.md | created | initial version |";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 40,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    assert!(!rls.is_empty(), "table should render");
    for rl in &rls {
        let w: usize = rl
            .line
            .spans
            .iter()
            .map(|s| s.content.chars().count())
            .sum();
        assert!(w <= 40, "line too wide ({w} > 40): {:?}", rl.line);
    }
    let body: String = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        body.contains("function"),
        "content should not be truncated: {body}"
    );
    assert!(
        body.contains("initial"),
        "content should not be truncated: {body}"
    );
    assert!(
        body.contains("version"),
        "content should not be truncated: {body}"
    );
    assert!(
        body.contains("src/mai") && body.contains("n.rs"),
        "long token should hard-break: {body}"
    );
    assert!(!body.contains('…'), "no ellipsis: {body}");
}

#[test]
fn inline_markdown_table_right_aligns() {
    let md = "| Item | Count |\n|------|------:|\n| a    | 1     |\n| bb   | 22    |";
    let spans = render_text_spans(md);
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(body.contains(" 1"), "right-aligned 1: {body}");
    assert!(body.contains("22"), "right-aligned 22: {body}");
}

#[test]
fn inline_markdown_table_renders_inline_formatting() {
    use ratatui::style::Modifier;
    let md = "| Name | Type |\n|------|------|\n| **bold** | `code` |";
    let spans = render_text_spans(md);
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        !body.contains("**"),
        "bold markers should be stripped: {body}"
    );
    assert!(
        !body.contains('`'),
        "code markers should be stripped: {body}"
    );
    assert!(body.contains("bold"), "bold text should be present: {body}");
    assert!(body.contains("code"), "code text should be present: {body}");
    let bold_span = spans
        .iter()
        .find(|s| s.content == "bold")
        .expect("bold span");
    assert!(
        bold_span.style.add_modifier.contains(Modifier::BOLD),
        "bold cell should be BOLD: {bold_span:?}"
    );
    let code_span = spans
        .iter()
        .find(|s| s.content == "code")
        .expect("code span");
    assert!(
        code_span.style.fg.is_some(),
        "code cell should have fg color: {code_span:?}"
    );
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
        render_turn_lines(&cx, &app.turns[0])
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

#[test]
fn current_line_text_excludes_decoration() {
    let mut a = app();
    push_turn(&mut a);
    a.log_off = 0;
    a.log_vis = vec![view::VisLine {
        rendered: "  hello world   ".to_string(),
        content: (2, 13),
        raw: None,
    }]; // "hello world"
    a.nav_cursor = 0;
    assert_eq!(a.current_line_text().as_deref(), Some("hello world"));
}

#[test]
fn vim_motions_move_within_content() {
    let mut a = app();
    push_turn(&mut a);
    a.log_off = 0;
    a.log_vis = vec![view::VisLine {
        rendered: "  aa bb cc".to_string(),
        content: (2, 10),
        raw: None,
    }]; // "aa bb cc"
    a.nav_cursor = 0;
    a.nav_col = 2;
    assert_eq!(a.first_nonblank_col(), 2);
    a.nav_set_col(usize::MAX);
    assert_eq!(a.nav_col, 9);
    a.nav_col = 2;
    assert_eq!(a.nav_word_target(WordMotion::NextStart { big: false }), 5);
    a.nav_col = 2;
    assert_eq!(a.nav_word_target(WordMotion::NextEnd { big: false }), 3);
    a.nav_col = 5;
    assert_eq!(a.nav_word_target(WordMotion::PrevStart { big: false }), 2);
}

#[test]
fn vim_word_motion_skips_punctuation() {
    let mut a = app();
    push_turn(&mut a);
    a.log_off = 0;
    a.log_vis = vec![view::VisLine {
        rendered: "  a.b c".to_string(),
        content: (2, 7),
        raw: None,
    }]; // "a.b c"
    a.nav_cursor = 0;
    a.nav_col = 2;
    assert_eq!(a.nav_word_target(WordMotion::NextStart { big: false }), 3);
    assert_eq!(a.nav_word_target(WordMotion::NextStart { big: true }), 6);
}

#[test]
fn empty_text_blocks_leave_no_gap() {
    use crate::tui::view::blocks::render_turns;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Thinking("Let me explore.".to_string()));
    // Reasoning models often emit a whitespace-only content block between
    // the reasoning trace and a tool call; it must not render as a gap.
    a.apply_event(AgentEvent::Text("\n".to_string()));
    a.apply_event(AgentEvent::Text("  \n  ".to_string()));
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "return 1;".to_string(),
        label: None,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":1}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let text = render_turns(&a, 80);
    let blanks: Vec<bool> = text
        .lines
        .iter()
        .map(|l| {
            let text_is_empty = l
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim()
                .is_empty();
            let is_raised_detail = l
                .spans
                .iter()
                .any(|span| span.style.bg == Some(a.theme.surface));
            text_is_empty && !is_raised_detail
        })
        .collect();
    let max_run = blanks
        .iter()
        .fold((0usize, 0usize), |(mx, cur), &b| {
            if b {
                (mx.max(cur + 1), cur + 1)
            } else {
                (mx, 0)
            }
        })
        .0;
    assert!(
        max_run <= 2,
        "max blank run {max_run}; lines:\n{:#?}",
        text.lines
    );
}
