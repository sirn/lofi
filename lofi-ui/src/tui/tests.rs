#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::many_single_char_names)]
// Cost/format tests assert exact computed float values.
#![allow(clippy::float_cmp)]

use super::*;
use std::sync::Arc;

use lofi_types::{ContentBlock, Role, Usage};

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

/// The native tool header shows a parenthetical line-range suffix for `read`
/// and a `(took Ns)` suffix for `bash`, derived from the structured result.
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

/// Render assistant text containing inline markdown and collect the content
/// spans (skipping the 2-space margin) so tests can assert on styles.
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
        .find(|s| s.content == "inline_spans")
        .expect("code span");
    // Code spans have an inline_bg background; plain text does not.
    assert!(code.style.bg.is_some(), "code should have bg: {code:?}");
}

#[test]
fn inline_markdown_code_wraps_across_lines() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    // Inline code that spans a wrap boundary must still be rendered as code
    // on every wrapped line.  Previously, wrapping split the text first and
    // each segment was parsed independently, leaving backtick markers visible
    // and losing the code style.
    let md = "Run `git rebase --interactive upstream main` now";
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(md.to_string()));
    let turn = &a.turns[0];
    // Narrow enough to force the code span to wrap.
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
    // No backtick markers should survive in the output.
    assert!(
        !body.contains('`'),
        "backtick markers should be stripped: {body}"
    );
    // Every word from the code span should carry the code style (bg set).
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
    // Find the assistant text line (source contains the markdown).
    let rl = rls
        .iter()
        .find(|r| {
            r.raw
                .as_ref()
                .is_some_and(|r| r.source.contains("**bold**"))
        })
        .expect("a line with a raw map");
    let raw = rl.raw.as_ref().unwrap();
    // Whole content → full source.
    let start = *raw.map.first().unwrap();
    let end = *raw.map.last().unwrap();
    assert_eq!(&raw.source[start..end], "**bold** text");
    // The visible "bold" (display positions 0..4) snaps to source `**bold**`.
    let s = raw.map[0];
    let e = raw.map[4];
    assert_eq!(&raw.source[s..e], "**bold**");
}

/// Populate the app's log fields from rendered lines so yank tests exercise
/// the same path as `feed_segment`.
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
    // Find the heading row (source contains "##").
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
    // Find the header data row (source starts with `| Name`).
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
    // Find a data row.
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
    // The header-separator border (├─┼─┤) carries the markdown separator
    // line so yanking it recovers `|------|-----|`.
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
    // Find the top border row (raw exists but source is empty).
    let border = a
        .log_vis
        .iter()
        .position(|v| v.raw.as_ref().is_some_and(|r| r.source.is_empty()))
        .expect("border row");
    a.nav_cursor = border;
    // Non-separator border rows carry no markdown source — yank should
    // return nothing, not the rendered box-drawing characters.
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
    // Find the nested item row (source starts with "  -").
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
    // Find the indented line (source starts with "  return").
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
    // Select all content lines (skip marker line 0 and gap line 1).
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
    // Every quote line should have the bar character.
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
    // The bare > should produce a line with just the bar (not dropped).
    assert!(
        quote_lines
            .iter()
            .any(|s| s.trim() == "▎" || s.ends_with("▎ ")),
        "Empty quote line should render bar only: {quote_lines:?}"
    );
    assert!(
        quote_lines
            .iter()
            .any(|s| s.contains("Line three") && s.contains("▎")),
        "Line three should have bar: {quote_lines:?}"
    );
    // Yanking the full quote should preserve the bare ">" line.
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
    // Select all content lines (skip turn marker at line 0 and gap at 1).
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
    // Select all lines.
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
    // Select the entire table top to bottom.
    a.sel = Some(Selection {
        start: (2, 0),
        end: (n - 1, a.log_vis[n - 1].rendered.chars().count()),
    });
    let text = a.selection_text().expect("selection text");
    // Non-separator borders are suppressed; the header, separator, and
    // data rows carry markdown source, joined by `\n`.
    assert_eq!(text, "| Name | Age |\n|------|-----|\n| Ada | 36 |");
    // No box-drawing characters survive.
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
    // All content must be present (wrapped, not truncated). Words that fit
    // survive intact; long unbreakable tokens hard-break across rows.
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
    // Right-aligned column: `--:` → numbers should be right-padded.
    let md = "| Item | Count |\n|------|------:|\n| a    | 1     |\n| bb   | 22    |";
    let spans = render_text_spans(md);
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    // The right-aligned cells should have leading spaces before the numbers.
    assert!(body.contains(" 1"), "right-aligned 1: {body}");
    assert!(body.contains("22"), "right-aligned 22: {body}");
}

