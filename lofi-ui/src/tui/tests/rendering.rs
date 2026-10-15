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
    let Block::Tool(exec) = &a.transcript.turns[0].blocks[0] else {
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

    let Block::Tool(tool) = &a.transcript.turns[0].blocks[0] else {
        panic!("expected tool block");
    };
    assert!(tool.done);
    assert!(tool.is_error);
    assert_eq!(tool.result.as_deref(), Some("Operation aborted"));
    assert!(tool.native[0].done);
    assert!(tool.native[0].is_error);
    assert!(matches!(
        a.transcript.turns[0].blocks.last(),
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
    let Block::Tool(exec) = &a.transcript.turns[0].blocks[0] else {
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
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
    a.transcript.turns.push(Turn {
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
    let turn = &a.transcript.turns[0];
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
    a.transcript.turns.push(Turn {
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);

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
    // Left quarter-block bar marker in muted for job notifications.
    assert_eq!(lines[0].line.spans[0].content, "▎ ");
    assert_eq!(lines[0].line.spans[0].style.fg, Some(a.theme.muted));
    // Body is muted and italic, not the regular user fg.
    let body = &lines[0].line.spans[1];
    assert_eq!(body.style.fg, Some(a.theme.muted));
    assert!(body.style.add_modifier.contains(Modifier::ITALIC));
    // Marker repeats on wrapped rows like user/agent messages.
    turn.prompt = "a long notice text that wraps to a second row easily".to_string();
    let lines = render_turn_lines(&cx, &turn);
    assert!(lines.iter().all(|line| {
        line.line.spans[0].content == "▎ " && line.line.spans[0].style.fg == Some(a.theme.muted)
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);

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
    a.transcript.turns.push(Turn {
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
    let collapsed = render_turn_lines(&cx, &a.transcript.turns[0]);
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
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
    a.transcript.turns.push(Turn {
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
    let collapsed = render_turn_lines(&cx, &a.transcript.turns[0]);
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
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
fn user_shell_nonzero_exit_is_visible_and_error_colored() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.transcript.turns.push(Turn {
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
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
    a.transcript.turns.push(Turn {
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].detail.is_none());
    let text = lines[0].line.to_string();
    assert!(text.contains("Compacted 12 messages · kept 3"));
    assert!(!text.contains("summary detail"));
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
    let lines = render_turn_lines(&cx, &a.transcript.turns[0]);
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
