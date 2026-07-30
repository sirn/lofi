#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::float_cmp)]

use super::*;
use std::sync::Arc;

use lofi_types::{ContentBlock, Role, Usage};
use ratatui::backend::TestBackend;

fn app() -> App {
    App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        0,
        lofi_types::CompactionConfig::default(),
    )
}

fn push_turn(app: &mut App) {
    app.push_turn(Turn {
        prompt: "p".to_string(),
        blocks: Vec::new(),
    });
}

fn test_append_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent: Option<&str>,
) -> lofi_core::Result<(u64, u64)> {
    let cursor = match parent {
        Some(parent) => store::SessionCursor::new(path.to_path_buf(), Some(parent.to_string())),
        None => store::SessionCursor::open(path.to_path_buf())?,
    };
    cursor.append_events(events)
}

fn test_append_compaction(
    path: &Path,
    kept_messages: &[Message],
    parent: Option<&str>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: store::CompactionCounts,
) -> lofi_core::Result<(u64, u64, String)> {
    let cursor = match parent {
        Some(parent) => store::SessionCursor::new(path.to_path_buf(), Some(parent.to_string())),
        None => store::SessionCursor::open(path.to_path_buf())?,
    };
    let (start, end) =
        cursor.append_compaction(kept_messages, summary, summarized_range, counts)?;
    Ok((start, end, cursor.leaf_id().unwrap_or_default()))
}

#[test]
fn compact_thresholds_use_the_models_actual_small_context_window() {
    let mut config = lofi_types::CompactionConfig::default();
    config.auto.context_ratio = Some(0.5);
    config.reserved_context_tokens = 20_000;
    let a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        100_000,
        config,
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
    a.apply_event(AgentEvent::TurnStart {
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
    assert!(!nt.is_error);
}

#[test]
fn rich_header_suffix_for_read_and_bash() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    a.turns.push(Turn {
        prompt: String::new(),
        blocks: vec![Block::Tool(ToolCall {
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
    for line in &lines {
        assert_eq!(line.line.spans[0].content, "▌ ");
        assert_eq!(line.line.spans[0].style.fg, Some(a.theme.user));
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
        .find(|line| rendered(line).contains("Private thought"))
        .expect("thinking row");
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
    let lines = render_turn_lines(&cx, &a.turns[0]);

    assert_eq!(lines.len(), 3, "header, command, and status only");
    for line in &lines {
        assert_eq!(line.line.spans[0].content, "  ", "two-cell left gutter");
        assert!(
            line.line.spans.iter().all(|span| span.style.bg.is_none()),
            "exec has no background tile: {:?}",
            line.line
        );
    }
}

#[test]
fn user_bash_renders_as_shell_tree_with_exit_status() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        prompt: String::new(),
        blocks: vec![Block::UserBash {
            command: "ps".to_string(),
            output: "PID TTY\n42 pts/3".to_string(),
            exit_code: Some(0),
            signal: None,
            duration: Duration::from_millis(1_100),
            truncated: false,
            cancelled: false,
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

    assert_eq!(
        rendered,
        vec![
            "  $ ps",
            "  │ PID TTY",
            "  │ 42 pts/3",
            "  └ ✓ Exit 0, took 1.1s",
        ]
    );
    assert_eq!(lines[0].line.spans[1].content, "$ ");
    assert_eq!(lines[0].line.spans[1].style.fg, Some(a.theme.success));
    assert_eq!(lines[0].line.spans[2].style.fg, Some(a.theme.fg));
    assert_eq!(lines[1].line.spans[1].style.fg, Some(a.theme.subtle));
    assert_eq!(lines[3].line.spans[2].style.fg, Some(a.theme.success));
}

#[test]
fn user_bash_nonzero_exit_is_visible_and_error_colored() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        prompt: String::new(),
        blocks: vec![Block::UserBash {
            command: "false".to_string(),
            output: "failed".to_string(),
            exit_code: Some(7),
            signal: None,
            duration: Duration::from_millis(900),
            truncated: false,
            cancelled: false,
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
    let status: String = lines[2]
        .line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();

    assert_eq!(status, "  └ ✗ Exit 7, took 0.9s");
    assert_eq!(lines[0].line.spans[1].style.fg, Some(a.theme.success));
    assert_eq!(lines[0].line.spans[2].style.fg, Some(a.theme.fg));
    assert_eq!(lines[1].line.spans[2].style.fg, Some(a.theme.error));
    assert_eq!(lines[2].line.spans[2].style.fg, Some(a.theme.error));
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

fn inline_markdown_bold_italic_underscore() {
    use ratatui::style::Modifier;
    let spans = render_text_spans("**bold** *italic* _underline_");
    let bold = spans
        .iter()
        .find(|s| s.content == "bold")
        .expect("bold span");
    assert!(bold.style.add_modifier == Modifier::BOLD, "bold: {bold:?}");
    let italic = spans
        .iter()
        .find(|s| s.content == "italic")
        .expect("italic span");
    assert!(
        italic.style.add_modifier == Modifier::ITALIC,
        "italic: {italic:?}"
    );
    let under = spans
        .iter()
        .find(|s| s.content == "underline")
        .expect("underline span");
    assert!(
        under.style.add_modifier == Modifier::UNDERLINED,
        "underline: {under:?}"
    );
}

#[test]
fn inline_markdown_code_stays_literal() {
    let spans = render_text_spans("use `inline_spans` here");
    let code = spans
        .iter()
        .find(|s| s.content == " inline_spans ")
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
        .find(|s| s.content == " code ")
        .expect("code span");
    assert!(
        code_span.style.fg.is_some(),
        "code cell should have fg color: {code_span:?}"
    );
}

#[test]
fn numbered_empty_body_line_keeps_its_number() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    // Verbose so the `read` result renders — non-verbose now hides
    // non-bash results for a cleaner transcript.
    a.verbose = true;
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.read('a.txt')".to_string(),
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
        result: serde_json::json!({ "content": "line one\n\nline three", "start_line": 1, "total_lines": 3, "truncated": false }).to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // The empty-body numbered line keeps its decoration: content range
    // collapses to (cstart, cstart) but the line still has spans, so the
    // Navigate cursor overlay preserves it instead of replacing it.
    let empty = rls
        .iter()
        .find(|rl| rl.content.0 > 0 && rl.content.0 == rl.content.1 && !rl.line.spans.is_empty())
        .expect("empty-body numbered line should keep its decoration");
    let s: String = empty
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        s.contains(" 2 ") || s.contains(" 2"),
        "number preserved: {s:?}"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn non_verbose_hides_read_results_keeps_mutations_and_errors() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.read('a.txt'); lofi.write(...); lofi.edit(...); lofi.bash('echo x')"
            .to_string(),
        label: Some("mixed".to_string()),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "read".to_string(),
        args: "a.txt".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(), id: 0, result: serde_json::json!({ "content": "secret line one", "start_line": 1, "total_lines": 1, "truncated": false }).to_string(), is_error: false,
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 1,
        name: "write".to_string(),
        args: "b.txt".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 1,
        result: serde_json::json!({ "ok": true, "content": "written content here" }).to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 2,
        name: "edit".to_string(),
        args: "c.txt".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 2,
        result: serde_json::json!({ "ok": true, "old": "old text", "new": "edited content here" })
            .to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 3,
        name: "bash".to_string(),
        args: "echo x".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 3,
        result: serde_json::json!({ "ok": true, "output": "bash output here", "code": 0 })
            .to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 4,
        name: "read".to_string(),
        args: "missing.txt".to_string(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 4,
        result: "no such file".to_string(),
        is_error: true,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let text = |a: &App| -> String {
        let cx = Cx {
            app: a,
            theme: a.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, &a.turns[0])
            .iter()
            .flat_map(|rl| rl.line.spans.iter())
            .flat_map(|s| s.content.chars())
            .collect()
    };
    a.verbose = false;
    let nv = text(&a);
    assert!(
        !nv.contains("secret line one"),
        "non-verbose read body should hide: {nv}"
    );
    assert!(
        nv.contains("bash output here"),
        "non-verbose bash body should show: {nv}"
    );
    assert!(
        nv.contains("written content here"),
        "non-verbose write body should show its content: {nv}"
    );
    assert!(
        nv.contains("edited content here"),
        "non-verbose edit body should show its content: {nv}"
    );
    assert!(
        nv.contains("no such file"),
        "non-verbose error should stay visible: {nv}"
    );
    assert!(
        nv.contains("Tool read"),
        "read header should still show: {nv}"
    );
    assert!(
        nv.contains("Tool write"),
        "write header should still show: {nv}"
    );
    a.verbose = true;
    let v = text(&a);
    assert!(
        v.contains("secret line one"),
        "verbose read body should show: {v}"
    );
}

#[test]
fn non_verbose_hides_exec_result_body_keeps_status_and_errors() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let text = |a: &App| -> String {
        let cx = Cx {
            app: a,
            theme: a.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, &a.turns[0])
            .iter()
            .flat_map(|rl| rl.line.spans.iter())
            .flat_map(|s| s.content.chars())
            .collect()
    };
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "const x = 1;".to_string(),
        label: Some("compute".to_string()),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":\"all done marker\"}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    a.verbose = false;
    let nv = text(&a);
    assert!(
        !nv.contains("all done marker"),
        "non-verbose exec result body should hide: {nv}"
    );
    assert!(
        nv.contains("Succeed"),
        "non-verbose exec status header should stay: {nv}"
    );
    a.verbose = true;
    let v = text(&a);
    assert!(
        v.contains("all done marker"),
        "verbose exec result body should show: {v}"
    );

    // A failed exec keeps its error body even in non-verbose so a failure is
    // never silently swallowed.
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e2".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e2".to_string(),
        code: "throw new Error('x')".to_string(),
        label: Some("compute".to_string()),
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e2".to_string(),
        result: "exec blew up here".to_string(),
        is_error: true,
        elapsed_ms: 0,
    });
    a.verbose = false;
    let nv = text(&a);
    assert!(
        nv.contains("exec blew up here"),
        "non-verbose exec error body should stay: {nv}"
    );
    assert!(
        nv.contains("Failed"),
        "non-verbose exec error status should stay: {nv}"
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
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .trim()
                .is_empty()
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

fn last_text(app: &App) -> Option<&str> {
    app.turns.last()?.blocks.iter().rev().find_map(|b| match b {
        Block::Text(t) => Some(t.as_str()),
        _ => None,
    })
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}

fn sev_chain<I: IntoIterator<Item = SessionEventKind>>(kinds: I) -> Vec<SessionEvent> {
    let mut out = Vec::new();
    let mut parent: Option<String> = None;
    for (i, kind) in kinds.into_iter().enumerate() {
        let id = format!("e{i}");
        out.push(SessionEvent {
            id: id.clone(),
            parent_id: parent.clone(),
            kind,
        });
        parent = Some(id);
    }
    out
}

fn msg(m: Message) -> SessionEventKind {
    SessionEventKind::Message(m)
}

#[test]
fn standalone_user_bash_keeps_turn_backing_metadata_aligned() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "first".into(),
    });
    a.apply_event(AgentEvent::Text("answer".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 10,
        byte_end: 20,
    });

    a.apply_event(AgentEvent::UserBash {
        command: "pwd".into(),
        output: "/tmp".into(),
        exit_code: Some(0),
        signal: None,
        duration_ms: 1,
        truncated: false,
        cancelled: false,
        exclude_from_context: false,
    });

    assert_eq!(a.turns.len(), 2);
    assert_eq!(a.turn_byte_ranges.len(), 2);
    assert_eq!(a.turn_event_offsets.len(), 2);
    assert!(a.turns[0].blocks.is_empty());

    a.turn_byte_ranges[1] = Some((20, 30));
    a.apply_event(AgentEvent::TurnStart {
        prompt: "second".into(),
    });
    assert!(a.turns[1].blocks.is_empty());
    assert_eq!(a.turns.len(), a.turn_byte_ranges.len());
    assert_eq!(a.turns.len(), a.turn_event_offsets.len());
}

#[test]
fn resumed_user_bash_and_final_turn_are_file_backed_shells() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let cursor = store
        .create_cursor(std::path::Path::new("/workspace"), &"p/m".into())
        .unwrap();
    let kinds = [
        msg(user("first")),
        msg(assistant("first answer")),
        SessionEventKind::TurnEnd {
            model: "p/m".into(),
            elapsed_ms: 1,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::UserBash {
            command: "pwd".into(),
            output: "/workspace".into(),
            exit_code: Some(0),
            signal: None,
            duration_ms: 1,
            truncated: false,
            cancelled: false,
            exclude_from_context: false,
        },
        msg(user("second")),
        msg(assistant("final answer")),
        SessionEventKind::TurnEnd {
            model: "p/m".into(),
            elapsed_ms: 1,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut events: Vec<_> = kinds
        .into_iter()
        .map(|kind| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind,
        })
        .collect();
    cursor.append_events(&mut events).unwrap();
    let snapshot = cursor.snapshot().unwrap();

    let mut a = app();
    a.session.cursor = Some(cursor.clone());
    a.restore_indexed_session(&cursor, &snapshot.index, snapshot.file_size)
        .unwrap();

    assert_eq!(a.turns.len(), 3);
    assert_eq!(a.turns.len(), a.turn_byte_ranges.len());
    assert_eq!(a.turns.len(), a.turn_event_offsets.len());
    assert!(a.turns.iter().all(|turn| turn.blocks.is_empty()));
    let final_turn = a.materialize_turn(2);
    assert!(final_turn
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "final answer")));
}

#[test]
fn text_deltas_accumulate_into_one_text_block() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text("hel".to_string()));
    a.apply_event(AgentEvent::Text("lo".to_string()));
    assert_eq!(last_text(&a), Some("hello"));
    assert_eq!(a.turns[0].blocks.len(), 1);
}

#[test]
fn thinking_deltas_accumulate_into_their_own_block() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Thinking("hm".to_string()));
    a.apply_event(AgentEvent::Text("hi".to_string()));
    a.apply_event(AgentEvent::Thinking("more".to_string()));
    let blocks = &a.turns[0].blocks;
    assert!(matches!(blocks[0], Block::Thinking(_)));
    assert!(matches!(blocks[1], Block::Text(_)));
    assert!(matches!(blocks[2], Block::Thinking(_)));
    assert_eq!(blocks.len(), 3);
}

#[test]
fn tool_start_then_text_starts_new_text_block() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text("first".to_string()));
    a.apply_event(AgentEvent::ToolStart {
        id: "t1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::Text("second".to_string()));
    let blocks = &a.turns[0].blocks;
    assert_eq!(blocks.len(), 3);
    assert!(matches!(blocks[0], Block::Text(_)));
    assert!(matches!(blocks[1], Block::Tool(_)));
    assert!(matches!(blocks[2], Block::Text(_)));
}

#[test]
fn tool_input_and_end_land_under_matching_id() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "t1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolStart {
        id: "t2".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "t1".to_string(),
        code: "code-1".to_string(),
        label: None,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "t2".to_string(),
        result: "r2".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "t1".to_string(),
        result: "r1".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let blocks = &a.turns[0].blocks;
    let Block::Tool(t1) = &blocks[0] else {
        unreachable!()
    };
    let Block::Tool(t2) = &blocks[1] else {
        unreachable!()
    };
    assert_eq!(t1.input, "code-1");
    assert_eq!(t1.result.as_deref(), Some("r1"));
    assert!(t1.done);
    assert_eq!(t2.result.as_deref(), Some("r2"));
    assert!(t2.done);
}