#[test]
fn inline_markdown_table_renders_inline_formatting() {
    use ratatui::style::Modifier;
    // Bold and code formatting inside table cells should be rendered with
    // the appropriate styles, and markers stripped from the visible text.
    let md = "| Name | Type |\n|------|------|\n| **bold** | `code` |";
    let spans = render_text_spans(md);
    let body: String = spans.iter().map(|s| s.content.as_ref()).collect();
    // Markers should be stripped.
    assert!(
        !body.contains("**"),
        "bold markers should be stripped: {body}"
    );
    assert!(
        !body.contains('`'),
        "code markers should be stripped: {body}"
    );
    // Content should be present.
    assert!(body.contains("bold"), "bold text should be present: {body}");
    assert!(body.contains("code"), "code text should be present: {body}");
    // The bold cell should have BOLD modifier.
    let bold_span = spans
        .iter()
        .find(|s| s.content == "bold")
        .expect("bold span");
    assert!(
        bold_span.style.add_modifier.contains(Modifier::BOLD),
        "bold cell should be BOLD: {bold_span:?}"
    );
    // The code cell should have the code style (fg = info).
    let code_span = spans
        .iter()
        .find(|s| s.content == "code")
        .expect("code span");
    assert!(
        code_span.style.fg.is_some(),
        "code cell should have fg color: {code_span:?}"
    );
}

/// A numbered `read` line whose body is empty must still carry its line
/// number as decoration with an empty content range, so the Navigate
/// cursor overlay preserves it instead of treating it as a blank line.
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
    // A read (hidden in non-verbose), mutating tools bash/write/edit (kept),
    // and a failed read whose error stays visible even when read results hide.
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
    // Non-verbose: read body hidden; bash body and the written write/edit
    // content, the error, and every tool header stay visible.
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
    // Verbose: the hidden read body comes back.
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
    // A successful exec whose returned value is a short summary. In
    // non-verbose the result body hides (the native-tool lines already showed
    // the work) but the Succeed status header stays; verbose brings it back.
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
    // ^ and 0 land on the first content char.
    assert_eq!(a.first_nonblank_col(), 2);
    // $ lands on the last content char.
    a.nav_set_col(usize::MAX);
    assert_eq!(a.nav_col, 9);
    // w from "aa" -> "bb" start.
    a.nav_col = 2;
    assert_eq!(a.nav_word_target(WordMotion::NextStart { big: false }), 5);
    // e from "aa" -> end of "aa".
    a.nav_col = 2;
    assert_eq!(a.nav_word_target(WordMotion::NextEnd { big: false }), 3);
    // b from "bb" -> "aa" start.
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
    // w from "a" lands on "." (punctuation is its own word).
    assert_eq!(a.nav_word_target(WordMotion::NextStart { big: false }), 3);
    // W from "a" lands on "c" (only whitespace separates, so "a.b" is one WORD).
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
    // At most the Stack separator plus one exec-padding fill (= 2) should
    // ever appear consecutively; whitespace-only text blocks leave nothing.
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

/// Build a `Vec<SessionEvent>` from kinds, assigning fresh ids and
/// chaining each event's `parent_id` to the previous one (root = first).
/// Mirrors what `store::append_events` does on disk, so
/// `messages_from_events` / `replay_session_events` see a valid tree.
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

/// Wrap a message as a `Message` session-event kind.
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
    // TurnEnd accumulates into the footer totals (input + output).
    assert_eq!(a.total_in, 10);
    assert_eq!(a.total_out, 20);
}

#[test]
fn round_usage_updates_totals_per_round() {
    let mut a = app();
    a.apply_event(AgentEvent::TurnStart { prompt: "p".into() });
    // Two rounds within one turn: each carries the turn's cumulative
    // cost and that round's usage.
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
    // Footer shows the live running cost (base cost + turn_cost).
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
    // Tokens accumulate per round; turn_cost is replaced with the
    // turn's new cumulative cost.
    assert_eq!(a.total_in, 300);
    assert_eq!(a.total_out, 130);
    assert!((a.turn_cost - 0.03).abs() < 1e-9);
    assert!((a.cost + a.turn_cost - 0.03).abs() < 1e-9);

    // TurnEnd folds turn_cost into cost and does NOT re-add tokens.
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
    // Tokens unchanged: TurnEnd skipped re-accumulation.
    assert_eq!(a.total_in, 300);
    assert_eq!(a.total_out, 130);
}

#[test]
fn turn_end_folds_bundled_totals_on_resume_path() {
    // The resume path has no RoundUsage events, so TurnEnd must apply
    // its bundled cost/usage as before.
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
    // No spaces to break at: hard-break at the column width.
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
    // End of line lands at the end of the last row.
    a.input_cursor = "hello world".len();
    assert_eq!(a.input_cursor_pos(7), (1, 5));
}

#[test]
fn cursor_up_recalls_at_first_cell() {
    let mut a = app();
    a.history_nav.push("first".to_string());
    a.history_nav.push("second".to_string());
    a.set_input("Hello".to_string());
    // first line, not at start -> jump to start (no recall yet)
    a.cursor_up();
    assert_eq!(a.input, "Hello");
    assert_eq!(a.input_cursor, 0);
    // at first cell -> recall previous ("second"), cursor parked at start
    a.cursor_up();
    assert_eq!(a.input, "second");
    assert_eq!(a.input_cursor, 0);
    // recall again -> "first"
    a.cursor_up();
    assert_eq!(a.input, "first");
    assert_eq!(a.input_cursor, 0);
    // at the oldest entry, another up is a no-op
    a.cursor_up();
    assert_eq!(a.input, "first");
}

