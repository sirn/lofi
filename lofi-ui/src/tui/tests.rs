#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::float_cmp)]

use super::*;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_providers::{Provider, ToolSchema};
use lofi_types::{ContentBlock, Message, Model, Role, StreamingEvent, Usage};
use ratatui::backend::TestBackend;

fn app() -> App {
    App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::Medium,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    )
}

fn push_turn(app: &mut App) {
    app.push_turn(Turn {
        kind: lofi_types::PromptKind::User,
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

/// Install a core-owned session sink (plus its read mirror) on a test `App`,
/// pointing at the test's temporary store. Mirror of what production code
/// does via `SessionConfig`/`run`. Branch switching goes through the sink in
/// production, so tests that exercise `/tree` must seed one here.
fn attach_session_sink(
    a: &mut App,
    store: store::SessionStore,
    cwd: &Path,
    cursor: store::SessionCursor,
) {
    a.session.sink = Some(lofi_core::session::sink::SessionSink::with_store(
        store,
        cwd,
        Some(cursor.clone()),
    ));
    a.session.cursor = Some(cursor);
    a.session.cwd = cwd.to_path_buf();
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
        kind: PromptKind::default(),
    }
}

fn assistant(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
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

fn user_text(m: &lofi_types::Message) -> &str {
    match &m.blocks[..] {
        [lofi_types::ContentBlock::Text { text }] => text,
        _ => "",
    }
}

fn ctrl_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::CONTROL,
        KeyEventKind::Press,
    ))
}

fn plain_key(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new_with_kind(
        code,
        KeyModifiers::empty(),
        KeyEventKind::Press,
    ))
}

// into "previous summary". Returns the leaf id of the pre-compaction range.
fn seed_compacted_turn(path: &std::path::Path) {
    let mut old: Vec<SessionEvent> = (0..5)
        .map(|i| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: msg(assistant(&format!("old {i}"))),
        })
        .collect();
    test_append_events(path, &mut old, None).unwrap();
    test_append_compaction(
        path,
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
}

// Append the silent continuation that triggered the regression: two large
// assistant messages followed by a TurnEnd reporting 113k input tokens.
fn seed_post_compact_continuation(path: &std::path::Path) {
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
                stop_reason: None,
            },
        },
    ];
    test_append_events(path, &mut continuation, None).unwrap();
}

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

mod reliability;
mod rendering;
mod session_lifecycle;
mod input_and_modals;
mod session_tree;
mod status_and_replay;
mod navigation;
mod pickers;
mod resize;
mod compaction_resume;
mod streaming;
mod jobs_and_theme;
