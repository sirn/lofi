use super::*;

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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
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
fn oversized_turn_skips_the_collapsed_cache() {
    // A single entry past the per-turn cap reparses from the transcript on
    // demand instead of flushing every other entry out of the budget.
    use lofi_core::session::store::SessionStore;
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let cursor = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap();
    let path = cursor.path().to_path_buf();
    // Incompressible pseudo-random text so the lz4 entry stays above the cap.
    let mut entropy = String::with_capacity(2 * 1024 * 1024);
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    while entropy.len() < 2 * 1024 * 1024 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        entropy.push(char::from(b'!' + (state % 90) as u8));
    }
    let mut evs = vec![
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: entropy }],
                kind: PromptKind::default(),
            }),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "small answer".into(),
                }],
                kind: PromptKind::default(),
            }),
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "m".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        },
    ];
    test_append_events(&path, &mut evs, None).unwrap();

    let mut a = app();
    attach_session_sink(
        &mut a,
        store,
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
    let c0 = a.session.cursor.as_ref().unwrap().clone();
    let snap = c0.snapshot().unwrap();
    a.restore_indexed_session(&c0, &snap.index, snap.file_size)
        .unwrap();

    let rendered = a.materialize_turn(0);
    assert!(!rendered.blocks.is_empty());
    assert!(
        a.collapsed_turns.borrow().map.is_empty(),
        "entry above the cap must not be retained"
    );
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
        stop_reason: None,
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
    attach_session_sink(
        &mut a,
        session_store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::new(path, Some(branch_b_end.clone())),
    );
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
#[allow(clippy::too_many_lines)]
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
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
fn tree_revert_to_cancelled_turn_drops_aborted_tail() {
    // n: user prompt
    // n+1: agent message + TurnEnd (turn 1 completes)
    // turn 2: user prompt, agent partial (thinking), then TurnCancelled.
    // Reverting to the cancelled turn row must drop the aborted turn-2 tail and
    // land the head on turn 1's TurnEnd, not stay at the cancelled outcome
    // (which would be a visible no-op: its parent is already the head).
    use lofi_core::session::store::{self, SessionStore};
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let path = store
        .create_cursor(std::path::Path::new("/x"), &"m".into())
        .unwrap()
        .path()
        .to_path_buf();
    let kinds = [
        msg(user("first")),
        msg(assistant("hello")),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
        msg(user("second")),
        msg(assistant("partial")),
        SessionEventKind::TurnCancelled {
            model: "m".into(),
            elapsed_ms: 40,
            cost: 0.0,
            usage: Usage::default(),
        },
    ];
    let mut batch: Vec<SessionEvent> = kinds
        .into_iter()
        .map(|kind| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind,
        })
        .collect();
    test_append_events(&path, &mut batch, None).unwrap();
    let events = store::SessionCursor::open(path.clone())
        .unwrap()
        .load_tree_events()
        .unwrap();
    let partial_id = events[4].id.clone();
    let cancelled_id = events[5].id.clone();

    let mut a = app();
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
    assert!(a.slash_command("/tree"));
    let picker = a.tree_picker.as_ref().expect("picker opened");
    let cancelled_idx = picker
        .entries
        .iter()
        .position(|e| e.label.contains("(cancelled)"))
        .expect("cancelled turn row present");
    // The branch point is the aborted turn's last message, not the cancelled
    // outcome (which is already the leaf), so the revert drops only the marker.
    let branch_point = &picker.entries[cancelled_idx].branch_point;
    assert_eq!(*branch_point, partial_id);
    assert_ne!(*branch_point, cancelled_id);
    a.tree_picker.as_mut().unwrap().selected = cancelled_idx;
    a.tree_picker_confirm();
    assert!(a.tree_picker.is_none());
    assert_eq!(
        a.session
            .cursor
            .as_ref()
            .and_then(store::SessionCursor::leaf_id),
        Some(partial_id)
    );
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
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        session_store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "first".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "hello".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
        SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "second".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "world".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::new(path.clone(), None),
    );
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "tu1".into(),
                name: "bash".into(),
                input: serde_json::json!({"cmd": "ls"}),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tu1".into(),
                content: "file_a.txt file_b.txt".into(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: "done".into(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::ToolUse {
                id: "exec_0".into(),
                name: "exec".into(),
                input: serde_json::json!({"code": "..."}),
            }],
            kind: PromptKind::default(),
        }),
        SessionEventKind::Message(Message {
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "exec_0".into(),
                content: "exec result".into(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
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
            kind: PromptKind::default(),
        }),
        SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 100,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
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
    attach_session_sink(
        &mut a,
        store.clone(),
        std::path::Path::new("/x"),
        store::SessionCursor::open(path).unwrap(),
    );
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
