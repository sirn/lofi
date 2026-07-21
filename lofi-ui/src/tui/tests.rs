#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::wildcard_imports)]
// Cost/format tests assert exact computed float values.
#![allow(clippy::float_cmp)]

use super::*;
use lofi_types::{ContentBlock, Role, Usage};

fn app() -> App {
    App::new("openai/gpt-4o".to_string(), ThinkingLevel::Medium, 0, lofi_types::CompactionConfig::default())
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

/// A numbered `read` line whose body is empty must still carry its line
/// number as decoration with an empty content range, so the Navigate
/// cursor overlay preserves it instead of treating it as a blank line.
#[test]
fn numbered_empty_body_line_keeps_its_number() {
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
        result: "line one\n\nline three".to_string(),
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
    let s: String = empty.line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(s.contains(" 2 ") || s.contains(" 2"), "number preserved: {s:?}");
}

#[test]
fn current_line_text_excludes_decoration() {
    let mut a = app();
    push_turn(&mut a);
    a.log_off = 0;
    a.log_lines = vec!["  hello world   ".to_string()];
    a.log_content = vec![(2, 13)]; // "hello world"
    a.nav_cursor = 0;
    assert_eq!(a.current_line_text().as_deref(), Some("hello world"));
}

#[test]
fn vim_motions_move_within_content() {
    let mut a = app();
    push_turn(&mut a);
    a.log_off = 0;
    a.log_lines = vec!["  aa bb cc".to_string()];
    a.log_content = vec![(2, 10)]; // "aa bb cc"
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
    a.log_lines = vec!["  a.b c".to_string()];
    a.log_content = vec![(2, 7)]; // "a.b c"
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
        label: "m".into(),
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
        label: "m".into(),
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
        label: "m".into(),
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
fn session_info_shows_notification() {
    let mut a = app();
    // No session path: a warning notification (not a modal or transcript turn).
    assert!(a.slash_command("/session"));
    assert!(a.turns.is_empty());
    assert!(a.info.is_none());
    let (msg, kind) = a
        .notify_badge()
        .expect("notification shown");
    assert!(msg.contains("no session file"));
    assert_eq!(kind, NotifyKind::Warn);
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
    assert!(top_y < 15, "tree modal should be centered, got top_y={top_y}");
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
    let path = store.create(std::path::Path::new("/x"), "m").unwrap();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "first".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "hello".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "second".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "world".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
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
    let path = store.create(std::path::Path::new("/x"), "m").unwrap();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "first".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "hello".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Compaction {
            summary: "summary".into(),
            first_kept_entry_id: String::new(),
            summarized_range: [String::new(), String::new()],
            summarized: 3,
            kept: 1,
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "second".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "world".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
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
    assert!(picker.entries.iter().any(|e| e.label.starts_with("compact:")));
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
fn modal_tab_cycles_with_wraparound() {
    // /tree on a two-turn session yields 4 entries, defaulting to the
    // last (index 3). Tab wraps forward (3 -> 0); Shift+Tab wraps back
    // (0 -> 3); Ctrl+N/Ctrl+P clamp at the edges.
    use lofi_core::session::store::SessionStore;
    use lofi_types::{ContentBlock, Role};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store.create(std::path::Path::new("/x"), "m").unwrap();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "first".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "hello".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "second".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "world".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
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
    let path = store.create(std::path::Path::new("/x"), "m").unwrap();
    let kinds = [
        SessionEventKind::Message(Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text { text: "sys".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "first".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "hello".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "second".into() }],
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text { text: "world".into() }],
        }),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|k| SessionEvent { id: String::new(), parent_id: None, kind: k })
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
    assert!(collapsed_s.contains("compacted 7 msgs"), "collapsed: {collapsed_s}");
    assert!(!collapsed_s.contains("Build a coding agent"), "collapsed leaked summary: {collapsed_s}");

    // Expanded: the marker plus the folded summary text.
    a.toggle_verbose();
    let expanded = render_turns(&a, 80);
    let expanded_s = join_rendered(&expanded);
    assert!(expanded_s.contains("compacted 7 msgs"));
    assert!(expanded_s.contains("Build a coding agent"), "expanded missing summary: {expanded_s}");
    assert!(expanded_s.contains("Use Rust."), "expanded missing summary: {expanded_s}");
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
    let cx = Cx { app: &a, theme: a.theme, width: 40, active_turn: false };
    let rls = render_turn_lines(&cx, turn);
    // Line 0 is the status header: model, level, duration only — the error
    // must not appear inline there (it used to, and got clipped).
    let header: String = rls[0].line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(header.contains("failed in"), "header missing status: {header}");
    assert!(header.contains("openai/gpt-4o"), "header missing label: {header}");
    assert!(!header.contains("503"), "header must not carry the error inline: {header}");
    // The error text lives on later, indented lines that each fit the column.
    let body: String = rls[1..]
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(body.contains("503"), "wrapped error missing from body: {body}");
    for rl in &rls[1..] {
        assert!(rl.line.width() <= 40, "body line overflows: {}", rl.line.width());
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
    let cx = Cx { app: &a, theme: a.theme, width: 80, active_turn: false };
    let rls = render_turn_lines(&cx, turn);
    // The provider emits the error twice on a stream failure — as a fatal
    // `✗` line and again in the TurnFailed marker. The dedup must collapse
    // them so the message appears exactly once across the rendered turn.
    let all: String = rls
        .iter()
        .flat_map(|rl| rl.line.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert_eq!(all.matches("stream interrupted").count(), 1, "dedup failed: {all}");
}

#[test]
fn exec_result_wraps_long_lines_instead_of_truncating() {
    use crate::tui::view::blocks::render_turn_lines;
    use crate::tui::view::component::Cx;
    let mut a = app();
    push_turn(&mut a);
    a.apply_event(AgentEvent::ToolStart { id: "e1".to_string(), name: "exec".to_string() });
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
        result: token.to_string(),
        is_error: false,
    });
    a.apply_event(AgentEvent::ToolEnd {
        id: "e1".to_string(),
        result: "{\"value\":null}".to_string(),
        is_error: false,
        elapsed_ms: 0,
    });
    let turn = &a.turns[0];
    let cx = Cx { app: &a, theme: a.theme, width: 28, active_turn: false };
    let rls = render_turn_lines(&cx, turn);
    // No row overflows the column — wrapping keeps every line in bounds.
    for rl in &rls {
        assert!(rl.line.width() <= 28, "row overflows 28: {}", rl.line.width());
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
    assert!(all.contains("hij"), "result tail clipped (no wrapping): {all}");
}

#[test]
fn footer_shows_model_and_thinking() {
    let a = App::new("openai/gpt-5.6-sol".to_string(), ThinkingLevel::XHigh, 0, lofi_types::CompactionConfig::default());
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("gpt-5.6-sol"));
    assert!(r.contains("· xhigh"));
}

#[test]
fn footer_hides_thinking_when_off() {
    let a = App::new("openai/gpt-4o".to_string(), ThinkingLevel::Off, 0, lofi_types::CompactionConfig::default());
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
        label: "m".into(),
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
fn turns_from_events_links_tool_results() {
    let messages = vec![
        user("run it"),
        Message {
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text { text: "ok".to_string() },
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
            blocks: vec![ContentBlock::Text { text: "done".to_string() }],
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
            blocks: vec![ContentBlock::Text { text: "done".to_string() }],
        }),
        SessionEventKind::ToolTiming { tool_call_id: "t1".into(), elapsed_ms: 7 },
        SessionEventKind::TurnEnd {
            label: "proxy/gemini-3-flash · medium".into(),
            elapsed_ms: 2000,
            cost: 0.0,
            usage: Usage::default(),
        },
    ]);
    let turns = turns_from_session_events(&events);
    assert_eq!(turns.len(), 1);
    let blocks = &turns[0].blocks;
    // Tool elapsed is restored from the ToolTiming event.
    let Block::Tool(t) = &blocks[0] else { unreachable!() };
    assert_eq!(t.elapsed, Some(Duration::from_millis(7)));
    // The trailing block is the restored turn-end marker.
    match blocks.last() {
        Some(Block::TurnEnd { label, elapsed }) => {
            assert_eq!(label, "proxy/gemini-3-flash · medium");
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
    std::fs::write(&path, "{\"type\":\"meta\",\"version\":2,\"created\":1,\"cwd\":\"/x\",\"model\":\"m\"}\n").unwrap();

    // First (successful) turn: two messages + a TurnEnd, chained from
    // the root so append_events assigns linear ids.
    let mut t1_events: Vec<SessionEvent> = [
        SessionEventKind::Message(user("hi")),
        SessionEventKind::Message(assistant("hello")),
        SessionEventKind::TurnEnd {
            label: "m".into(),
            elapsed_ms: 10,
            cost: 0.0,
            usage: Usage::default(),
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent { id: String::new(), parent_id: None, kind })
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
            label: "m".into(),
            elapsed_ms: 5,
            error: "boom".into(),
            cost: 0.01,
            usage: Usage::default(),
        },
    ]
    .into_iter()
    .map(|kind| SessionEvent { id: String::new(), parent_id: None, kind })
    .collect();
    store::append_events(&path, &mut t2_events, Some(&checkpoint)).unwrap();

    let (_meta, events, _, _) = store::load(&path).unwrap();
    // The active leaf is the TurnFailed marker; the active path includes
    // the failed turn's messages (they're ancestors of TurnFailed).
    let path_idx = active_path_from_leaf(&events);
    assert_eq!(path_idx.len(), 6, "active path includes failed turn's msgs");
    assert!(matches!(&events[path_idx[0]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "hi")));
    assert!(matches!(&events[path_idx[1]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant));
    assert!(matches!(&events[path_idx[2]].kind, SessionEventKind::TurnEnd { .. }));
    assert!(matches!(&events[path_idx[3]].kind, SessionEventKind::Message(m) if m.role == Role::User && matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "oops")));
    assert!(matches!(&events[path_idx[4]].kind, SessionEventKind::Message(m) if m.role == Role::Assistant));
    assert!(matches!(&events[path_idx[5]].kind, SessionEventKind::TurnFailed { .. }));

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
            summarized: 1,
            kept: 2,
        },
        SessionEventKind::Message(assistant("continued")),
    ]
    .into_iter()
    .map(|kind| SessionEvent { id: String::new(), parent_id: None, kind })
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
        if let SessionEventKind::Compaction { first_kept_entry_id, .. } = &mut e.kind {
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
fn messages_from_events_elides_kept_tail_on_resume() {
    // After a compaction, resuming must rebuild the kept tail in its elided
    // (lightweight, recall-recoverable) form — not the verbatim on-disk tail.
    // Two exec results in the kept tail; with keep_results=1 only the most
    // recent survives verbatim, the older becomes a lofi.result stub.
    use lofi_types::{ContentBlock, SessionEvent, SessionEventKind};

    let exec_call = |id: &str| {
        Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "exec".to_string(),
                input: serde_json::json!({"code": "return 1"}),
            }],
        }
    };
    let exec_result = |id: &str, out: &str| {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: out.to_string(),
                is_error: false,
            }],
        }
    };

    // e0 summarized; e1..e4 kept tail; e5 marker (first_kept_entry_id = e1).
    let mut events: Vec<SessionEvent> = sev_chain([
        msg(user("old prompt")),
        msg(exec_call("t1")),
        msg(exec_result("t1", "out-1")),
        msg(exec_call("t2")),
        msg(exec_result("t2", "out-2")),
        SessionEventKind::Compaction {
            summary: "SUMMARY".to_string(),
            first_kept_entry_id: "e1".to_string(),
            summarized_range: [String::new(), String::new()],
            summarized: 1,
            kept: 4,
        },
    ]);

    let edit = lofi_types::EditConfig {
        enabled: true,
        keep_results: 1,
        keep_thinking: 0,
        keep_calls: 0,
    };
    let msgs = messages_from_events(&events, &edit);
    // [SUMMARY, exec_call t1, result out-1, exec_call t2, result out-2]
    assert_eq!(msgs.len(), 5);
    assert_eq!(user_text(&msgs[0]), "SUMMARY");
    // Most recent result kept verbatim.
    let ContentBlock::ToolResult { content, .. } = &msgs[4].blocks[0] else { panic!() };
    assert_eq!(content, "out-2");
    // Older result elided to a recoverable stub naming its event id (e2).
    let ContentBlock::ToolResult { content, .. } = &msgs[2].blocks[0] else { panic!() };
    assert!(content.contains("lofi.result(\"e2\")"), "got {content}");
    // Older tool-call code elided too.
    let ContentBlock::ToolUse { input, .. } = &msgs[1].blocks[0] else { panic!() };
    let code = input.get("code").and_then(|v| v.as_str()).unwrap_or("");
    assert!(code.contains("lofi.result(\"e1\")"), "got {code}");

    // With editing disabled, the tail comes back verbatim.
    let verbatim = messages_from_events(&events, &lofi_types::EditConfig { enabled: false, ..edit });
    let ContentBlock::ToolResult { content, .. } = &verbatim[2].blocks[0] else { panic!() };
    assert_eq!(content, "out-1");
}