#[test]
fn cursor_down_recalls_at_last_cell() {
    let mut a = app();
    a.history_nav.push("first".to_string());
    a.history_nav.push("second".to_string());
    a.set_input("Hello".to_string());
    // walk back to "first" (cursor at start)
    a.cursor_up();
    a.cursor_up();
    a.cursor_up();
    a.cursor_up();
    assert_eq!(a.input, "first");
    // last line, not at end -> jump to end
    a.cursor_down();
    assert_eq!(a.input, "first");
    assert_eq!(a.input_cursor, "first".len());
    // at end -> recall next ("second"), cursor at end
    a.cursor_down();
    assert_eq!(a.input, "second");
    assert_eq!(a.input_cursor, "second".len());
    // recall next -> restored stash "Hello"
    a.cursor_down();
    assert_eq!(a.input, "Hello");
    assert_eq!(a.input_cursor, "Hello".len());
}

#[test]
fn cursor_up_down_navigate_multiline() {
    let mut a = app();
    a.set_input("line1\nline2\nline3".to_string());
    // cursor at end (row 2). up -> row 1
    a.cursor_up();
    assert_eq!(a.cursor_row_col().0, 1);
    a.cursor_up();
    assert_eq!(a.cursor_row_col().0, 0);
    // up on first line (col>0) -> start of line
    a.cursor_up();
    assert_eq!(a.cursor_row_col(), (0, 0));
    // down -> row 1
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

    // /resume with no store notifies (warn) and opens no picker.
    assert!(a.slash_command("/resume"));
    assert!(a.picker.is_none());
    let (msg, kind) = a.notify_badge().expect("/resume notified");
    assert_eq!(kind, NotifyKind::Warn);
    assert!(msg.contains("disabled"));

    assert!(a.slash_command("/new"));
    assert!(a.turns.is_empty());
    assert!(a.session.path.is_none());
    // Footer stats reset with the session.
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
    // No session path: the modal still opens, showing "(none)" and a note.
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
    a.session.path = Some(std::path::PathBuf::from("/tmp/sessions/abc123.jsonl"));
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
    // Paste is ignored while the modal owns input.
    handle_event(&Event::Paste("pasted".to_string()), &mut a, None, &mut run);
    assert_eq!(a.input, "");
    // Dismiss, then paste lands in the prompt.
    handle_event(&plain_key(KeyCode::Esc), &mut a, None, &mut run);
    handle_event(&Event::Paste("pasted".to_string()), &mut a, None, &mut run);
    assert_eq!(a.input, "pasted");
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
            },
            TreeEntry {
                branch_point: "y".into(),
                label: "user: yo".into(),
                prefix: "|- ".into(),
                prefill: String::new(),
                is_active: false,
            },
        ],
        selected: 0,
    });
    let backend = TestBackend::new(80, 22);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    let buf = term.backend().buffer();
    // Top border row of the modal carries the 'R' of "Roll back".
    let top_y = (0..22)
        .find(|&y| {
            (0..80)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect::<String>()
                .contains('R')
        })
        .expect("tree modal border found");
    // 2 entries => height 4; bottom-anchored would put the border at y=18,
    // centered at ~9. Insist on centered.
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
    // j scrolls down, k back up.
    handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    assert_eq!(a.info.as_ref().unwrap().scroll, 1);
    for _ in 0..5 {
        handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    }
    assert_eq!(a.info.as_ref().unwrap().scroll, 6);
    handle_event(&plain_key(KeyCode::Char('k')), &mut a, None, &mut run);
    assert_eq!(a.info.as_ref().unwrap().scroll, 5);
    // Scrolling clamps at the bottom.
    for _ in 0..total {
        handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    }
    assert_eq!(a.info.as_ref().unwrap().scroll, total - view_h);
    // q dismisses.
    handle_event(&plain_key(KeyCode::Char('q')), &mut a, None, &mut run);
    assert!(a.info.is_none());
}

#[test]
fn slash_complete_filters_and_accepts() {
    let mut a = app();
    // "/" matches all commands.
    a.input = "/".to_string();
    a.refresh_slash_complete();
    let sc = a.slash_complete.as_ref().expect("popover open");
    assert_eq!(sc.candidates.len(), SLASH_COMMANDS.len());
    // "/tr" filters to just /tree.
    a.input = "/tr".to_string();
    a.refresh_slash_complete();
    let sc = a.slash_complete.as_ref().expect("popover open");
    assert_eq!(sc.candidates, vec![8]); // /tree is index 8
                                        // Typing the full command dismisses (nothing left to complete).
    a.input = "/tree".to_string();
    a.refresh_slash_complete();
    assert!(a.slash_complete.is_none());
    // Non-command input dismisses.
    a.input = "hello".to_string();
    a.refresh_slash_complete();
    assert!(a.slash_complete.is_none());
    // Accept replaces the input with the selected candidate.
    a.input = "/".to_string();
    a.refresh_slash_complete();
    a.slash_complete_down(); // index 1 = /compact
    a.slash_complete_down(); // index 2 = /exit
    a.slash_complete_down(); // index 3 = /help
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
    // Tab cycles forward with wrap-around: after `len` presses we're
    // back at the first candidate (/clear, index 0 in SLASH_COMMANDS).
    for _ in 0..len {
        handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    }
    assert_eq!(a.slash_complete.as_ref().unwrap().selected, 0);
    // Enter accepts the selection (auto-completes), replacing the input.
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
    // Same for Shift+Tab on a single candidate.
    a.input = "/tr".to_string();
    a.refresh_slash_complete();
    handle_event(&plain_key(KeyCode::BackTab), &mut a, None, &mut run);
    assert_eq!(a.input, "/tree");
    assert!(a.slash_complete.is_none());
}

