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
    app.turns.push(Turn {
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
    let mut a = app();
    a.ctx_limit = 100_000;
    a.compaction.auto.context_ratio = Some(0.5);
    a.compaction.reserved_context_tokens = 20_000;

    assert_eq!(a.compaction.soft_threshold(a.ctx_limit), Some(50_000));
    assert_eq!(a.derive_compact_budget(), 25_000);
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