#[test]
#[allow(clippy::too_many_lines)]
fn round_commit_releases_only_hidden_exec_result_and_verbose_restores_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("round-commit.jsonl");
    std::fs::write(
        &path,
        b"{\"type\":\"meta\",\"version\":2,\"created\":0,\"cwd\":\"\",\"model\":\"p/m\"}\n",
    )
    .unwrap();
    let cursor = store::SessionCursor::new(path, None);
    let durable_result = "{\"value\":\"durable outer result\"}";
    let mut events = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(user("go")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::ToolUse {
                    id: "e1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({ "code": "return 1" }),
                }],
            }),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::Tool,
                blocks: vec![ContentBlock::ToolResult {
                    tool_use_id: "e1".into(),
                    content: durable_result.into(),
                    is_error: false,
                }],
            }),
        },
    ];
    let (start, end) = cursor.append_events(&mut events).unwrap();

    let mut a = app();
    a.session.cursor = Some(cursor);
    a.apply_event(AgentEvent::TurnStart {
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Thinking("still visible thought".into()));
    a.apply_event(AgentEvent::Text("still visible text".into()));
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".into(),
        name: "exec".into(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".into(),
        code: "return 1".into(),
        label: None,
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".into(),
        id: 0,
        name: "bash".into(),
        args: "printf native".into(),
    });
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".into(),
        id: 0,
        result: "native result stays resident".into(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".into(),
        result: durable_result.into(),
        is_error: false,
        elapsed_ms: 1,
    });
    let block_count = a.turns[0].blocks.len();

    a.apply_event(AgentEvent::RoundCommitted {
        byte_start: start,
        byte_end: end,
    });

    assert_eq!(a.turns[0].prompt, "go");
    assert_eq!(a.turns[0].blocks.len(), block_count);
    assert!(
        matches!(&a.turns[0].blocks[0], Block::Thinking(thinking) if thinking.text == "still visible thought")
    );
    assert!(matches!(&a.turns[0].blocks[1], Block::Text(text) if text == "still visible text"));
    let Block::Tool(tool) = &a.turns[0].blocks[2] else {
        panic!("exec block preserved")
    };
    assert!(tool.result_committed);
    assert!(
        tool.result.is_none(),
        "collapsed hidden duplicate is released"
    );
    assert_eq!(
        tool.native[0].result.as_deref(),
        Some("native result stays resident")
    );

    a.toggle_verbose();
    let Block::Tool(tool) = &a.turns[0].blocks[2] else {
        panic!("exec block preserved")
    };
    assert_eq!(tool.result.as_deref(), Some(durable_result));
    assert_eq!(a.turns[0].prompt, "go");
    assert_eq!(a.turns[0].blocks.len(), block_count);

    a.toggle_verbose();
    let Block::Tool(tool) = &a.turns[0].blocks[2] else {
        panic!("exec block preserved")
    };
    assert!(tool.result.is_none());
}

#[test]
fn turn_committed_extends_existing_range_across_silent_continuation() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });
    a.apply_event(AgentEvent::TurnContinue);
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 250,
        byte_end: 300,
    });
    assert_eq!(a.turn_byte_ranges, vec![Some((100, 300))]);
}

#[test]
fn run_finished_keeps_latest_persisted_turn_visible_until_next_prompt() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("visible response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });

    a.run_finished();

    assert!(a.turns[0]
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "visible response")));
    assert_eq!(a.turn_byte_ranges, vec![Some((100, 200))]);
}

#[test]
fn next_turn_freezes_previous_response_atomically_with_new_prompt() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "previous prompt".into(),
    });
    a.apply_event(AgentEvent::Text("previous response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });
    a.run_finished();

    assert!(a.turns[0]
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "previous response")));

    a.apply_event(AgentEvent::TurnStart {
        prompt: "new prompt".into(),
    });
    assert_eq!(a.turns.len(), 2);
    assert!(a.turns[0].blocks.is_empty());
    assert_eq!(a.turns[1].prompt, "new prompt");
}

#[test]
fn settled_first_turn_remains_visible_from_committed_cursor_range() {
    let dir = tempfile::tempdir().unwrap();
    let store = store::SessionStore::new(dir.path().join("sessions"));
    let cursor = store
        .create_cursor(Path::new("/tmp/settled-first-turn"), &"p/m".into())
        .unwrap();
    let mut events = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::System,
                blocks: vec![ContentBlock::Text {
                    text: "system".into(),
                }],
            }),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(user("go")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(assistant("visible response")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
            },
        },
    ];
    let (start, end) = cursor.append_events(&mut events).unwrap();

    let mut a = app();
    a.session.cursor = Some(cursor);
    a.apply_event(AgentEvent::TurnStart {
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("visible response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: start,
        byte_end: end,
    });
    a.run_finished();

    assert!(a.turns[0]
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "visible response")));
}