#[test]
fn tree_no_session_pushes_error() {
    let mut a = app();
    // No session.path set — ephemeral. /tree notifies on the rule line
    // and leaves no overlay.
    assert!(a.slash_command("/tree"));
    assert!(a.tree_picker.is_none());
    let (msg, kind) = a.notify_badge().expect("/tree notified");
    assert_eq!(kind, NotifyKind::Error);
    assert!(msg.contains("no session file"));
    assert!(a.branch_hint.is_none());
}

#[test]
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
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut batch, None).unwrap();
    // Read back the ids so the test can assert against them.
    let (_meta, events, _o, _s) = store::load(&path).unwrap();
    let turn_end1_id = events[2].id.clone();

    let mut a = app();
    a.session.path = Some(path);
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    // Tree: user1, agent1 (turn_end1), user2, agent2 (turn_end2).
    assert_eq!(picker.entries.len(), 4);
    assert_eq!(picker.selected, 3); // defaults to the last entry
                                    // Find the "edit turn 2" entry (prefill = "second").
    let edit_idx = picker
        .entries
        .iter()
        .position(|e| e.prefill == "second")
        .unwrap();
    a.tree_picker.as_mut().unwrap().selected = edit_idx;
    a.tree_picker_confirm();
    // Confirm rolls back: the visible turns drop to just turn 1
    // (user "first" + assistant "hello" + turn_end), the input is
    // prefilled with "second", and the branch hint is turn_end1's id.
    assert!(a.tree_picker.is_none());
    assert_eq!(a.input, "second");
    assert_eq!(a.branch_hint.as_deref(), Some(turn_end1_id.as_str()));
    // One visible turn (turn 1); turn 2 is rolled back out of view.
    assert_eq!(a.turns.len(), 1);
    assert_eq!(a.history.lock().unwrap().len(), 2); // user1 + assistant1
}

#[test]
fn tree_shows_compaction_node_and_reverts_before_it() {
    // user1 -> assistant1 -> turn_end1 -> Compaction -> user2 -> assistant2
    // -> turn_end2. /tree must list a "compact:" node between turn 1 and
    // turn 2. Selecting it rolls back to the compaction's parent
    // (turn_end1), restoring the pre-compaction history (user1 + assistant1)
    // and leaving the input empty.
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _o, _s) = store::load(&path).unwrap();
    let turn_end1_id = events[2].id.clone();

    let mut a = app();
    a.session.path = Some(path);
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    // Trunk: user1, agent1, compact, user2, agent2.
    assert!(picker
        .entries
        .iter()
        .any(|e| e.label.starts_with("compact:")));
    let comp_idx = picker
        .entries
        .iter()
        .position(|e| e.label.starts_with("compact:"))
        .unwrap();
    // The compaction node reverts to its parent (turn_end1), prefill empty.
    assert_eq!(picker.entries[comp_idx].branch_point, turn_end1_id);
    assert!(picker.entries[comp_idx].prefill.is_empty());
    a.tree_picker.as_mut().unwrap().selected = comp_idx;
    a.tree_picker_confirm();
    // Rolled back to before the compact: only turn 1's messages remain.
    assert!(a.tree_picker.is_none());
    assert_eq!(a.branch_hint.as_deref(), Some(turn_end1_id.as_str()));
    assert_eq!(a.history.lock().unwrap().len(), 2); // user1 + assistant1
    assert_eq!(a.turns.len(), 1);
}

