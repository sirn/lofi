#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
use super::analyze::analyze;
use super::height::markdown_body_height;
use super::model::MdBlock;
use super::render::render_markdown_body;
use super::table::Align;
use crate::tui::theme::Theme;
use ratatui::style::Style;

fn analyze_blocks(src: &str) -> Vec<MdBlock> {
    analyze(src, Theme::default(), Style::default())
}
#[test]
fn analyze_empty_yields_no_blocks() {
    assert!(analyze_blocks("").is_empty());
}

#[test]
fn raw_map_stays_monotonic_across_formatting_runs() {
    // Bold text containing an inline code span: the span after the code
    // reports the `**` marker as its boundary, which precedes the code
    // span's content. The map must never move backwards or selection
    // slicing panics with begin > end.
    let lines = render_markdown_body(
        "2. **That `TruncAPr2` fixes the test.** If assumption 1 is wrong, \
         `TruncAPr2` may not fix it.",
        Theme::default(),
        80,
        78,
        Style::default(),
        |_| vec![],
    );
    for line in &lines {
        if let Some(raw) = &line.raw {
            assert!(
                raw.map.windows(2).all(|w| w[0] <= w[1]),
                "raw map must be monotonic: {:?}",
                raw.map
            );
        }
    }
}

#[test]
fn list_lines_keep_their_markers() {
    let rl = render_markdown_body(
        "- Hello\n- World\n1. one\n2. two",
        Theme::default(),
        80,
        78,
        Style::default(),
        |_| vec![],
    );
    let text: Vec<String> = rl
        .iter()
        .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    assert_eq!(text, ["- Hello", "- World", "1. one", "2. two"]);
}