#[test]
fn run_finished_keeps_blocks_for_hard_cap_continuation() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "go".into(),
    });
    a.apply_event(AgentEvent::Text("partial response".into()));
    a.apply_event(AgentEvent::TurnCommitted {
        byte_start: 100,
        byte_end: 200,
    });
    a.context_pressure = true;

    a.run_finished();

    assert!(!a.turns[0].blocks.is_empty());
}

#[test]
fn turn_end_updates_usage() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.0,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert_eq!(a.total_in, 10);
    assert_eq!(a.total_out, 20);
}

#[test]
fn round_usage_updates_totals_per_round() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart { prompt: "p".into() });
    a.apply_event(AgentEvent::RoundUsage {
        cost: 0.01,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert_eq!(a.total_in, 100);
    assert_eq!(a.total_out, 50);
    assert_eq!(a.turn_cost, 0.01);
    assert!(a.turn_has_round_usage);
    assert!((a.cost + a.turn_cost - 0.01).abs() < 1e-9);

    a.apply_event(AgentEvent::RoundUsage {
        cost: 0.03,
        usage: Usage {
            input_tokens: 200,
            output_tokens: 80,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert_eq!(a.total_in, 300);
    assert_eq!(a.total_out, 130);
    assert!((a.turn_cost - 0.03).abs() < 1e-9);
    assert!((a.cost + a.turn_cost - 0.03).abs() < 1e-9);

    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.03,
        usage: Usage {
            input_tokens: 200,
            output_tokens: 80,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert!((a.cost - 0.03).abs() < 1e-9);
    assert_eq!(a.turn_cost, 0.0);
    assert!(!a.turn_has_round_usage);
    assert_eq!(a.total_in, 300);
    assert_eq!(a.total_out, 130);
}

#[test]
fn turn_end_folds_bundled_totals_on_resume_path() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart { prompt: "p".into() });
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.05,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    assert!((a.cost - 0.05).abs() < 1e-9);
    assert_eq!(a.total_in, 10);
    assert_eq!(a.total_out, 20);
    assert_eq!(a.turn_cost, 0.0);
    assert!(!a.turn_has_round_usage);
}

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
    let body: String = info
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(body.contains("(none)"));
    assert!(body.contains("No session file"));
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
    let body: String = info
        .lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
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

fn confirm_request(
    command: &str,
) -> (
    lofi_core::ConfirmRequest,
    tokio::sync::oneshot::Receiver<bool>,
) {
    let (respond, response) = tokio::sync::oneshot::channel();
    (
        lofi_core::ConfirmRequest {
            id: 1,
            command: command.to_string(),
            reason: std::sync::Arc::new(std::sync::Mutex::new(lofi_core::ConfirmReason::Policy)),
            active: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            respond,
        },
        response,
    )
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
fn slash_complete_filters_and_accepts() {
    let mut a = app();
    a.input = "/".to_string();
    a.refresh_slash_complete();
    let sc = a.slash_complete.as_ref().expect("popover open");
    assert_eq!(sc.candidates.len(), SLASH_COMMANDS.len());
    a.input = "/tr".to_string();
    a.refresh_slash_complete();
    let sc = a.slash_complete.as_ref().expect("popover open");
    assert_eq!(sc.candidates, vec![9]); // /tree is index 9
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
fn tree_no_session_pushes_error() {
    let mut a = app();
    assert!(a.slash_command("/tree"));
    assert!(a.tree_picker.is_none());
    let (msg, kind) = a.notify_badge().expect("/tree notified");
    assert_eq!(kind, NotifyKind::Error);
    assert!(msg.contains("no session file"));
    assert!(a.session.cursor.is_none());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tree_opens_rolls_back_and_prefills_prompt() {
    // Build a two-turn session: user1 → assistant1 → turn_end1 →
    // user2 → assistant2 → turn_end2. The picker should offer three
    // branch points: "after turn 1" (turn_end1), "edit turn 2"
    // (user2, prefilled), and "after turn 2" (turn_end2). Confirming
    // the "edit turn 2" entry rolls the transcript back to turn 1,
    // prefills the input with user2's text, and sets the branch hint
    // to turn_end1's id (so the resend is a sibling of user2).
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "first".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: k,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();
    // Read back the ids so the test can assert against them.
    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let turn_end1_id = events[2].id.clone();
    let selected_leaf = turn_end1_id.clone();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    assert_eq!(picker.entries.len(), 4);
    assert_eq!(picker.selected, 3); // defaults to the last entry
    let edit_idx = picker
        .entries
        .iter()
        .position(|e| e.prefill == "second")
        .unwrap();
    a.tree_picker.as_mut().unwrap().selected = edit_idx;
    a.tree_picker_confirm();
    assert!(a.tree_picker.is_none());
    assert_eq!(a.input, "second");
    assert_eq!(
        a.session
            .cursor
            .as_ref()
            .and_then(store::SessionCursor::leaf_id),
        Some(turn_end1_id)
    );
    // One visible turn (turn 1); turn 2 is rolled back out of view. The
    // selected lineage stays file-backed: /tree must not retain a second copy
    // of historical response/tool bodies in the display turns.
    assert_eq!(a.turns.len(), 1);
    assert!(a.turns[0].blocks.is_empty());
    assert_eq!(a.lifecycle.history_stats().messages, 2); // user1 + assistant1

    // The final selected turn ends at its lineage event, not physical EOF.
    // Otherwise lazy materialization would read the rolled-back second turn
    // and display sibling/future content after selecting an earlier branch.
    let snapshot = a.session.cursor.as_ref().unwrap().tree_snapshot().unwrap();
    let index = snapshot.index;
    let file_size = snapshot.file_size;
    let selected_end = index
        .iter()
        .find(|event| event.id.matches(&selected_leaf))
        .unwrap()
        .end_offset;
    let first_user = index
        .iter()
        .find(|event| event.kind == store::IndexKind::UserPrompt)
        .unwrap();
    assert_eq!(
        a.turn_byte_ranges,
        vec![Some((first_user.offset, selected_end))]
    );
    assert!(selected_end < file_size);
    let materialized = a.materialize_turn(0);
    assert!(materialized
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "hello")));
    assert!(!materialized
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "world")));

    let cached = a.materialize_turn(0);
    assert!(cached
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "hello")));
    let hits = a.collapsed_turns.borrow().frame_hits;
    a.bump_render_epoch();
    let after_layout_invalidation = a.materialize_turn(0);
    assert!(after_layout_invalidation
        .blocks
        .iter()
        .any(|block| matches!(block, Block::Text(text) if text == "hello")));
    assert_eq!(a.collapsed_turns.borrow().frame_hits, hits + 1);
}

#[test]
fn tree_file_backing_excludes_physically_interleaved_sibling_events() {
    use lofi_core::session::store::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let session_store = SessionStore::new(dir.path().join("s"));
    let path = session_store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let turn_end = || SessionEventKind::TurnEnd {
        model: "m".into(),
        elapsed_ms: 1,
        cost: 0.0,
        usage: Usage::default(),
    };
    let wrap = |kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    };

    let mut root = [
        wrap(msg(user("root"))),
        wrap(msg(assistant("shared"))),
        wrap(turn_end()),
    ];
    test_append_events(&path, &mut root, None).unwrap();
    let root_end = root[2].id.clone();
    let mut branch_a = [
        wrap(msg(user("branch a"))),
        wrap(msg(assistant("SIBLING MUST NOT LEAK"))),
        wrap(turn_end()),
    ];
    test_append_events(&path, &mut branch_a, Some(&root_end)).unwrap();
    let mut branch_b = [
        wrap(msg(user("branch b"))),
        wrap(msg(assistant("chosen"))),
        wrap(turn_end()),
    ];
    test_append_events(&path, &mut branch_b, Some(&root_end)).unwrap();
    let branch_b_end = branch_b[2].id.clone();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::new(path, Some(branch_b_end.clone())));
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let selected = a
        .tree_picker
        .as_ref()
        .unwrap()
        .entries
        .iter()
        .position(|entry| entry.branch_point == branch_b_end)
        .unwrap();
    a.tree_picker.as_mut().unwrap().selected = selected;
    a.tree_picker_confirm();

    assert_eq!(a.turns.len(), 2);
    assert!(a.turns.iter().all(|turn| turn.blocks.is_empty()));
    let shared = a.materialize_turn(0);
    let chosen = a.materialize_turn(1);
    let text = |turn: &Turn| {
        turn.blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(text(&shared), "shared");
    assert_eq!(text(&chosen), "chosen");
    assert!(!text(&shared).contains("SIBLING MUST NOT LEAK"));

    let mut term = ratatui::Terminal::new(TestBackend::new(80, 20)).unwrap();
    term.draw(|frame| crate::tui::view::render(frame, &mut a))
        .unwrap();
    assert_eq!(a.frozen_heights.len(), a.turns.len());
    let buf = term.backend().buffer();
    let screen = (0..20)
        .map(|y| (0..80).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(screen.contains("shared"), "{screen}");
    assert!(screen.contains("chosen"), "{screen}");
    assert!(!screen.contains("SIBLING MUST NOT LEAK"), "{screen}");
}

#[test]
fn tree_shows_compaction_node_and_reverts_before_it() {
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "first".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Compaction {
            summary: "summary".into(),
            first_kept_entry_id: String::new(),
            summarized_range: [String::new(), String::new()],
            checkpointed_tail: false,
            summarized: 3,
            represented: 3,
            kept: 1,
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: k,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();
    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let turn_end1_id = events[2].id.clone();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    assert!(picker
        .entries
        .iter()
        .any(|e| e.label.starts_with("compact:")));
    let comp_idx = picker
        .entries
        .iter()
        .position(|e| e.label.starts_with("compact:"))
        .unwrap();
    assert_eq!(picker.entries[comp_idx].branch_point, turn_end1_id);
    assert!(picker.entries[comp_idx].prefill.is_empty());
    a.tree_picker.as_mut().unwrap().selected = comp_idx;
    a.tree_picker_confirm();
    assert!(a.tree_picker.is_none());
    assert_eq!(
        a.session
            .cursor
            .as_ref()
            .and_then(store::SessionCursor::leaf_id),
        Some(turn_end1_id)
    );
    assert_eq!(a.lifecycle.history_stats().messages, 2); // user1 + assistant1
    assert_eq!(a.turns.len(), 1);
}

#[test]
fn tree_hides_checkpoint_copies_and_reverts_to_pre_compaction_leaf() {
    use lofi_core::session::store::{self, SessionStore};
    let dir = tempfile::tempdir().unwrap();
    let session_store = SessionStore::new(dir.path().join("s"));
    let path = session_store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let mut original: Vec<SessionEvent> = [
        msg(user("first")),
        msg(assistant("hello")),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut original, None).unwrap();
    let pre_compaction_leaf = original[2].id.clone();
    test_append_compaction(
        &path,
        &[user("first"), assistant("hello")],
        None,
        "summary",
        &[original[0].id.clone(), original[1].id.clone()],
        store::CompactionCounts {
            summarized: 2,
            represented: 2,
            kept: 2,
        },
    )
    .unwrap();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().unwrap();
    assert_eq!(
        picker
            .entries
            .iter()
            .filter(|e| e.label.starts_with("user: first"))
            .count(),
        1,
        "checkpoint copy must not appear as another tree turn"
    );
    let compact = picker
        .entries
        .iter()
        .find(|e| e.label.starts_with("compact:"))
        .unwrap();
    assert_eq!(compact.branch_point, pre_compaction_leaf);
}

#[test]
fn modal_tab_cycles_with_wraparound() {
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "first".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: k,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();
    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let len = a.tree_picker.as_ref().unwrap().entries.len();
    assert_eq!(len, 4);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 3);
    let mut run = None;
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 0);
    handle_event(&plain_key(KeyCode::BackTab), &mut a, None, &mut run);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 3);
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 0);
    handle_event(
        &Event::Key(crossterm::event::KeyEvent::new_with_kind(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
        )),
        &mut a,
        None,
        &mut run,
    );
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 1);
}