#[test]
fn tree_hides_checkpoint_copies_and_reverts_to_pre_compaction_leaf() {
    use lofi_core::session::store::{self, SessionStore};
    let dir = tempfile::tempdir().unwrap();
    let session_store = SessionStore::new(dir.path().join("s"));
    let path = session_store
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut original, None).unwrap();
    let pre_compaction_leaf = original[2].id.clone();
    store::append_compaction(
        &path,
        &[user("first"), assistant("hello")],
        None,
        "summary".into(),
        [original[0].id.clone(), original[1].id.clone()],
        2,
        2,
    )
    .unwrap();

    let mut a = app();
    a.session.path = Some(path);
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
    // /tree on a two-turn session yields 4 entries, defaulting to the
    // last (index 3). Tab wraps forward (3 -> 0); Shift+Tab wraps back
    // (0 -> 3); Ctrl+N/Ctrl+P clamp at the edges.
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut batch, None).unwrap();
    let mut a = app();
    a.session.path = Some(path);
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let len = a.tree_picker.as_ref().unwrap().entries.len();
    assert_eq!(len, 4);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 3);
    let mut run = None;
    // Tab wraps forward: last (3) -> first (0).
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 0);
    // Shift+Tab wraps back: first (0) -> last (3).
    handle_event(&plain_key(KeyCode::BackTab), &mut a, None, &mut run);
    assert_eq!(a.tree_picker.as_ref().unwrap().selected, 3);
    // Ctrl+N moves forward (clamped, no wrap): 0 -> 1.
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
    // Reverting to the first user prompt (root, no parent) sets
    // branch_hint to "" — the active path is empty. Reopening /tree
    // must still show every turn as an unhighlighted branch, not
    // "no branch points in this session yet".
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut batch, None).unwrap();

    let mut a = app();
    a.session.path = Some(path.clone());
    a.session.cwd = std::path::PathBuf::from("/x");
    // First /tree: select the root user prompt (entry 0) and revert.
    // Its branch_point is its parent (the system message), so the
    // active path becomes just the system message — the transcript is
    // empty (no visible turns) but branch_hint is the system id.
    assert!(a.slash_command("/tree"));
    a.tree_picker.as_mut().unwrap().selected = 0;
    a.tree_picker_confirm();
    assert!(a.branch_hint.is_some());
    assert_eq!(a.turns.len(), 0); // rolled back to before any user turn
    assert_eq!(a.input, "first");
    // Reopen /tree: all four nodes must appear, none active.
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker reopened");
    assert_eq!(picker.entries.len(), 4);
    assert!(picker.entries.iter().all(|e| !e.is_active));
}

