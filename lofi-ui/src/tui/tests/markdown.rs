use super::*;

fn render_text_spans(markdown: &str) -> Vec<ratatui::text::Span<'static>> {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::Text(markdown.to_string()));
    let turn = &a.transcript.turns[0];
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
    let line = render_turn_lines(&cx, &a.transcript.turns[0])
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
    let linked: Vec<_> = render_turn_lines(&cx, &a.transcript.turns[0])
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
    let line = render_turn_lines(&cx, &a.transcript.turns[0])
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
    let turn = &a.transcript.turns[0];
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
    let turn = &a.transcript.turns[0];
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
        let turn = &a.transcript.turns[0];
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
    let turn = &a.transcript.turns[0];
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