#[test]
fn tree_revert_to_root_then_reopens() {
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text { text: "sys".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "first".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: k,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::new(path.clone(), None));
    a.session.cwd = std::path::PathBuf::from("/x");
    // First /tree: select the root user prompt (entry 0) and revert.
    // Its branch_point is its parent (the system message), so the
    // active path becomes just the system message — the transcript is
    // empty (no visible turns) but the cursor leaf is the system id.
    assert!(a.slash_command("/tree"));
    a.tree_picker.as_mut().unwrap().selected = 0;
    a.tree_picker_confirm();
    assert!(a
        .session
        .cursor
        .as_ref()
        .and_then(store::SessionCursor::leaf_id)
        .is_some());
    assert_eq!(a.turns.len(), 0); // rolled back to before any user turn
    assert_eq!(a.input, "first");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker reopened");
    assert_eq!(picker.entries.len(), 4);
    assert!(picker.entries.iter().all(|e| !e.is_active));
}

#[test]
fn tree_shows_tool_result_nodes() {
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "list files".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "tu1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu1".into(),
                content: "file_a.txt file_b.txt".into(),
                is_error: false,
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: k,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();
    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let tool_result_id = events[2].id.clone();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    assert_eq!(picker.entries.len(), 3);
    assert!(picker.entries[0].label.starts_with("user:"));
    assert!(
        picker.entries[1].label.starts_with("tool:"),
        "expected tool: node, got {}",
        picker.entries[1].label
    );
    assert!(picker.entries[2].label.starts_with("agent:"));
    assert!(
        picker.entries[1].label.contains("bash"),
        "tool label should contain tool name, got {}",
        picker.entries[1].label
    );
    assert!(
        picker.entries[1].label.contains("file_a"),
        "tool label should contain result preview, got {}",
        picker.entries[1].label
    );
    assert_eq!(picker.entries[1].branch_point, tool_result_id);
    assert!(picker.entries[1].prefill.is_empty());
}

#[test]
#[allow(clippy::too_many_lines)]
fn tree_exec_label_shows_native_tools() {
    // user -> assistant(exec ToolUse) -> tool_result -> native_tool(read) ->
    // native_tool(edit) -> native_tool(read) -> assistant(text) -> turn_end
    // /tree must show the exec tool result as `exec: read demo.txt, edit demo.txt, read demo.txt`
    // instead of the raw exec result.
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, NativeToolRecord, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "do stuff".into(),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "exec_0".into(),
                name: "exec".into(),
                input: serde_json::json!({"code": "..."}),
            }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "exec_0".into(),
                content: "exec result".into(),
                is_error: false,
            }],
        }),
        SessionEventKind::NativeTool(NativeToolRecord {
            parent: "exec_0".into(),
            call_id: 0,
            name: "write".into(),
            args: "demo.txt".into(),
            result: "ok".into(),
            is_error: false,
        }),
        SessionEventKind::NativeTool(NativeToolRecord {
            parent: "exec_0".into(),
            call_id: 1,
            name: "edit".into(),
            args: "demo.txt".into(),
            result: "ok".into(),
            is_error: false,
        }),
        SessionEventKind::NativeTool(NativeToolRecord {
            parent: "exec_0".into(),
            call_id: 2,
            name: "read".into(),
            args: "demo.txt".into(),
            result: "ok".into(),
            is_error: false,
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".into(),
            }],
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: k,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    let exec_entry = picker
        .entries
        .iter()
        .find(|e| e.label.starts_with("exec:"))
        .expect("should have an exec: node");
    assert!(
        exec_entry.label.contains("write"),
        "exec label should list write tool, got: {}",
        exec_entry.label
    );
    assert!(
        exec_entry.label.contains("edit"),
        "exec label should list edit tool, got: {}",
        exec_entry.label
    );
    assert!(
        exec_entry.label.contains("read"),
        "exec label should list read tool, got: {}",
        exec_entry.label
    );
    assert!(
        !exec_entry.label.contains("exec result"),
        "exec label should not contain the raw result, got: {}",
        exec_entry.label
    );

    // The interactive picker uses the progressive hydrator, not the
    // synchronous fallback above. Keep that path covered independently so an
    // optimization cannot silently degrade exec rows to raw result previews.
    let cursor = a.session.cursor.as_ref().unwrap();
    let snapshot = cursor.tree_snapshot().unwrap();
    let skeletons =
        build_tree_entry_skeletons(&snapshot.index, snapshot.leaf_id.as_deref(), cursor);
    let mut hydrated = Vec::new();
    hydrate_tree_entry_window(
        &snapshot.index,
        cursor,
        &skeletons,
        0..skeletons.len(),
        |rows| {
            hydrated.extend(rows);
            true
        },
        || false,
    );
    let progressive_exec = hydrated
        .iter()
        .map(|(_, entry)| entry)
        .find(|entry| entry.label.starts_with("exec:"))
        .expect("progressive hydration should produce an exec node");
    assert!(progressive_exec.label.contains("write demo.txt"));
    assert!(progressive_exec.label.contains("edit demo.txt"));
    assert!(progressive_exec.label.contains("read demo.txt"));
    assert!(!progressive_exec.label.contains("exec result"));
}

#[test]
fn tree_shows_tool_result_nodes_in_v1_session() {
    // V1 session files have no `type` field on message lines and no
    // turn_end events — just raw Message objects. The index scan must
    // still classify role=tool messages as ToolResult so they appear as
    // `tool:` nodes in /tree.
    use lofi_types::{ContentBlock, Message, Role};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s");
    let header = r#"{"type":"meta","version":1,"created":1784436411351,"cwd":"/x","model":"m"}
"#;
    let lines = [
        serde_json::to_string(&Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "list files".into(),
            }],
        })
        .unwrap(),
        serde_json::to_string(&Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "tu1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            }],
        })
        .unwrap(),
        serde_json::to_string(&Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu1".into(),
                content: "file_a.txt".into(),
                is_error: false,
            }],
        })
        .unwrap(),
    ];
    std::fs::write(&path, format!("{header}{}\n", lines.join("\n"))).unwrap();

    let mut a = app();
    a.session.cursor = Some(store::SessionCursor::open(path).unwrap());
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    assert!(
        picker.entries.iter().any(|e| e.label.starts_with("user:")),
        "should have user node"
    );
    assert!(
        picker.entries.iter().any(|e| e.label.starts_with("tool:")),
        "should have tool node in v1 session"
    );
}

#[test]
fn verbose_toggles() {
    let mut a = app();
    assert!(!a.verbose);
    let before = a.turns.len();
    a.frozen_heights = vec![3, 5, 8];
    a.toggle_verbose();
    assert!(a.verbose);
    assert_eq!(a.debug_after_draw, Some("verbose"));
    assert!(a.frozen_heights.is_empty());
    assert_eq!(a.frozen_heights_other_mode, vec![3, 5, 8]);
    assert_eq!(a.turns.len(), before);
    a.debug_after_draw = None;
    a.frozen_heights = vec![30, 50, 80];
    a.toggle_verbose();
    assert!(!a.verbose);
    assert_eq!(a.debug_after_draw, Some("verbose"));
    assert_eq!(a.frozen_heights, vec![3, 5, 8]);
    assert_eq!(a.frozen_heights_other_mode, vec![30, 50, 80]);
    assert_eq!(a.turns.len(), before);
}
#[test]
fn verbose_expands_compaction_summary() {
    use crate::tui::view::blocks::render_turns;
    let summary = "## Session Goal\nBuild a coding agent.\n## Decisions\n- Use Rust.";
    let mut a = app();
    a.turns.push(Turn {
        prompt: "p".to_string(),
        blocks: vec![Block::Compaction {
            summarized: 7,
            kept: 2,
            summary: summary.to_string(),
        }],
    });

    let collapsed = render_turns(&a, 80);
    let collapsed_s = join_rendered(&collapsed);
    assert!(
        collapsed_s.contains("Compacted 7 messages"),
        "collapsed: {collapsed_s}"
    );
    assert!(
        !collapsed_s.contains("Build a coding agent"),
        "collapsed leaked summary: {collapsed_s}"
    );

    a.toggle_verbose();
    let expanded = render_turns(&a, 80);
    let expanded_s = join_rendered(&expanded);
    assert!(expanded_s.contains("Compacted 7 messages"));
    assert!(
        expanded_s.contains("Build a coding agent"),
        "expanded missing summary: {expanded_s}"
    );
    assert!(
        expanded_s.contains("Use Rust."),
        "expanded missing summary: {expanded_s}"
    );
}