#[test]
fn tree_shows_tool_result_nodes() {
    // user -> assistant(ToolUse) -> tool_result -> assistant(text) -> turn_end
    // /tree must list a `tool:` node between the user prompt and the agent
    // turn-end, showing the tool name and a result preview. Selecting the
    // tool node rolls back to after the tool result (branch_point = its id).
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _o, _s) = store::load(&path).unwrap();
    let tool_result_id = events[2].id.clone();

    let mut a = app();
    a.session.path = Some(path);
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    // Tree: user, tool, agent (turn_end).
    assert_eq!(picker.entries.len(), 3);
    assert!(picker.entries[0].label.starts_with("user:"));
    assert!(
        picker.entries[1].label.starts_with("tool:"),
        "expected tool: node, got {}",
        picker.entries[1].label
    );
    assert!(picker.entries[2].label.starts_with("agent:"));
    // The tool node shows the tool name (bash) and a result preview.
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
    // Tool node is branchable (branch_point = its own id, prefill empty).
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
        .create(std::path::Path::new("/x"), &"m".into())
        .unwrap();
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
    store::append_events(&path, &mut batch, None).unwrap();

    let mut a = app();
    a.session.path = Some(path);
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    // Find the exec entry (starts with "exec:")
    let exec_entry = picker
        .entries
        .iter()
        .find(|e| e.label.starts_with("exec:"))
        .expect("should have an exec: node");
    // The label should list the native tools, not the raw exec result.
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
    // Write a v1 header + raw message lines (no `type` field on messages).
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
    a.session.path = Some(path);
    a.session.cwd = std::path::PathBuf::from("/x");
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    // V1: user, tool (no turn_end, so no agent node).
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
    a.toggle_verbose();
    assert!(a.verbose);
    // Verbose state surfaces on the rule line, not as a chat turn.
    assert_eq!(a.turns.len(), before);
    a.toggle_verbose();
    assert!(!a.verbose);
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

    // Collapsed: only the one-line marker; the folded text is absent.
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

    // Expanded: the marker plus the folded summary text.
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
    // The error text lives on later, indented lines that each fit the column.
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
    // A single long unbreakable result line. Under truncation its tail
    // ("hij") would be clipped at the column edge; under wrapping it must
    // appear on a continuation row.
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
    // No row overflows the column — wrapping keeps every line in bounds.
    for rl in &rls {
        assert!(
            rl.line.width() <= 28,
            "row overflows 28: {}",
            rl.line.width()
        );
    }
    // The tail of the long line is visible (wrapping, not truncation).
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
fn checkpointed_tail_is_hidden_from_ui_but_used_for_model_resume() {
    let events = sev_chain([
        msg(user("old prompt")),
        msg(assistant("old reply")),
        msg(user("kept prompt")),
        msg(assistant("kept reply")),
        // Durable, context-edited copies written by append_compaction.
        msg(user("kept prompt")),
        msg(assistant("kept reply")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: "e4".to_string(),
            summarized_range: ["e0".to_string(), "e1".to_string()],
            checkpointed_tail: true,
            summarized: 2,
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
    // Text, Tool, Text.
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
    // Tool elapsed is restored from the ToolTiming event.
    let Block::Tool(t) = &blocks[0] else {
        unreachable!()
    };
    assert_eq!(t.elapsed, Some(Duration::from_millis(7)));
    // The trailing block is the restored turn-end marker.
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
    use lofi_core::session::store::{active_path_from_leaf, last_event_id};

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
    store::append_events(&path, &mut t1_events, None).unwrap();
    let checkpoint = last_event_id(&path).unwrap().unwrap();

    // Second (failed) turn: messages + TurnFailed marker, all chained
    // linearly off the checkpoint (parent_hint = checkpoint), mirroring
    // the recorder's flush(Failed) which does NOT branch the marker.
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
    store::append_events(&path, &mut t2_events, Some(&checkpoint)).unwrap();

    let (_meta, events, _, _) = store::load(&path).unwrap();
    // The active leaf is the TurnFailed marker; the active path includes
    // the failed turn's messages (they're ancestors of TurnFailed).
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

    // messages_from_events yields only the checkpoint's messages,
    // excluding the failed turn's messages via the TurnFailed boundary.
    let msgs = messages_from_events(&events, &lofi_types::EditConfig::default());
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role, Role::User);
    assert_eq!(msgs[1].role, Role::Assistant);
}

#[test]
fn messages_from_events_prepends_compaction_summary() {
    // After a compaction, the summary must LEAD the history (matching
    // `compacted_history`: summary first, then the kept tail), and a
    // force-continued turn appended after the marker must follow the kept
    // tail — not sandwich the summary mid-stream.
    use lofi_core::session::store;
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"meta\",\"version\":2,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n",
    )
    .unwrap();

    // Summarized prefix (folded), then the kept tail (latest turn), then a
    // Compaction marker whose first_kept_entry_id points at the kept tail's
    // first message, then a force-continued assistant message.
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
    store::append_events(&path, &mut events, None).unwrap();
    // Patch the marker's first_kept_entry_id to the kept-prompt event id.
    let (_meta, mut events, _off, _size) = store::load(&path).unwrap();
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
    // [summary, kept-prompt, kept-reply, continued]
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
    // compact_now writes the EDITED kept-tail messages to the transcript.
    // messages_from_events reads verbatim — no in-memory edit_tail needed.
    // This test simulates the on-disk layout: edited kept-tail messages
    // (already stubbed by compact_now), then the marker, then post-compaction
    // messages with full results.
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let exec_call_stub = |id: &str, eid: &str| {
        // Already edit_tail'd: code replaced with a lofi.result stub.
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "exec".to_string(),
                input: serde_json::json!({"code": format!("[code cleared — re-expand with lofi.result(\"{eid}\")]" )}),
            }],
        }
    };
    let exec_result_stub = |id: &str, eid: &str| {
        // Already edit_tail'd: content replaced with a lofi.result stub.
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: format!("[exec result cleared — re-expand with lofi.result(\"{eid}\")]"),
                is_error: false,
            }],
        }
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

    // On-disk layout after compact_now:
    // [old summarized msg] [edited kept-tail: stubbed t1 + verbatim t2] [marker]
    // [post-compaction: full t3]
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
            kept: 4,
        },
        msg(exec_call_full("t3")),
        msg(exec_result_full("t3", "post-compaction-result")),
    ]);

    let msgs = messages_from_events(&events, &lofi_types::EditConfig::default());
    // [SUMMARY, edited kept-tail(4), post-compaction(2)] = 7
    assert_eq!(msgs.len(), 7);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");

    // Kept tail: read verbatim from disk (already edited by compact_now).
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

    // Post-compaction: verbatim, full results.
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
    // A component starting with `~` keeps the tilde: `~sirn` -> `~s`.
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
    // Cost lives in the footer now, not the header.
    assert!(
        !header.contains('$'),
        "header should not show cost: {header}"
    );
}

#[test]
fn footer_shows_session_cache_metrics() {
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
    assert!(footer.contains("cache ↑800k ↓200k"), "footer: {footer}");
}

#[test]
fn queue_badge_shows_preview_and_count() {
    let mut a = app();
    // Empty queue: no badge.
    assert!(a.queue_badge().is_none());
    // One item: "Queue: <preview>".
    a.prompt_queue.push("fix the bug".to_string());
    let badge = a.queue_badge().expect("badge for one item");
    assert!(badge.contains("Queue: fix the bug"), "badge: {badge}");
    // Two items: "Queue: <preview> (+1)".
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
    // Alt+Up restores the last queued (LIFO).
    let ev = Event::Key(crossterm::event::KeyEvent::new_with_kind(
        KeyCode::Up,
        KeyModifiers::ALT,
        KeyEventKind::Press,
    ));
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "second prompt");
    assert_eq!(a.prompt_queue.len(), 1);
    // Again: restores the first.
    handle_event(&ev, &mut a, None, &mut run);
    assert_eq!(a.input, "first prompt");
    assert!(a.prompt_queue.is_empty());
}