#[test]
fn analyze_paragraph_records_source_range() {
    let blocks = analyze_blocks("hello world");
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        MdBlock::Paragraph { spans, src } => {
            assert_eq!(*src, 0..11);
            let text: String = spans.iter().map(|m| m.span.content.as_ref()).collect();
            assert_eq!(text, "hello world");
        }
        other => panic!(
            "expected paragraph, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

fn rendered_text(src: &str) -> Vec<String> {
    render_markdown_body(src, Theme::default(), 80, 78, Style::default(), |_| vec![])
        .iter()
        .map(|l| l.line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn html_block_tag_renders_as_text() {
    assert_eq!(rendered_text("<input>"), ["<input>"]);
}

#[test]
fn inline_html_tag_renders_as_text() {
    assert_eq!(
        rendered_text("before <input> after"),
        ["before <input> after"]
    );
}

fn para_spans(src: &str) -> Vec<(String, Style)> {
    let blocks = analyze_blocks(src);
    match &blocks[0] {
        MdBlock::Paragraph { spans, .. } => spans
            .iter()
            .map(|m| (m.span.content.to_string(), m.span.style))
            .collect(),
        other => panic!(
            "expected paragraph, got {:?}",
            std::mem::discriminant(other)
        ),
    }
}

use ratatui::style::Modifier;

#[test]
fn single_tilde_renders_verbatim() {
    let src = "So your ~$250 ballpark is right — call it **~$240–250/mo**.";
    let spans = para_spans(src);
    let text: String = spans.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(text, src, "single tildes must survive verbatim");
    assert!(
        spans
            .iter()
            .all(|(_, style)| !style.add_modifier.contains(Modifier::CROSSED_OUT)),
        "a wide single-tilde pair must not strike the text between"
    );
}

#[test]
fn double_tilde_still_strikes_through() {
    let spans = para_spans("a ~~gone~~ word");
    let struck: Vec<&str> = spans
        .iter()
        .filter(|(_, style)| style.add_modifier.contains(Modifier::CROSSED_OUT))
        .map(|(s, _)| s.as_str())
        .collect();
    assert_eq!(struck, ["gone"]);
}

#[test]
fn single_tilde_inside_double_tilde_stays_struck() {
    let spans = para_spans("a ~~b ~c~ d~~ e");
    let struck: String = spans
        .iter()
        .filter(|(_, style)| style.add_modifier.contains(Modifier::CROSSED_OUT))
        .map(|(s, _)| s.as_str())
        .collect();
    assert_eq!(struck, "b ~c~ d");
}

#[test]
fn analyze_heading_captures_level_and_content_offset() {
    let src = "## Title here";
    let blocks = analyze_blocks(src);
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        MdBlock::Heading {
            level,
            src: range,
            content_start,
        } => {
            assert_eq!(*level, 2);
            assert_eq!(*range, 0..13);
            assert_eq!(*content_start, 3); // after "## "
                                           // Content (after the `# ` prefix) slices back to the source.
            assert_eq!(&src[*content_start..range.end], "Title here");
        }
        other => panic!("expected heading, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_fenced_code_splits_body_lines_with_ranges() {
    let src = "```rust\nfn main() {}\n  more\n```";
    let blocks = analyze_blocks(src);
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        MdBlock::Code { lang, lines, fence } => {
            assert_eq!(lang.as_deref(), Some("rust"));
            assert_eq!(lines.len(), 2);
            assert_eq!(lines[0].0, "fn main() {}");
            assert_eq!(lines[1].0, "  more");
            // Body ranges slice back to the exact source text.
            for (line, r) in lines {
                assert_eq!(&src[r.clone()], line);
            }
            assert_eq!(&src[fence.clone()], src);
        }
        other => panic!("expected code, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_table_captures_header_rows_and_aligns() {
    let src = "| a | b |\n|---|--:|\n| 1 | 2 |";
    let blocks = analyze_blocks(src);
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        MdBlock::Table {
            header,
            aligns,
            rows,
            src: range,
        } => {
            assert_eq!(*range, 0..src.len());
            assert_eq!(header.len(), 2);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].len(), 2);
            assert!(matches!(aligns[0], Align::Left));
            assert!(matches!(aligns[1], Align::Right));
            // Cell source ranges slice back to the raw cell text.
            assert_eq!(&src[header[0].src.clone()], "a");
            assert_eq!(&src[rows[0][1].src.clone()], "2");
        }
        other => panic!("expected table, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_blockquote_splits_lines_and_prefixes() {
    let src = "> one\n> ``two``\n>\n";
    let blocks = analyze_blocks(src);
    assert_eq!(blocks.len(), 1);
    match &blocks[0] {
        MdBlock::Quote { lines, src: range } => {
            assert_eq!(lines.len(), 3);
            assert_eq!(lines[0].prefix_len, 2);
            assert_eq!(lines[2].prefix_len, 1); // bare ">"
            let l0: String = lines[0]
                .spans
                .iter()
                .map(|m| m.span.content.as_ref())
                .collect();
            assert_eq!(l0, "one");
            // Each line's source range slices back to the raw quote line.
            assert_eq!(&src[lines[0].src.clone()], "> one");
            assert_eq!(&src[lines[2].src.clone()], ">");
            let _ = range;
        }
        other => panic!("expected quote, got {:?}", std::mem::discriminant(other)),
    }
}

#[test]
fn analyze_sequences_multiple_block_kinds() {
    let src = "# H\n\npara\n\n> q\n\n```\nx\n```";
    let blocks = analyze_blocks(src);
    let kinds: Vec<&str> = blocks
        .iter()
        .map(|b| match b {
            MdBlock::Blank { .. } => "blank",
            MdBlock::Paragraph { .. } => "para",
            MdBlock::Heading { .. } => "heading",
            MdBlock::Code { .. } => "code",
            MdBlock::Table { .. } => "table",
            MdBlock::Quote { .. } => "quote",
        })
        .collect();
    // Blank source lines between blocks are preserved as explicit blocks.
    assert_eq!(
        kinds,
        ["heading", "blank", "para", "blank", "quote", "blank", "code"]
    );
}

#[test]
fn analyze_inline_offsets_are_global_source_bytes() {
    // Bold inside a paragraph after another block: span offsets must point
    // into the full source, not the block-local sub-slice.
    let src = "intro\n\nbody **bold** tail";
    let blocks = analyze_blocks(src);
    // Find the bold span across all paragraphs; its content_start must
    // locate "bold" in src.
    let bold = blocks
        .iter()
        .filter_map(|b| match b {
            MdBlock::Paragraph { spans, .. } => Some(spans),
            _ => None,
        })
        .flatten()
        .find(|m| m.span.content.as_ref() == "bold")
        .unwrap_or_else(|| panic!("bold span"));
    assert_eq!(&src[bold.content_start..bold.content_start + 4], "bold");
}

#[test]
fn heading_content_start_never_precedes_block() {
    // Bare ATX marker followed by more content: heading has no inline
    // events, so `content_start` must clamp to the block range instead of
    // the `InlineExtent` sentinel (0), which precedes `src.start`.
    for c in ["foo\n# ", "foo\n#", "#\n", "## ", "#   "] {
        for b in analyze_blocks(c) {
            if let MdBlock::Heading {
                content_start, src, ..
            } = b
            {
                assert!(
                    content_start >= src.start,
                    "content_start {content_start} < src.start {} for {c:?}",
                    src.start
                );
            }
        }
    }
}

#[test]
fn height_matches_render_for_bare_heading() {
    assert_height_matches("foo\n# ");
    assert_height_matches("\n\n#\n");
}

/// Reference height via the existing renderer: count the lines produced.
fn reference_height(text: &str, content_w: usize) -> usize {
    render_markdown_body(
        text,
        Theme::default(),
        content_w + 2,
        content_w,
        Style::default(),
        |_| vec![],
    )
    .len()
}

fn assert_height_matches(text: &str) {
    for content_w in [10usize, 20, 30, 40, 80, 120] {
        let expected = reference_height(text, content_w);
        let actual = markdown_body_height(text, content_w);
        assert_eq!(
            actual, expected,
            "height mismatch at content_w={content_w} for:\n{text}",
        );
    }
}

#[test]
fn height_simple_paragraph() {
    assert_height_matches("hello world");
    assert_height_matches("The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog. The quick brown fox jumps over the lazy dog.");
    assert_height_matches("short");
    assert_height_matches("\n".repeat(3).as_str());
}

#[test]
fn height_markdown_structures() {
    assert_height_matches("# Heading one\n\nSome text under it.");
    assert_height_matches("## Sub");
    assert_height_matches("> quote\n> more quote");
    assert_height_matches(
        "> \
> not-empty",
    );
    assert_height_matches("- item one\n- item two");
    assert_height_matches("1. numbered\n2. list");
    assert_height_matches("regular\n\n```rust\nfn main() {}\n```\n\nafter");
    assert_height_matches("```\nplain\n```");
    assert_height_matches("| a | b |\n|---|---|\n| 1 | 2 |");
    assert_height_matches(
        "| very long cell content that will definitely wrap | another |\n|---|---|\n| x | y |",
    );
    assert_height_matches("**bold** and *italic* and `code` mixed inline");
    assert_height_matches("a\n\n[link](https://example.com) trail");
}

#[test]
fn height_wrap_boundaries() {
    // Words exactly at the wrap boundary.
    assert_height_matches("aaaaaaaaaa bbbbbbbbbb cccccccccc dddddddddd eeeeeeeeee");
    // Long unbroken word forces character-level break.
    assert_height_matches("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    // Mixed code and headings and wrapping.
    assert_height_matches("# A heading that is sufficiently long to wrap across multiple lines at narrow widths for sure");
    // Unicode.
    assert_height_matches("こんにちは、世界。これはテストです。もっと書きます。");
    assert_height_matches("emoji 🎉 🚀 and text together");
}

#[test]
fn height_empty() {
    assert_height_matches("");
}

/// Seeded fuzz over many random markdown shapes: every line picked from a
/// pool of construct patterns (headings, lists, quotes, code fences,
/// tables, wrapping text, unicode, links) then joined. `markdown_body_height`
/// must agree with `render_markdown_body(...).len()` for all of them, for
/// each probe width. Both walk the same `analyze()` tree; this guards that
/// the render and height paths never drift apart (the historical source of
/// transcript popping on scroll/resize).
#[test]
fn height_fuzz_seeded() {
    // Simple deterministic PRNG (xorshift64) — no external deps.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n.max(1) as u64) as usize
        }
    }

    let lines_pool = [
        "plain short text",
        "The quick brown fox jumps over the lazy dog. And then some more to wrap.",
        "# h1",
        "## h2 with a rather long heading to force wrapping at narrow widths",
        "### h3",
        "#### h4",
        "##### h5",
        "###### h6",
        "- bullet item",
        "- bullet item with wrapping text to check wrapping behavior at small widths",
        "1. numbered one",
        "2. numbered two",
        "> quote single line",
        "> quote with enough content to wrap across multiple visual lines",
        "> ",
        "```rust",
        "let x = 42; // code",
        "```",
        "| col1 | col2 | col3 |",
        "|------|------|------|",
        "| a    | b    | c    |",
        "| long-cell-content-that-wraps | another | c |",
        "**bold** inline and *italic* too",
        "a `code span` in line",
        "[link text](https://example.com/some/path) and trailing",
        "unicode: こんにちは世界 🎉🚀 テスト",
        "mixed **bold** with `code` and [link](https://example.com)",
        "",    // blank line
        "   ", // whitespace-only
        ">> nested quote marker doesn\u{2019}t exist as a concept but is fine as text",
        "text with *unclosed star and **unclosed bold",
        "#   ", // bare ATX marker (empty heading)
        "# ",
        "## ",
        concat!("#", "\n"),
        "",
    ];

    for seed in 0..64u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let n_lines = 1 + rng.below(24);
        let mut text_parts: Vec<&str> = Vec::new();
        for _ in 0..n_lines {
            text_parts.push(lines_pool[rng.below(lines_pool.len())]);
        }
        let text = text_parts.join("\n");

        for content_w in [5usize, 8, 12, 16, 24, 40, 64, 96, 160] {
            let expected = reference_height(&text, content_w);
            let actual = markdown_body_height(&text, content_w);
            assert_eq!(
                actual, expected,
                "height mismatch (seed={seed} content_w={content_w}) for:\n{text}",
            );
        }
    }
}