fn join_rendered(text: &ratatui::text::Text<'static>) -> String {
    let mut s = String::new();
    for line in &text.lines {
        for span in &line.spans {
            s.push_str(span.content.as_ref());
        }
        s.push('\n');
    }
    s
}

#[test]
fn turn_failed_wraps_error_below_header() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    a.turns.push(Turn {
        prompt: String::new(),
        blocks: vec![Block::TurnFailed {
            label: "openai/gpt-4o · medium".to_string(),
            elapsed: Duration::from_secs(12),
            error: "the provider returned a 503 with a very long service-unavailable message that must wrap".to_string(),
        }],
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 40,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // Line 0 is the status header: model, level, duration only — the error
    // must not appear inline there (it used to, and got clipped).
    let header: String = rls[0]
        .line
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        header.contains("Failed in"),
        "header missing status: {header}"
    );
    assert!(
        header.contains("openai/gpt-4o"),
        "header missing label: {header}"
    );
    assert!(
        !header.contains("503"),
        "header must not carry the error inline: {header}"
    );
    let body: String = rls[1..]
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(
        body.contains("503"),
        "wrapped error missing from body: {body}"
    );
    for rl in &rls[1..] {
        assert!(
            rl.line.width() <= 40,
            "body line overflows: {}",
            rl.line.width()
        );
    }
}

#[test]
fn turn_failed_dedups_after_fatal_error_block() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    let msg = "stream interrupted by upstream gateway";
    a.turns.push(Turn {
        prompt: String::new(),
        blocks: vec![
            Block::Error(msg.to_string()),
            Block::TurnFailed {
                label: "openai/gpt-4o · medium".to_string(),
                elapsed: Duration::from_secs(3),
                error: format!("provider error: {msg}"),
            },
        ],
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 80,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    // The provider emits the error twice on a stream failure — as a fatal
    // `✗` line and again in the TurnFailed marker. The dedup must collapse
    // them so the message appears exactly once across the rendered turn.
    let all: String = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(
        all.matches("stream interrupted").count(),
        1,
        "dedup failed: {all}"
    );
}

#[test]
fn exec_result_wraps_long_lines_instead_of_truncating() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart {
        id: "e1".to_string(),
        name: "exec".to_string(),
    });
    a.apply_event(AgentEvent::ToolInput {
        id: "e1".to_string(),
        code: "lofi.bash('echo t')".to_string(),
        label: Some("echo".to_string()),
    });
    a.apply_event(AgentEvent::NativeToolStart {
        parent: "e1".to_string(),
        id: 0,
        name: "bash".to_string(),
        args: "echo t".to_string(),
    });
    let token = "abcdefghijklmnopqrstuvwxyz1234567890abcdefghij";
    a.apply_event(AgentEvent::NativeToolEnd {
        parent: "e1".to_string(),
        id: 0,
        result: serde_json::json!({ "ok": true, "output": token, "code": 0 }).to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let turn = &a.turns[0];
    let cx = Cx {
        app: &a,
        theme: a.theme,
        width: 28,
        active_turn: false,
    };
    let rls = render_turn_lines(&cx, turn);
    for rl in &rls {
        assert!(
            rl.line.width() <= 28,
            "row overflows 28: {}",
            rl.line.width()
        );
    }
    let all = rls
        .iter()
        .map(|rl| {
            rl.line
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("|");
    assert!(
        all.contains("hij"),
        "result tail clipped (no wrapping): {all}"
    );
}

#[test]
fn footer_shows_model_and_thinking() {
    let a = App::new(
        "openai/gpt-5.6-sol".to_string(),
        ThinkingLevel::XHigh,
        0,
        lofi_types::CompactionConfig::default(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("gpt-5.6-sol"));
    assert!(r.contains(":xhigh"));
}

#[test]
fn footer_hides_thinking_when_off() {
    let a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Off,
        0,
        lofi_types::CompactionConfig::default(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert_eq!(r, "openai/gpt-4o");
}

#[test]
fn footer_shows_ctx_after_usage() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 0.0,
        usage: Usage {
            input_tokens: 40_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    let r: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("context 40k/200k"), "footer: {r}");
}

#[test]
fn thinking_timing_is_restored_from_transcript() {
    let assistant_with_thinking = Message {
        role: Role::Assistant,
        blocks: vec![
            ContentBlock::Thinking {
                text: "hm".to_string(),
                signature: None,
            },
            ContentBlock::Text {
                text: "ok".to_string(),
            },
        ],
    };
    let events = sev_chain([
        msg(user("hi")),
        msg(assistant_with_thinking),
        SessionEventKind::ThinkingTiming { elapsed_ms: 1234 },
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let thinking = turns[0]
        .blocks
        .iter()
        .find_map(|b| match b {
            Block::Thinking(t) => Some(t),
            _ => None,
        })
        .expect("thinking block");
    assert_eq!(thinking.elapsed, Some(Duration::from_millis(1234)));
}

#[test]
fn turns_from_events_round_trip() {
    let events = sev_chain([
        msg(user("hello")),
        msg(assistant("hi there")),
        msg(user("again")),
        msg(assistant("yep")),
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[0].prompt, "hello");
    assert_eq!(turns[1].prompt, "again");
    assert!(matches!(turns[0].blocks[0], Block::Text(_)));
}

#[test]
fn selected_replay_preserves_prompt_when_compaction_parent_was_filtered_out() {
    let events = vec![
        SessionEvent {
            id: "prompt".into(),
            parent_id: Some("older".into()),
            kind: msg(user("current prompt")),
        },
        SessionEvent {
            id: "reply".into(),
            parent_id: Some("prompt".into()),
            kind: msg(assistant("current reply")),
        },
        SessionEvent {
            id: "marker".into(),
            parent_id: Some("filtered-checkpoint-copy".into()),
            kind: SessionEventKind::Compaction {
                summary: "summary".into(),
                first_kept_entry_id: "filtered-checkpoint-copy".into(),
                summarized_range: ["a".into(), "b".into()],
                checkpointed_tail: true,
                summarized: 1,
                represented: 1,
                kept: 1,
            },
        },
        SessionEvent {
            id: "continued".into(),
            parent_id: Some("marker".into()),
            kind: msg(assistant("continued reply")),
        },
    ];

    let turns = turns_from_selected_session_events(&events);
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].prompt, "current prompt");
    let rendered = turns[0]
        .blocks
        .iter()
        .filter_map(|block| match block {
            Block::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("current reply"));
    assert!(rendered.contains("continued reply"));
}

#[test]
fn checkpointed_tail_is_hidden_from_ui_but_used_for_model_resume() {
    let events = sev_chain([
        msg(user("old prompt")),
        msg(assistant("old reply")),
        msg(user("kept prompt")),
        msg(assistant("kept reply")),
        msg(user("kept prompt")),
        msg(assistant("kept reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: "e4".to_string(),
            summarized_range: ["e0".to_string(), "e1".to_string()],
            checkpointed_tail: true,
            summarized: 2,
            represented: 2,
            kept: 2,
        },
        msg(assistant("continued")),
    ]);

    let turns = turns_from_session_events(&events);
    assert_eq!(
        turns.len(),
        2,
        "checkpoint copies must not duplicate UI turns"
    );
    assert_eq!(turns[0].prompt, "old prompt");
    assert_eq!(turns[1].prompt, "kept prompt");

    let offsets: Vec<u64> = (0..events.len()).map(|i| i as u64 * 10).collect();
    assert_eq!(
        turn_byte_ranges_from_events(&events, &offsets, 80),
        vec![Some((0, 20)), Some((20, 80))]
    );

    let messages = messages_from_events(&events, &lofi_types::EditConfig::default());
    assert_eq!(messages.len(), 4);
    assert_eq!(user_text(&messages[0]), "SUMMARY");
    assert_eq!(user_text(&messages[1]), "kept prompt");
    assert_eq!(user_text(&messages[2]), "kept reply");
    assert_eq!(user_text(&messages[3]), "continued");
}

#[test]
fn turns_from_events_links_tool_results() {
    let messages = vec![
        user("run it"),
        Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "ok".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "t1".to_string(),
                    name: "exec".to_string(),
                    input: serde_json::json!({"cmd": "ls"}),
                },
            ],
        },
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "file.txt".to_string(),
                is_error: false,
            }],
        },
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        },
    ];
    let events: Vec<SessionEvent> = sev_chain(messages.into_iter().map(msg));
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let blocks = &turns[0].blocks;
    assert!(matches!(blocks[0], Block::Text(_)));
    let Block::Tool(t) = &blocks[1] else {
        unreachable!()
    };
    assert_eq!(t.result.as_deref(), Some("file.txt"));
    assert!(t.done);
    assert!(matches!(blocks[2], Block::Text(_)));
}

#[test]
fn turns_from_events_restores_timings() {
    let events = sev_chain([
        msg(user("run it")),
        msg(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "t1".to_string(),
                name: "exec".to_string(),
                input: serde_json::json!({"code": "return 1"}),
            }],
        }),
        msg(Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".to_string(),
                content: "1".to_string(),
                is_error: false,
            }],
        }),
        msg(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".to_string(),
            }],
        }),
        SessionEventKind::ToolTiming {
            tool_call_id: "t1".into(),
            elapsed_ms: 7,
        },
        SessionEventKind::TurnEnd {
            model: "proxy/gemini-3-flash · medium".into(),
            elapsed_ms: 2000,
            cost: 0.0,
            usage: Usage::default(),
        },
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let blocks = &turns[0].blocks;
    let Block::Tool(t) = &blocks[0] else {
        unreachable!()
    };
    assert_eq!(t.elapsed, Some(Duration::from_millis(7)));
    match blocks.last() {
        Some(Block::TurnEnd { label, elapsed }) => {
            assert_eq!(label, "proxy/gemini-3-flash:medium");
            assert_eq!(*elapsed, Duration::from_secs(2));
        }
        other => panic!("expected TurnEnd, got {other:?}"),
    }
}