/// With no model configured, submitting a prompt must not start a run;
/// it re-surfaces the configuration hint on the prompt's turn instead.
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
    // Simulated rendered log lines paired with their content ranges
    // (as components now report them): the leading gutter/rails and the
    // trailing padding tail fall outside the content range, while the
    // content's own leading spaces (indentation) are inside it.
    a.log_off = 0;
    a.log_vis = vec![
        // gutter "  " + content "hello world" + padding "   "
        view::VisLine {
            rendered: "  hello world   ".to_string(),
            content: (2, 13),
            raw: None,
        },
        // gutter "  " + rails "│ │ " + content "lofi-core…Agent {" + padding
        view::VisLine {
            rendered: "  │ │ lofi-core/src/agent.rs:233:pub struct Agent {     ".to_string(),
            content: (6, 51),
            raw: None,
        },
        // gutter "  " + content "    let x = 1;" (indentation preserved!)
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
    // A rendered line strips markdown markers (e.g. **bold** → bold), but the
    // raw source is retained via a position map so yank copies the original
    // markdown. The map `[0,3,4,5,8]` for source `**bold**` (display "bold")
    // snaps the whole-row yank to `source[0..8]` = `**bold**`.
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
    // Decoration-only lines carry no raw source; yank returns the rendered
    // content slice as before.
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
    // Selecting just the visible "bold" within `**bold**` still yields the
    // raw `**bold**` — the map snaps the selection to the enclosing markers.
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
    // Select display content [2, 6) = "bold".
    a.sel = Some(Selection {
        start: (0, 2),
        end: (0, 6),
    });
    assert_eq!(a.selection_text().as_deref(), Some("**bold**"));
}