fn user_text(m: &lofi_types::Message) -> &str {
    match &m.blocks[..] {
        [lofi_types::ContentBlock::Text { text }] => text,
        _ => "",
    }
}    #[test]
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
        label: "m".into(),
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
    assert!(!header.contains('$'), "header should not show cost: {header}");
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
        last.blocks.iter().any(|b| matches!(b, Block::Error(m) if m == "set OPENAI_API_KEY")),
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
    a.log_lines = vec![
        // gutter "  " + content "hello world" + padding "   "
        "  hello world   ".to_string(),
        // gutter "  " + rails "│ │ " + content "lofi-core…Agent {" + padding
        "  │ │ lofi-core/src/agent.rs:233:pub struct Agent {     ".to_string(),
        // gutter "  " + content "    let x = 1;" (indentation preserved!)
        "      let x = 1;".to_string(),
    ];
    a.log_content = vec![
        (2, 13),  // "hello world"
        (6, 51),  // "lofi-core/src/agent.rs:233:pub struct Agent {"
        (2, 16),  // "    let x = 1;"
    ];
    a.sel = Some(Selection { start: (0, 0), end: (2, 40) });
    assert_eq!(
        a.selection_text().as_deref(),
        Some("hello world\nlofi-core/src/agent.rs:233:pub struct Agent {\n    let x = 1;")
    );
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
    assert_eq!(a.thinking_label.as_deref(), Some(" · xhigh"));
    assert_eq!(a.ctx_limit, 200_000);
}