#[test]
fn messages_from_events_excludes_failed_turn_branch() {
    // Build a tree: root chain [user1, assistant1, TurnEnd1], then a
    // failed turn chained linearly off TurnEnd1: [user2, assistant2,
    // TurnFailed]. The TurnFailed marker is the active leaf. The active
    // path INCLUDES the failed turn's messages (so the UI can render
    // them), but `messages_from_events` must EXCLUDE them from the
    // agent's history via the TurnFailed boundary — the model resumes
    // from the checkpoint (TurnEnd1), not the failed partial content.
    use lofi_core::session::store::active_path_from_leaf;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"meta\",\"version\":2,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n",
    )
    .unwrap();

    // First (successful) turn: two messages + a TurnEnd, chained from
    // the root so append_events assigns linear ids.
    let mut t1_events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("hi")),
        SessionEventKind::Message(assistant("hello")),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 10,
            cost: 0.0,
            usage: Usage::default(),
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut t1_events, None).unwrap();
    let checkpoint = t1_events.last().unwrap().id.clone();

    let mut t2_events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("oops")),
        SessionEventKind::Message(assistant("partial")),
        SessionEventKind::TurnFailed {
            model: "m".into(),
            elapsed_ms: 5,
            error: "boom".into(),
            cost: 0.01,
            usage: Usage::default(),
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut t2_events, Some(&checkpoint)).unwrap();

    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let path_idx = active_path_from_leaf(&events);
    assert_eq!(path_idx.len(), 6, "active path includes failed turn's msgs");
    assert!(
        matches!(&events[path_idx[0]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "hi"))
    );
    assert!(
        matches!(&events[path_idx[1]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant)
    );
    assert!(matches!(
        &events[path_idx[2]].kind,
        SessionEventKind::TurnEnd { .. }
    ));
    assert!(
        matches!(&events[path_idx[3]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "oops"))
    );
    assert!(
        matches!(&events[path_idx[4]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant)
    );
    assert!(matches!(
        &events[path_idx[5]].kind,
        SessionEventKind::TurnFailed { .. }
    ));

    let msgs = messages_from_events(&events, &lofi_types::EditConfig::default());
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
}

#[test]
fn messages_from_events_prepends_compaction_summary() {
    use lofi_core::session::store;
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"meta\",\"version\":2,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n",
    )
    .unwrap();

    let mut events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("old")),
        SessionEventKind::Message(user("kept-prompt")),
        SessionEventKind::Message(assistant("kept-reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: String::new(), // patched after append
            summarized_range: [String::new(), String::new()],
            checkpointed_tail: false,
            summarized: 1,
            represented: 1,
            kept: 2,
        },
        SessionEventKind::Message(assistant("continued")),
    ]
    .into_iter()
    .map(|kind| SessionEvent {
        id: String::new(),
        parent_id: None,
        kind,
    })
    .collect();
    test_append_events(&path, &mut events, None).unwrap();
    let mut events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let kept_prompt_id = events
        .iter()
        .find(|e| matches!(&e.kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "kept-prompt")))
        .unwrap()
        .id
        .clone();
    for e in &mut events {
        if let SessionEventKind::Compaction {
            first_kept_entry_id,
            ..
        } = &mut e.kind
        {
            *first_kept_entry_id = kept_prompt_id.clone();
        }
    }

    let msgs = messages_from_events(&events, &lofi_types::EditConfig::default());
    assert_eq!(msgs.len(), 4);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");
    assert_eq!(user_text(&msgs[1]), "kept-prompt");
    assert_eq!(msgs[2].role, Role::Assistant);
    assert_eq!(msgs[3].role, Role::Assistant);
}

#[test]
fn messages_from_events_compact_all_does_not_restore_old_messages() {
    let events = sev_chain([
        msg(user("old prompt")),
        msg(assistant("old reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: String::new(),
            summarized_range: ["e0".to_string(), "e1".to_string()],
            checkpointed_tail: false,
            summarized: 2,
            represented: 2,
            kept: 0,
        },
        msg(assistant("continued")),
    ]);

    let msgs = messages_from_events(&events, &lofi_types::EditConfig::default());
    assert_eq!(msgs.len(), 2);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");
    assert_eq!(msgs[1].role, Role::Assistant);
    assert_eq!(user_text(&msgs[1]), "continued");
}

#[test]
fn messages_from_events_reads_kept_tail_verbatim_on_resume() {
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let exec_call_stub = |id: &str, eid: &str| Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": format!("[code cleared — re-expand with lofi.result(\"{eid}\")]" )}),
        }],
    };
    let exec_result_stub = |id: &str, eid: &str| Message {
        role: Role::User,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: format!("[exec result cleared — re-expand with lofi.result(\"{eid}\")]"),
            is_error: false,
        }],
    };
    let exec_call_full = |id: &str| Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": "return 1"}),
        }],
    };
    let exec_result_full = |id: &str, out: &str| Message {
        role: Role::User,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: out.to_string(),
            is_error: false,
        }],
    };

    let events: Vec<SessionEvent> = sev_chain([
        msg(user("old prompt")),
        msg(exec_call_stub("t1", "e1")),
        msg(exec_result_stub("t1", "e2")),
        msg(exec_call_stub("t2", "e3")),
        msg(exec_result_full("t2", "out-2")), // most recent kept-tail result, kept verbatim by edit_tail
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: "e1".to_string(),
            summarized_range: [String::new(), String::new()],
            checkpointed_tail: false,
            summarized: 1,
            represented: 1,
            kept: 4,
        },
        msg(exec_call_full("t3")),
        msg(exec_result_full("t3", "post-compaction-result")),
    ]);

    let msgs = messages_from_events(&events, &lofi_types::EditConfig::default());
    assert_eq!(msgs.len(), 7);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");

    let ContentBlock::ToolResult { content, .. } = &msgs[4].blocks[0] else {
        panic!()
    };
    assert_eq!(content, "out-2");
    let ContentBlock::ToolResult { content, .. } = &msgs[2].blocks[0] else {
        panic!()
    };
    assert!(
        content.contains("lofi.result"),
        "older kept-tail result should be the stub written by compact_now, got {content}"
    );

    let ContentBlock::ToolResult { content, .. } = &msgs[6].blocks[0] else {
        panic!()
    };
    assert_eq!(content, "post-compaction-result");
    let ContentBlock::ToolUse { input, .. } = &msgs[5].blocks[0] else {
        panic!()
    };
    let code = input.get("code").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(code, "return 1");
}

fn user_text(m: &lofi_types::Message) -> &str {
    match &m.blocks[..] {
        [lofi_types::ContentBlock::Text { text }] => text,
        _ => "",
    }
}
#[test]
fn compact_count_formats() {
    assert_eq!(compact_count(0), "0");
    assert_eq!(compact_count(500), "500");
    assert_eq!(compact_count(119_000), "119k");
    assert_eq!(compact_count(200_000), "200k");
    assert_eq!(compact_count(2_000_000), "2M");
    assert_eq!(compact_count(9_700_000), "9.7M");
}

#[test]
fn fmt_cost_two_decimals() {
    assert_eq!(fmt_cost(0.0), "$0.00");
    assert_eq!(fmt_cost(0.01), "$0.01");
    assert_eq!(fmt_cost(81.0), "$81.00");
    assert_eq!(fmt_cost(81.40), "$81.40");
    assert_eq!(fmt_cost(81.45), "$81.45");
}

#[test]
fn abbreviate_path_progressive() {
    let home = dirs::home_dir().unwrap();
    let p = home.join("Dev/src/proj");
    assert_eq!(abbreviate_path(&p, 100), "~/Dev/src/proj");
    assert_eq!(abbreviate_path(&p, 10), "~/D/s/proj");
    assert_eq!(abbreviate_path(&p, 4), "proj");
    let p2 = home.join("Dev/~sirn/lofi");
    assert_eq!(abbreviate_path(&p2, 12), "~/D/~s/lofi");
}

#[test]
fn footer_and_header_show_cost_and_usage() {
    let mut a = App::new(
        "proxy/deepseek-v4-flash".to_string(),
        ThinkingLevel::Off,
        200_000,
        lofi_types::CompactionConfig::default(),
    );
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 18.0,
        usage: Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        },
    });
    let footer: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(footer.contains("↑1M ↓1M"), "footer: {footer}");
    assert!(footer.contains("context 2M/200k"), "footer: {footer}");
    let cost: String = a
        .render_footer_cost()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(cost.contains("$18.00"), "cost: {cost}");
    let header: String = a
        .render_header_line(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(header.contains("lofi"), "header: {header}");
    assert!(
        !header.contains('$'),
        "header should not show cost: {header}"
    );
}

#[test]
fn footer_shows_cache_percentage_without_cumulative_cache_counts() {
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::TurnEnd {
        model: "m".into(),
        elapsed_ms: 0,
        cost: 1.0,
        usage: Usage {
            input_tokens: 100_000,
            output_tokens: 50_000,
            cache_read_tokens: 800_000,
            cache_write_tokens: 200_000,
        },
    });
    let footer: String = a
        .render_footer_left(120)
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(footer.contains("73% cached"), "footer: {footer}");
    assert!(!footer.contains("cache ↑"), "footer: {footer}");
}

#[test]
fn retry_notice_is_transient_status_and_resets_on_success() {
    let mut a = app();
    push_turn(&mut a);
    let before = a.turns[0].blocks.len();

    a.apply_event(AgentEvent::RetryStart {
        attempt: 2,
        max_attempts: 10,
        delay_ms: 42_000,
        error: "provider error: HTTP 500 Internal Server Error".to_string(),
    });

    assert_eq!(a.retry_badge().as_deref(), Some("Retry: 2 of 10"));
    assert_eq!(
        a.turns[0].blocks.len(),
        before,
        "retry status must not enter the transcript"
    );

    a.apply_event(AgentEvent::RetryEnd {
        success: true,
        attempt: 2,
        final_error: None,
    });
    assert!(a.retry_badge().is_none());
    assert_eq!(a.turns[0].blocks.len(), before);
}