#[test]
fn selection_text_raw_skips_softwrap_newlines() {
    // `hello world` soft-wrapped across two rows; `second line` on a third.
    // Selecting all three rows yields the two source lines joined by a single
    // `\n` — the soft-wrap boundary contributes no separator.
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
    // Selecting only the continuation row "world" yields just `world` (char
    // level), not the whole source line — the map slices the row's fragment.
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
    // Viewport moved up to [12, 16]; cursor fell below -> clamp to 16.
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
    // Viewport moved down to [13, 17]; cursor fell above -> clamp to 13.
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
    // A non-empty draft clears the quit window, dropping the badge.
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
    // Empty prompt -> EOF -> quit.
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
    // Esc on a non-empty prompt just clears it (no mode switch).
    let mut b = app();
    b.set_input("draft".to_string());
    handle_event(&plain_key(KeyCode::Esc), &mut b, None, &mut run);
    assert_eq!(b.mode, Mode::Input);
    assert_eq!(b.input, "");
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
    // Clamp at the top.
    a.nav_move(-100);
    assert_eq!(a.nav_cursor, 0);
    // Clamp at the bottom.
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
    // Cursor left of anchor: same span, min becomes start.
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
    // Empty selection at the cursor (anchor == cursor); +1 makes the end
    // exclusive, so it spans one char once clamped to the content range.
    assert_eq!(a.sel.as_ref().unwrap().start, (3, 0));
    assert_eq!(a.sel.as_ref().unwrap().end, (3, 1));
    // Move down: selection extends to the next line, column preserved.
    handle_event(&plain_key(KeyCode::Char('j')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 4);
    assert_eq!(a.sel.as_ref().unwrap().start, (3, 0));
    assert_eq!(a.sel.as_ref().unwrap().end, (4, 1));
    // Tab drops selection, back to Navigate.
    handle_event(&plain_key(KeyCode::Tab), &mut a, None, &mut run);
    assert_eq!(a.mode, Mode::Navigate);
    assert!(a.sel.is_none());
}

#[test]
fn esc_discards_select_back_to_nav() {
    // From Select, Esc returns to Navigate and clears the selection —
    // same as Tab, but more conventional for drop-without-yank.
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
    // turn0 (3) + blank (1) = 4.
    assert_eq!(a.turn_start_line(1), 4);
    // + turn1 (2) + blank (1) = 7.
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
    // At the last turn, `]` stays at its start.
    handle_event(&plain_key(KeyCode::Char(']')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 7);
    handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 4);
    handle_event(&plain_key(KeyCode::Char('[')), &mut a, None, &mut run);
    assert_eq!(a.nav_cursor, 0);
    // At the first turn, `[` stays at 0.
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
    // The current model is the second entry.
    assert_eq!(picker.selected, 1);
    assert!(a.modal_open());
}

#[test]
fn model_picker_open_empty_notifies() {
    let mut a = app();
    a.model_choices = Vec::new();
    a.open_model_picker();
    assert!(a.model_picker.is_none());
    // A warn notification is surfaced (not a crash).
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
    // Move up to the first entry (anthropic/claude).
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
    // off first, then the model's declared levels.
    assert_eq!(
        picker.levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Medium,
            ThinkingLevel::High
        ]
    );
    // Medium is the current level.
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
    // Move to High (index 2).
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
    // No models available -> never switch.
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
    // No turn marker -> nothing to restore even with choices.
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

/// Regression for "Transcript with background color should resize when
/// viewport changed": the frozen-render cache must be invalidated on a width
/// change, not only on a content (epoch) change. Otherwise completed turns
/// keep their old-width rendering — background padding stays narrow after a
/// terminal resize. A long prompt wraps to fewer lines at a wider viewport, so
/// a strictly smaller height after widening proves the cache was rebuilt.
#[test]
fn frozen_cache_invalidates_on_width_change() {
    let mut a = app();
    // turn[0]: a long prompt that wraps to many lines when narrow.
    a.turns.push(Turn {
        prompt: "word ".repeat(30),
        blocks: Vec::new(),
    });
    // turn[1] keeps turn[0] frozen (the last turn is rebuilt each frame).
    push_turn(&mut a);

    a.ensure_frozen(20);
    let h_narrow = a.frozen_heights[0];
    assert!(a.frozen_render.get(0).is_some());

    a.ensure_frozen(100);
    let h_wide = a.frozen_heights[0];
    assert!(a.frozen_render.get(0).is_some());
    // Without width invalidation the cache would keep its narrow rendering
    // and h_wide would equal h_narrow.
    assert!(
        h_wide < h_narrow,
        "frozen cache should re-wrap at the new width: narrow={h_narrow} wide={h_wide}"
    );
}

/// Regression for "resize snaps a scrolled-up view to the bottom": a re-wrap
/// shrinks `total`/`base`, and the carried-over absolute `top_line` can land
/// past the new bottom, clamping to the bottom and sticky-pinning. The view
/// must instead keep its previous relative scroll position.
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
    // turn[1] keeps turn[0] frozen (the last turn is rebuilt each frame).
    push_turn(&mut a);

    // Render narrow, then scroll up to the middle of the transcript.
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

    // Resize wider. Without re-anchoring the carried-over `top_line` would
    // exceed the new (smaller) `base`, clamp to the bottom, and stick
    // (`pinned = true`). With re-anchoring the view keeps its relative
    // position and stays scrolled up.
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

/// The Navigate cursor is an absolute line index, so a re-wrap mustn't drift
/// it to a different proportional spot — it should stay on the same *screen
/// row* (the viewport re-anchor keeps the viewport at ~its previous content, so
/// the same row is ~the same line).
#[test]
fn resize_keeps_nav_cursor_on_same_content_line() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let mut a = app();
    // A prompt long enough to wrap to several lines at both widths.
    let prompt =
        "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima mike november oscar papa quebec romeo sierra tango uniform victor whiskey xray yankee zulu "
            .repeat(2);
    a.turns.push(Turn {
        prompt,
        blocks: Vec::new(),
    });
    push_turn(&mut a); // turn[1] keeps turn[0] frozen.
    a.mode = Mode::Navigate;

    // Narrow render; place the cursor on a line well inside turn 0.
    let mut term = Terminal::new(TestBackend::new(28, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    // Place the cursor on a line well inside turn 0 and capture the cumulative
    // content-char offset of its start — the stable anchor across a re-wrap.
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

    // Narrow render; park the cursor cell mid-content on a line inside turn 0.
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

    // Widen: the line re-wraps, but the cursor cell must stay on the same
    // content character (its `nav_col` re-seated onto that char).
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

    // Narrow render; set up a selection spanning two lines inside turn 0.
    let mut term = Terminal::new(TestBackend::new(28, 24)).unwrap();
    term.draw(|f| crate::tui::view::render(f, &mut a)).unwrap();
    // Anchor on line 2, cursor on line 5 — both within turn 0.
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

    // Widen: both selection endpoints must stay on their content chars.
    // Before the fix, the anchor kept its stale absolute line index and
    // drifted onto the wrong content character.
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

    // Narrow render; place the cursor a few rows into the viewport.
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

/// A height shrink that leaves the cursor's old row past the new viewport must
/// clamp it to the bottom edge instead of letting it disappear off-screen.
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

    // Tall viewport; cursor near the bottom of it.
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
    // Long flow text that wraps with break spaces (exercises `wrap`).
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
    // The cursor's exec header.
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

    // Render at 64; find the second Exec header and place the cursor on it.
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

/// A fenced code block renders as plain triple-backtick fences (not the old
/// `╭─`/`╰──` frame art) on a full-width surface tile with a 2-space right
/// gutter, and the language label follows the opening backticks.
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
    // Locate the opening fence, code line, and closing fence.
    let open = lines
        .iter()
        .find(|l| l.starts_with("  ```rust"))
        .expect("opening ```rust");
    let code = lines
        .iter()
        .find(|l| l.contains("let x = 1;"))
        .expect("code line");
    let close = lines
        .iter()
        .find(|l| l.trim() == "```")
        .expect("closing ```");
    // No frame art survives.
    for l in &lines {
        assert!(!l.contains('╭'), "stray frame art: {l}");
        assert!(!l.contains('╰'), "stray frame art: {l}");
        assert!(!l.contains('│'), "stray rail: {l}");
    }
    // Every fence/code line fills the full width (surface bg spans edge-to-edge
    // via rtile, with the 2-space right gutter as trailing bg padding).
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
    let content: String = chars[open_rl.content.0..open_rl.content.1].to_string();
    assert_eq!(content, "```rust");
}