#[test]
fn queue_badge_shows_preview_and_count() {
    let mut a = app();
    assert!(a.queue_badge().is_none());
    a.prompt_queue.push("fix the bug".to_string());
    let badge = a.queue_badge().expect("badge for one item");
    assert!(badge.contains("Queue: fix the bug"), "badge: {badge}");
    a.prompt_queue.push("also add tests".to_string());
    let badge = a.queue_badge().expect("badge for two items");
    assert!(badge.contains("Queue: fix the bug (+1)"), "badge: {badge}");
}

#[test]
fn queue_badge_truncates_long_prompt() {
    let mut a = app();
    let long = "x".repeat(100);
    a.prompt_queue.push(long);
    let badge = a.queue_badge().expect("badge");
    assert!(
        badge.ends_with("…"),
        "badge should end with ellipsis: {badge}"
    );
    assert!(
        !badge.contains(&"x".repeat(50)),
        "badge should be truncated: {badge}"
    );
}

#[test]
fn alt_up_restores_queued_prompt_lifo() {
    let mut a = app();
    let mut run = None;
    a.prompt_queue.push("first prompt".to_string());
    a.prompt_queue.push("second prompt".to_string());
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "second prompt");
    assert_eq!(a.prompt_queue.len(), 1);
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "first prompt");
    assert!(a.prompt_queue.is_empty());
}

#[test]
fn no_model_submit_surfaces_hint_without_running() {
    let mut a = app();
    a.no_models_hint = Some("set OPENAI_API_KEY".to_string());
    a.input = "hello".to_string();
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Enter,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ));
    let mut run = None;
    handle_event(&ev, &mut a, None, &mut run);
    assert!(run.is_none(), "no run should be started without a model");
    assert!(a.run.is_none());
    let last = a.turns.last().unwrap();
    assert_eq!(last.prompt, "hello");
    assert!(
        last.blocks
            .iter()
            .any(|b| matches!(b, Block::Error(m) if m == "set OPENAI_API_KEY")),
        "the hint should be attached to the turn"
    );
}

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

fn ctrl_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    ))
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

fn plain_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ))
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
    a.prompt_queue = vec!["steer first".to_string(), "follow up".to_string()];
    a.set_input("draft".to_string());
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    let mut run = Some(RunHandle {
        handle: tokio::spawn(std::future::pending()),
        rx,
        cancel: cancel.clone(),
        preempt: Arc::new(AtomicBool::new(false)),
        user_bash: None,
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
        user_bash: None,
    });

    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);

    assert!(!cancel.load(Ordering::Relaxed));
    assert!(a.slash_complete.is_none());
    assert_eq!(a.input, "/");
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
fn insert_str_normalizes_line_endings() {
    let mut a = app();
    a.insert_str("a\r\nb\rc");
    assert_eq!(a.input, "a\nb\nc");
}

#[test]
fn model_picker_open_preselects_current() {
    let mut a = app(); // model_label = "openai/gpt-4o"
    a.model_choices = vec![
        lofi_types::ModelChoice {
            provider: "anthropic".into(),
            id: "claude".into(),
            name: "Claude".into(),
            thinking_levels: vec![ThinkingLevel::Medium],
            supports_image: false,
            context_window: Some(200_000),
        },
        lofi_types::ModelChoice {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            name: String::new(),
            thinking_levels: vec![],
            supports_image: false,
            context_window: Some(128_000),
        },
    ];
    a.open_model_picker();
    let picker = a.model_picker.as_ref().unwrap();
    assert_eq!(picker.choices.len(), 2);
    assert_eq!(picker.selected, 1);
    assert!(a.modal_open());
}

#[test]
fn model_picker_open_empty_notifies() {
    let mut a = app();
    a.model_choices = Vec::new();
    a.open_model_picker();
    assert!(a.model_picker.is_none());
    assert!(a.notify.is_some());
}

#[test]
fn model_picker_confirm_sets_pending_switch() {
    let mut a = app();
    a.model_choices = vec![
        lofi_types::ModelChoice {
            provider: "anthropic".into(),
            id: "claude".into(),
            name: "Claude".into(),
            thinking_levels: vec![],
            supports_image: false,
            context_window: None,
        },
        lofi_types::ModelChoice {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            name: String::new(),
            thinking_levels: vec![],
            supports_image: false,
            context_window: None,
        },
    ];
    a.open_model_picker();
    a.model_picker.as_mut().unwrap().selected = 0;
    a.model_picker_confirm();
    assert_eq!(a.pending_model_switch.as_deref(), Some("anthropic/claude"));
    assert!(a.model_picker.is_none());
}

#[test]
fn apply_model_switch_updates_label_and_ctx_limit() {
    let mut a = app();
    a.ctx_limit = 0; // falls back to DEFAULT_CTX_LIMIT until a model reports one
    let model = lofi_types::Model {
        id: "claude".into(),
        name: "Claude".into(),
        provider: "anthropic".into(),
        api: lofi_types::Api::AnthropicMessages,
        reasoning: true,
        thinking: ThinkingLevel::XHigh,
        supports_image: true,
        context_window: Some(200_000),
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    };
    a.apply_model_switch(&model, ThinkingLevel::XHigh);
    assert_eq!(a.model_label, "anthropic/claude");
    assert_eq!(a.thinking_label.as_deref(), Some(":xhigh"));
    assert_eq!(a.ctx_limit, 200_000);
    assert_eq!(a.thinking, ThinkingLevel::XHigh);
}

#[test]
fn apply_model_switch_with_no_context_window_uses_default() {
    let mut a = app();
    a.ctx_limit = 100_000;
    let model = lofi_types::Model {
        id: "local".into(),
        name: "Local".into(),
        provider: "ollama".into(),
        api: lofi_types::Api::OpenAiResponses,
        reasoning: false,
        thinking: ThinkingLevel::Off,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    };
    a.apply_model_switch(&model, ThinkingLevel::Off);
    assert_eq!(a.ctx_limit, DEFAULT_CTX_LIMIT);
}

#[test]
fn thinking_picker_open_preselects_current() {
    let mut a = app(); // model_label = "openai/gpt-4o", thinking = Medium
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        supports_image: false,
        context_window: None,
    }];
    a.open_thinking_picker();
    let picker = a.thinking_picker.as_ref().unwrap();
    assert_eq!(
        picker.levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Medium,
            ThinkingLevel::High
        ]
    );
    assert_eq!(picker.selected, 1);
    assert!(a.modal_open());
}

#[test]
fn thinking_picker_open_no_levels_notifies() {
    let mut a = app();
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        supports_image: false,
        context_window: None,
    }];
    a.open_thinking_picker();
    assert!(a.thinking_picker.is_none());
    assert!(a.notify.is_some());
}

#[test]
fn thinking_picker_confirm_sets_pending_switch() {
    let mut a = app(); // model_label = "openai/gpt-4o"
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        supports_image: false,
        context_window: None,
    }];
    a.open_thinking_picker();
    a.thinking_picker.as_mut().unwrap().selected = 2;
    a.thinking_picker_confirm();
    assert_eq!(
        a.pending_model_switch.as_deref(),
        Some("openai/gpt-4o:high")
    );
    assert!(a.thinking_picker.is_none());
}

#[test]
fn resume_model_switch_when_model_differs() {
    let mut a = app(); // model_label = "openai/gpt-4o", thinking = Medium
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "anthropic".into(),
        id: "claude".into(),
        name: String::new(),
        thinking_levels: vec![],
        supports_image: false,
        context_window: None,
    }];
    let events = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "anthropic/claude:high".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
        },
    }];
    assert_eq!(
        a.resume_model_switch(&events).as_deref(),
        Some("anthropic/claude:high")
    );
}

#[test]
fn resume_model_switch_none_when_same_model() {
    let mut a = app();
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        supports_image: false,
        context_window: None,
    }];
    let events = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "openai/gpt-4o:medium".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
        },
    }];
    assert!(a.resume_model_switch(&events).is_none());
}

#[test]
fn resume_model_switch_none_when_no_choices_or_no_turn() {
    let mut a = app();
    a.model_choices = Vec::new();
    let events = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "anthropic/claude:high".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
        },
    }];
    assert!(a.resume_model_switch(&events).is_none());
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "x".into(),
        id: "y".into(),
        name: String::new(),
        thinking_levels: vec![],
        supports_image: false,
        context_window: None,
    }];
    let no_turn = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "hi".into() }],
        }),
    }];
    assert!(a.resume_model_switch(&no_turn).is_none());
}

#[test]
fn frozen_cache_invalidates_on_width_change() {
    let mut a = app();
    a.turns.push(Turn {
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
    let h_wide = a.frozen_heights[0];
    assert!(a.frozen_render.get(0).is_some());
    assert!(
        h_wide < h_narrow,
        "frozen cache should re-wrap at the new width: narrow={h_narrow} wide={h_wide}"
    );
}

#[test]
fn resize_reanchors_scrolled_up_view_instead_of_snapping_to_bottom() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    // A long prompt that wraps to many lines when narrow and far fewer when
    // wide, so the re-wrap materially shrinks `total`/`base` on resize.
    a.turns.push(Turn {
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
    a.apply_event(AgentEvent::TurnStart {
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

#[test]
fn resumed_compaction_restores_summarized_message_count() {
    // Exact regression: a hard compact kept no tail, its silent continuation
    // produced only two messages, and the session was then resumed with -c.
    // The marker's summarized count must survive resume so /compact does not
    // treat the restored summary as a single message.
    let dir = tempfile::tempdir().unwrap();
    let session_store = store::SessionStore::new(dir.path().join("sessions"));
    let path = session_store
        .create_cursor(std::path::Path::new("/tmp/resumed-compact"), &"p/m".into())
        .unwrap()
        .path()
        .to_path_buf();

    let mut old: Vec<SessionEvent> = (0..5)
        .map(|i| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&format!("old {i}"))),
        })
        .collect();
    test_append_events(&path, &mut old, None).unwrap();
    test_append_compaction(
        &path,
        &[],
        None,
        "previous summary",
        &[old[0].id.clone(), old[4].id.clone()],
        store::CompactionCounts {
            summarized: old.len(),
            represented: old.len(),
            kept: 0,
        },
    )
    .unwrap();

    let large = "continued work ".repeat(2_000);
    let mut continuation = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&large)),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&large)),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage {
                    input_tokens: 113_000,
                    ..Usage::default()
                },
            },
        },
    ];
    test_append_events(&path, &mut continuation, None).unwrap();

    let resumed = store::SessionCursor::open(path.clone()).unwrap();
    let index = resumed.snapshot().unwrap().index;
    let mut config = lofi_types::CompactionConfig::default();
    config.auto.max_context_tokens = Some(100_000);
    let mut a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        0,
        config,
    );
    a.session.cursor = Some(resumed.clone());
    a.lifecycle.restore_history(&resumed, &index).unwrap();
    restore_compaction_from_index(&mut a, &resumed, &index);

    assert_eq!(a.lifecycle.history_stats().messages, 3);
    assert_eq!(a.status_usage.unwrap().input_tokens, 113_000);
    a.apply_event(AgentEvent::RoundUsage {
        cost: 0.0,
        usage: Usage {
            input_tokens: 113_000,
            ..Usage::default()
        },
    });
    a.maybe_auto_compact();
    assert!(
        a.compacted,
        "resume must evaluate the next settled state and soft compaction must not reuse the hard cooldown"
    );

    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let marker = events
        .iter()
        .rev()
        .find_map(|event| match &event.kind {
            SessionEventKind::Compaction {
                summarized,
                represented,
                ..
            } => Some((*summarized, *represented)),
            _ => None,
        })
        .unwrap();
    assert_eq!(marker, (2, 7));
}

#[test]
fn resumed_compaction_stays_on_its_cursor_when_a_sibling_appends_later() {
    // Regression: resume snapshots branch A as its logical cursor. Another
    // writer then appends branch B to the same physical JSONL file before the
    // resumed app compacts. Compaction must read and checkpoint A, not infer
    // its thread from physical EOF (which now belongs to B).
    let dir = tempfile::tempdir().unwrap();
    let session_store = store::SessionStore::new(dir.path().join("sessions"));
    let path = session_store
        .create_cursor(
            std::path::Path::new("/tmp/resume-branch-compact"),
            &"p/m".into(),
        )
        .unwrap()
        .path()
        .to_path_buf();

    let mut branch_a: Vec<SessionEvent> = (0..4)
        .flat_map(|i| {
            [
                SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: msg(user(&format!("branch A prompt {i}"))),
                },
                SessionEvent {
                    id: String::new(),
                    parent_id: None,
                    kind: msg(assistant(&format!("branch A reply {i}"))),
                },
            ]
        })
        .collect();
    test_append_events(&path, &mut branch_a, None).unwrap();
    let branch_a_leaf = branch_a.last().unwrap().id.clone();

    let resumed = store::SessionCursor::open(path.clone()).unwrap();
    let resumed_index = resumed.snapshot().unwrap().index;
    let mut a = app();
    a.session.cursor = Some(resumed.clone());
    a.lifecycle
        .restore_history(&resumed, &resumed_index)
        .unwrap();

    let root = branch_a[0].id.clone();
    let mut branch_b = [
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(user("WRONG SIBLING PROMPT")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant("WRONG SIBLING REPLY")),
        },
    ];
    test_append_events(&path, &mut branch_b, Some(&root)).unwrap();
    let sibling_leaf = branch_b[1].id.clone();
    assert_eq!(
        store::SessionCursor::open(path.clone()).unwrap().leaf_id(),
        Some(sibling_leaf.clone())
    );

    assert!(a.compact_now(), "branch A has enough history to compact");
    let marker_leaf = a
        .session
        .cursor
        .as_ref()
        .and_then(store::SessionCursor::leaf_id)
        .expect("compaction advances the resumed cursor");
    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let marker_index = events
        .iter()
        .position(|event| event.id == marker_leaf)
        .expect("cursor points at persisted compaction marker");
    let lineage = store::active_path(&events, &marker_leaf);

    assert!(matches!(
        events[marker_index].kind,
        SessionEventKind::Compaction { .. }
    ));
    assert!(
        lineage.iter().any(|&i| events[i].id == branch_a_leaf),
        "compaction must descend from the branch captured by resume"
    );
    assert!(
        lineage.iter().all(|&i| events[i].id != sibling_leaf),
        "later physical-EOF sibling must not leak into resumed compaction"
    );
    let SessionEventKind::Compaction { summary, .. } = &events[marker_index].kind else {
        unreachable!()
    };
    assert!(summary.contains("branch A prompt"));
    assert!(!summary.contains("WRONG SIBLING"));
}

#[test]
fn resume_does_not_restore_usage_measured_before_latest_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let session_store = store::SessionStore::new(dir.path().join("sessions"));
    let path = session_store
        .create_cursor(
            std::path::Path::new("/tmp/stale-compact-usage"),
            &"p/m".into(),
        )
        .unwrap()
        .path()
        .to_path_buf();

    let mut old = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(user("old prompt")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant("old response")),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage {
                    input_tokens: 149_000,
                    ..Usage::default()
                },
            },
        },
    ];
    test_append_events(&path, &mut old, None).unwrap();
    test_append_compaction(
        &path,
        &[],
        None,
        "summary",
        &[old[0].id.clone(), old[1].id.clone()],
        store::CompactionCounts {
            summarized: 2,
            represented: 2,
            kept: 0,
        },
    )
    .unwrap();
    let mut continuation = vec![SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: msg(assistant("partial continuation")),
    }];
    test_append_events(&path, &mut continuation, None).unwrap();

    let resumed = store::SessionCursor::open(path.clone()).unwrap();
    let index = resumed.snapshot().unwrap().index;
    let mut a = app();
    restore_compaction_from_index(&mut a, &resumed, &index);

    assert_eq!(a.status_usage, None);
    assert!(!a.settled_usage_fresh);
    assert!(a.compacted);
}

#[test]
fn working_status_is_replaced_in_place_by_done_status() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen_rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        let area = buf.area;
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    let mut a = app();
    a.apply_event(AgentEvent::TurnStart {
        prompt: "show the status transition".to_string(),
    });
    a.apply_event(AgentEvent::Text(
        (0..40)
            .map(|i| format!("finished response line {i}"))
            .collect::<Vec<_>>()
            .join("\n"),
    ));
    a.run = Some(0);
    a.run_start = Some(Instant::now());
    a.apply_event(AgentEvent::TurnEnd {
        model: "openai/gpt-4o".into(),
        elapsed_ms: 1_500,
        cost: 0.0,
        usage: Usage::default(),
    });

    let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let active = screen_rows(&term);
    let working_y = active
        .iter()
        .position(|row| row.contains("Working for"))
        .expect("working status row");
    assert!(
        active.iter().all(|row| !row.contains("Done in")),
        "terminal status must stay buffered until settlement: {active:#?}"
    );

    a.run_finished();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let settled = screen_rows(&term);
    let done_y = settled
        .iter()
        .position(|row| row.contains("Done in 1.5s with openai/gpt-4o"))
        .expect("done status row");
    assert_eq!(
        done_y, working_y,
        "Done should replace Working on the same physical row"
    );
}

#[test]
fn auto_evaluation_dialog_renders_elapsed_state_and_ask_reason() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(term: &Terminal<TestBackend>) -> String {
        let buf = term.backend().buffer();
        let area = buf.area;
        (area.top()..area.bottom())
            .map(|y| {
                (area.left()..area.right())
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(
                "
",
            )
    }

    let mut a = app();
    let (mut req, _response) = confirm_request("rm generated.txt");
    req.reason = std::sync::Arc::new(std::sync::Mutex::new(
        lofi_core::ConfirmReason::AutoEvaluating {
            started_at: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(4))
                .unwrap(),
        },
    ));
    let reason = req.reason.clone();
    a.pending_confirms.push(req);
    let mut term = Terminal::new(TestBackend::new(90, 28)).unwrap();

    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let evaluating = screen(&term);
    assert!(
        evaluating.contains("Auto evaluation for 4s..."),
        "{evaluating}"
    );
    assert!(!evaluating.contains("allow or deny"), "{evaluating}");

    *reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = lofi_core::ConfirmReason::AutoAsk {
        reason: "command deletes a file".to_string(),
    };
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let asking = screen(&term);
    assert!(
        asking.contains("Auto evaluation asks: command deletes a file"),
        "{asking}"
    );
}

#[test]
fn collapsed_cache_round_trip_preserves_native_preview_rendering() {
    use crate::tui::view::blocks::{compact_native_previews, render_turn_lines};
    use crate::tui::view::component::Cx;

    let mut a = app();
    a.turns.push(Turn {
        prompt: "run it".to_string(),
        blocks: vec![Block::Tool(ToolCall {
            id: "exec-1".to_string(),
            name: "exec".to_string(),
            input: r#"await lofi.bash({ cmd: "printf test" })"#.to_string(),
            label: None,
            native: vec![NativeTool {
                id: 1,
                name: "bash".to_string(),
                args: "printf test".to_string(),
                result: Some(
                    serde_json::json!({
                        "output": "one\ntwo\nthree\nfour",
                        "duration_ms": 10,
                    })
                    .to_string(),
                ),
                preview: None,
                is_error: false,
                done: true,
            }],
            result: Some("{\"value\":null}".to_string()),
            result_committed: true,
            is_error: false,
            done: true,
            elapsed: Some(Duration::from_millis(10)),
        })],
    });
    let render = |app: &App, turn: &Turn| {
        let cx = Cx {
            app,
            theme: app.theme,
            width: 80,
            active_turn: false,
        };
        render_turn_lines(&cx, turn)
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
    let expected = render(&a, &a.turns[0]);
    let mut compact = a.turns[0].clone();
    compact_native_previews(&mut compact);
    let mut cache = CollapsedTurnCache::new();
    cache.insert(0, &compact);
    let restored = cache.get(0).unwrap();
    assert_eq!(render(&a, &restored), expected);
    assert!(restored.blocks.iter().all(|block| match block {
        Block::Tool(tool) => tool.native.iter().all(|native| native.result.is_none()),
        _ => true,
    }));
}
