#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use super::index::{load_compaction_path, load_index, load_indexed_path};
use super::*;
use lofi_types::{ContentBlock, PromptKind, Role, SessionEventKind, ThinkingLevel, Usage};
use std::io::Write as _;

fn ev(msg: Message) -> SessionEvent {
    SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::Message(msg),
    }
}
use tempfile::tempdir;

fn user(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

fn assistant(text: &str) -> Message {
    Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    }
}

fn message_text(event: &SessionEvent) -> &str {
    match &event.kind {
        SessionEventKind::Message(message) => match &message.blocks[0] {
            ContentBlock::Text { text } => text,
            _ => panic!("expected text"),
        },
        _ => panic!("expected message"),
    }
}

/// A store rooted at a fresh temp dir, so tests never touch real state
/// and never race on the process-global `XDG_STATE_HOME` env var.
fn isolated_store() -> (tempfile::TempDir, SessionStore) {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("sessions"));
    (dir, store)
}

#[test]
fn failed_append_rolls_back_and_preserves_selected_head() {
    const CHILD_MODE: &str = "LOFI_APPEND_FAILURE_TEST";
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        for mode in ["events", "cursor", "checkpoint"] {
            // A separate process keeps the file limit and signal disposition
            // out of parallel tests. Ignore SIGXFSZ so writes return errors.
            let output = std::process::Command::new("sh")
                .args([
                    "-c",
                    "trap '' XFSZ; ulimit -f 8; exec \"$1\" --exact session::store::tests::failed_append_rolls_back_and_preserves_selected_head --nocapture",
                    "append-failure",
                ])
                .arg(std::env::current_exe().unwrap())
                .env(CHILD_MODE, mode)
                .output()
                .unwrap();
            assert!(output.status.success(), "{mode}: {output:?}");
        }
        return;
    };
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/append-failure"), &"p/m".into())
        .unwrap();
    cursor.append_events(&mut [ev(user("baseline"))]).unwrap();
    let before = std::fs::read(cursor.path()).unwrap();
    let leaf = cursor.leaf_id();
    let huge = user(&"x".repeat(64 * 1024));
    let result = match mode.as_str() {
        "events" => append_events(cursor.path(), &mut [ev(huge)], leaf.as_deref()),
        "cursor" => cursor.append_events(&mut [ev(huge)]),
        "checkpoint" => cursor.append_compaction(
            &[user("partial checkpoint"), huge],
            "summary",
            &[leaf.clone().unwrap(), leaf.clone().unwrap()],
            CompactionCounts {
                summarized: 1,
                represented: 1,
                kept: 2,
            },
        ),
        _ => panic!("unknown append mode"),
    };
    assert!(matches!(result, Err(Error::State(_))));
    assert_eq!(std::fs::read(cursor.path()).unwrap(), before);
    assert_eq!(cursor.leaf_id(), leaf);
    assert_eq!(cursor.load_events().unwrap().len(), 1);
    assert_eq!(cursor.load_compaction_events().unwrap().len(), 1);
    let reopened = SessionCursor::open(cursor.path().to_path_buf()).unwrap();
    assert_eq!(reopened.leaf_id(), leaf);
    cursor.append_events(&mut [ev(user("retry"))]).unwrap();
    let events = cursor.load_events().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(message_text(&events[1]), "retry");
    assert_eq!(events[1].parent_id, leaf);
}

#[test]
fn create_load_round_trip() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/project");
    let path = store.create(cwd, &"openai/gpt-5.6-sol".into()).unwrap();
    let (meta, msgs, _, _) = load(&path).unwrap();
    assert_eq!(meta.version, SESSION_VERSION);
    assert_eq!(meta.cwd, "/tmp/project");
    assert_eq!(meta.model, "openai/gpt-5.6-sol".into());
    assert!(msgs.is_empty());
}

#[test]
fn recovery_message_lookup_is_bounded_and_typed() {
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/recovery-message"), &"p/m".into())
        .unwrap();
    let mut events = [
        ev(Message {
            origin: None,
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "x".repeat(4 * 1024 * 1024),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        }),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::ThinkingTiming { elapsed_ms: 1 },
        },
    ];
    cursor.append_events(&mut events).unwrap();

    let Some(RecoveryMessage::Message(message)) =
        cursor.recovery_message_by_id(&events[0].id).unwrap()
    else {
        panic!("expected recoverable message");
    };
    let ContentBlock::ToolResult { content, .. } = &message.blocks[0] else {
        panic!("expected tool result");
    };
    assert!(content.len() <= crate::agent::MAX_EXEC_RESULT_BYTES);
    assert!(content.contains("transcript content truncated for display"));
    assert!(matches!(
        cursor.recovery_message_by_id(&events[1].id).unwrap(),
        Some(RecoveryMessage::NotMessage)
    ));
    assert!(cursor
        .recovery_message_by_id("missing-event")
        .unwrap()
        .is_none());
}

#[test]
fn display_event_collection_keeps_newest_content() {
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/display-budget"), &"p/m".into())
        .unwrap();
    let payload = "x".repeat(300 * 1024);
    let mut events: Vec<SessionEvent> = (0..12)
        .map(|index| ev(user(&format!("{index}:{payload}"))))
        .collect();
    cursor.append_events(&mut events).unwrap();
    let offsets: Vec<u64> = cursor
        .snapshot()
        .unwrap()
        .index
        .iter()
        .map(|entry| entry.offset)
        .collect();

    let displayed = cursor.display_events_at(&offsets).unwrap();

    assert_eq!(displayed.len(), 12);
    assert!(message_text(displayed.last().unwrap()).starts_with("11:"));
    assert!(message_text(displayed.first().unwrap()).is_empty());
}

#[test]
fn linear_snapshot_ignores_only_the_final_malformed_record() {
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/partial-resume"), &"p/m".into())
        .unwrap();
    let mut events = [ev(user("committed"))];
    cursor.append_events(&mut events).unwrap();
    let path = cursor.path().to_path_buf();
    let committed_size = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(br#"{"id":"partial"#)
        .unwrap();

    let (resumed, snapshot) = SessionCursor::open_snapshot(path.clone()).unwrap();

    assert_eq!(snapshot.file_size, committed_size);
    assert_eq!(snapshot.index.len(), 1);
    assert_eq!(
        resumed.event_at(snapshot.index[0].offset).unwrap().id,
        events[0].id
    );

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let (_, snapshot) = SessionCursor::open_snapshot(path.clone()).unwrap();
    assert_eq!(snapshot.file_size, committed_size);

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{}\n")
        .unwrap();
    assert!(SessionCursor::open_snapshot(path).is_err());
}

#[test]
fn linear_snapshot_compaction_cache_falls_back_to_marker() {
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/missing-kept-id"), &"p/m".into())
        .unwrap();
    let mut events = [
        ev(user("before")),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Compaction {
                summary: "summary".into(),
                first_kept_entry_id: "missing".into(),
                summarized_range: [String::new(), String::new()],
                checkpointed_tail: true,
                summarized: 1,
                represented: 1,
                kept: 0,
            },
        },
    ];
    cursor.append_events(&mut events).unwrap();

    let (resumed, snapshot) = SessionCursor::open_snapshot(cursor.path().to_path_buf()).unwrap();
    let compacted = resumed.load_compaction_events().unwrap();

    assert_eq!(snapshot.history_start, 1);
    assert_eq!(compacted.len(), 1);
    assert!(matches!(
        compacted[0].kind,
        SessionEventKind::Compaction { .. }
    ));
}

#[test]
fn turn_end_model_is_raw_not_rendered() {
    // The session file must store the model as raw data (a `{provider, id,
    // thinking}` object), never a rendered `provider/id:level` string —
    // a later display-format change must not strand stale text in old files.
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/raw");
    let path = store
        .create(cwd, &"openai/gpt-5.6-sol:medium".into())
        .unwrap();
    let header = std::fs::read_to_string(&path).unwrap();
    assert!(
        header.contains(
            "\"model\":{\"provider\":\"openai\",\"id\":\"gpt-5.6-sol\",\"thinking\":\"medium\",\"service_tier\":\"auto\"}"
        ),
        "header model must be a raw object: {header}"
    );
    assert!(
        !header.contains("\"model\":\""),
        "header must not store a rendered model string: {header}"
    );

    let mut batch = [SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "anthropic/claude:high".into(),
            elapsed_ms: 5,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    }];
    append_events(&path, &mut batch, None).unwrap();
    let body = std::fs::read_to_string(&path).unwrap();
    let turn_end_line = body.lines().last().unwrap();
    assert!(
        turn_end_line.contains(
            "\"model\":{\"provider\":\"anthropic\",\"id\":\"claude\",\"thinking\":\"high\",\"service_tier\":\"auto\"}"
        ),
        "turn-end model must be a raw object: {turn_end_line}"
    );
    assert!(
        !turn_end_line.contains("\"label\""),
        "turn-end must not carry a rendered label field: {turn_end_line}"
    );
    assert!(
        !turn_end_line.contains("\"model\":\""),
        "turn-end must not store a rendered model string: {turn_end_line}"
    );
}

#[test]
fn compaction_checkpoint_round_trips_as_one_chain() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/compact-checkpoint"), &"p/m".into())
        .unwrap();
    let mut original = [ev(user("old"))];
    append_events(&path, &mut original, None).unwrap();

    let kept = [assistant("kept")];
    let (_, _, checkpoint_leaf) = append_compaction(
        &path,
        &kept,
        None,
        "summary",
        &["first".into(), "last".into()],
        CompactionCounts {
            summarized: 5,
            represented: 5,
            kept: 1,
        },
    )
    .unwrap();

    let (_meta, events, _, _) = load(&path).unwrap();
    assert_eq!(events.len(), 3);
    let SessionEventKind::Compaction {
        first_kept_entry_id,
        summary,
        ..
    } = &events[2].kind
    else {
        panic!("expected compaction marker");
    };
    assert_eq!(summary, "summary");
    assert_eq!(first_kept_entry_id, &events[1].id);
    assert_eq!(events[1].parent_id.as_deref(), Some(events[0].id.as_str()));
    assert_eq!(events[2].parent_id.as_deref(), Some(events[1].id.as_str()));
    assert_ne!(events[0].id, events[1].id);
    assert_ne!(events[1].id, events[2].id);
    assert_eq!(checkpoint_leaf, events[2].id);
}

#[test]
fn compaction_checkpoint_honors_branch_parent() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/compact-branch"), &"p/m".into())
        .unwrap();
    let mut original = [ev(user("root")), ev(assistant("latest sibling"))];
    append_events(&path, &mut original, None).unwrap();

    append_compaction(
        &path,
        &[assistant("checkpoint")],
        Some(&original[0].id),
        "summary",
        &[original[0].id.clone(), original[0].id.clone()],
        CompactionCounts {
            summarized: 1,
            represented: 1,
            kept: 1,
        },
    )
    .unwrap();

    let (_meta, events, _, _) = load(&path).unwrap();
    assert_eq!(events[2].parent_id.as_deref(), Some(events[0].id.as_str()));
    assert_eq!(events[3].parent_id.as_deref(), Some(events[2].id.as_str()));
    assert!(!active_path_from_leaf(&events).contains(&1));
}

#[test]
fn cursor_owns_lineage_across_clones_and_compaction() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/cursor-lineage"), &"p/m".into())
        .unwrap();
    let cursor = SessionCursor::new(path.clone(), None);

    let mut root = [ev(user("root"))];
    cursor.append_events(&mut root).unwrap();
    let root_id = root[0].id.clone();
    assert_eq!(cursor.leaf_id().as_deref(), Some(root_id.as_str()));

    let clone = cursor.clone();
    let mut reply = [ev(assistant("reply"))];
    clone.append_events(&mut reply).unwrap();
    assert_eq!(cursor.leaf_id().as_deref(), Some(reply[0].id.as_str()));

    cursor
        .append_compaction(
            &[assistant("kept")],
            "summary",
            &[root_id.clone(), reply[0].id.clone()],
            CompactionCounts {
                summarized: 2,
                represented: 2,
                kept: 1,
            },
        )
        .unwrap();
    let marker_id = cursor.leaf_id().unwrap();
    let (_, events, _, _) = load(&path).unwrap();
    let marker = events.iter().find(|event| event.id == marker_id).unwrap();
    assert!(matches!(marker.kind, SessionEventKind::Compaction { .. }));
}

#[test]
fn compaction_preserves_images_without_restoring_oversized_result_text() {
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/compaction-image"), &"p/m".into())
        .unwrap();
    let image = lofi_types::ToolResultImage {
        bytes: vec![1; 512 * 1024],
        media_type: "image/png".into(),
    };
    let mut events = [ev(Message {
        origin: None,
        role: Role::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: "image-result".into(),
            content: "x".repeat(4 * 1024 * 1024),
            is_error: false,
            images: vec![image.clone()],
        }],
        kind: PromptKind::default(),
    })];
    cursor.append_events(&mut events).unwrap();
    let assert_result = |events: &[SessionEvent], expected_image: &lofi_types::ToolResultImage| {
        let SessionEventKind::Message(message) = &events[0].kind else {
            panic!("expected message");
        };
        let ContentBlock::ToolResult {
            content, images, ..
        } = &message.blocks[0]
        else {
            panic!("expected tool result");
        };
        assert!(content.len() <= crate::agent::MAX_EXEC_RESULT_BYTES);
        assert!(content.contains("transcript content truncated for display"));
        assert_eq!(images, std::slice::from_ref(expected_image));
    };
    assert_result(&cursor.load_compaction_events().unwrap(), &image);
    let displayed = cursor
        .display_events_at(&[cursor.snapshot().unwrap().index[0].offset])
        .unwrap();
    assert_result(
        &displayed,
        &lofi_types::ToolResultImage {
            bytes: Vec::new(),
            media_type: image.media_type.clone(),
        },
    );
}

#[test]
fn compaction_preserves_images_after_text_budget_is_exhausted() {
    let (_guard, store) = isolated_store();
    let cursor = store
        .create_cursor(Path::new("/tmp/compaction-image-budget"), &"p/m".into())
        .unwrap();
    let image = lofi_types::ToolResultImage {
        bytes: vec![2; 512 * 1024],
        media_type: "image/jpeg".into(),
    };
    let blocks = vec![
        ContentBlock::Text {
            text: "inspect image".into(),
        },
        ContentBlock::Image {
            bytes: vec![1; 512 * 1024],
            media_type: "image/png".into(),
        },
        ContentBlock::ToolResult {
            tool_use_id: "image-result".into(),
            content: "image result".into(),
            is_error: false,
            images: vec![image.clone()],
        },
    ];
    let mut events = [
        ev(Message {
            origin: None,
            role: Role::User,
            blocks: blocks.clone(),
            kind: PromptKind::default(),
        }),
        ev(Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::Text {
                    text: "x".repeat(2 * crate::agent::MAX_EXEC_RESULT_BYTES),
                };
                11
            ],
            kind: PromptKind::default(),
        }),
    ];
    cursor.append_events(&mut events).unwrap();
    let projected = cursor.load_compaction_events().unwrap();
    let SessionEventKind::Message(message) = &projected[0].kind else {
        panic!("expected message");
    };
    assert!(matches!(&message.blocks[0], ContentBlock::Text { text } if text.is_empty()));
    assert_eq!(message.blocks[1], blocks[1]);
    let ContentBlock::ToolResult {
        content, images, ..
    } = &message.blocks[2]
    else {
        panic!("expected tool result");
    };
    assert!(content.is_empty());
    assert_eq!(images, std::slice::from_ref(&image));
}

#[test]
fn cursor_compaction_cache_replaces_history_with_checkpoint_suffix() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/cursor-compaction-cache"), &"p/m".into())
        .unwrap();
    let cursor = SessionCursor::new(path, None);
    let mut original = [ev(user("old")), ev(assistant("old reply"))];
    cursor.append_events(&mut original).unwrap();
    assert_eq!(cursor.load_compaction_events().unwrap().len(), 2);

    cursor
        .append_compaction(
            &[assistant("kept")],
            "summary",
            &[original[0].id.clone(), original[1].id.clone()],
            CompactionCounts {
                summarized: 2,
                represented: 2,
                kept: 1,
            },
        )
        .unwrap();
    let mut continuation = [ev(user("new")), ev(assistant("new reply"))];
    cursor.append_events(&mut continuation).unwrap();

    let events = cursor.load_compaction_events().unwrap();
    assert_eq!(events.len(), 4);
    assert!(matches!(
        &events[0].kind,
        SessionEventKind::Message(message)
            if message.blocks.iter().any(|block| matches!(
                block,
                ContentBlock::Text { text } if text == "kept"
            ))
    ));
    assert!(matches!(
        events[1].kind,
        SessionEventKind::Compaction { .. }
    ));
    assert_eq!(events[2].id, continuation[0].id);
    assert_eq!(events[3].id, continuation[1].id);
    assert!(!events.iter().any(|event| event.id == original[0].id));
    assert!(!events.iter().any(|event| event.id == original[1].id));
}

#[test]
fn cursor_selection_survives_restart_and_beats_physical_eof() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/cursor-restart"), &"p/m".into())
        .unwrap();
    let cursor = SessionCursor::new(path.clone(), None);
    let mut trunk = [ev(user("root")), ev(assistant("old leaf"))];
    cursor.append_events(&mut trunk).unwrap();
    assert!(!std::fs::read_to_string(&path)
        .unwrap()
        .contains("\"type\":\"cursor\""));
    let root_id = trunk[0].id.clone();
    let old_leaf = trunk[1].id.clone();

    cursor.branch_from(root_id.clone()).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap()
            .matches("\"type\":\"cursor\"")
            .count(),
        1
    );
    drop(cursor);

    let resumed = SessionCursor::open(path.clone()).unwrap();
    assert_eq!(resumed.leaf_id().as_deref(), Some(root_id.as_str()));
    let snapshot = resumed.snapshot().unwrap();
    assert_eq!(snapshot.index.len(), 1);
    let selected = resumed.event_at(snapshot.index[0].offset).unwrap();
    assert_eq!(selected.id, root_id);

    let mut branch = [ev(user("new branch"))];
    resumed.append_events(&mut branch).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap()
            .matches("\"type\":\"cursor\"")
            .count(),
        1
    );
    assert_eq!(branch[0].parent_id.as_deref(), Some(trunk[0].id.as_str()));
    assert_ne!(branch[0].parent_id.as_deref(), Some(old_leaf.as_str()));

    let reopened = SessionCursor::open(path).unwrap();
    assert_eq!(reopened.leaf_id().as_deref(), Some(branch[0].id.as_str()));
}

#[test]
fn tree_active_path_is_not_shadowed_by_cursor_record() {
    // A cursor record carries the selected leaf in `id`, so an id->index
    // map that does not exclude cursor records resolves the leaf to the
    // record (no parent) and truncates the active path to the tail. The
    // active path must reach back to the root instead.
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/tree-cursor-shadow"), &"p/m".into())
        .unwrap();
    let cursor = SessionCursor::new(path.clone(), None);
    let mut events: Vec<SessionEvent> = (0..8).map(|i| ev(user(&format!("turn {i}")))).collect();
    cursor.append_events(&mut events).unwrap();

    let reopened = SessionCursor::open(path).unwrap();
    let snapshot = reopened.tree_snapshot().unwrap();
    let leaf = snapshot.leaf_id.as_deref().expect("leaf set after append");
    // Mirror the picker's map construction: id -> index, excluding cursor
    // records, which is the invariant under test.
    let by_id: std::collections::HashMap<&IndexId, usize> = snapshot
        .index
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind != IndexKind::Cursor && !e.id.is_empty())
        .map(|(i, e)| (&e.id, i))
        .collect();
    let path = crate::session::tree::active_path_from_index(&snapshot.index, &by_id, leaf);
    assert_eq!(
        path.len(),
        8,
        "active path must walk from leaf back through every turn, not stop at the cursor record"
    );
}

#[test]
fn cursor_none_is_explicit_root_not_physical_eof() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/cursor-root"), &"p/m".into())
        .unwrap();
    let mut existing = [ev(user("existing"))];
    append_events(&path, &mut existing, None).unwrap();

    let cursor = SessionCursor::new(path.clone(), None);
    let mut new_root = [ev(user("new root"))];
    cursor.append_events(&mut new_root).unwrap();
    assert!(new_root[0].parent_id.is_none());

    let (_, index, _) = load_index(&path).unwrap();
    let indexed_root = index
        .iter()
        .find(|event| event.id.matches(&new_root[0].id))
        .unwrap();
    assert!(indexed_root.parent_id.is_none());
    assert!(load_compaction_path(&path, &index, None)
        .unwrap()
        .is_empty());
    assert!(load_indexed_path(&path, &index, None).unwrap().is_empty());
    let branch = load_indexed_path(&path, &index, Some(&new_root[0].id)).unwrap();
    assert_eq!(branch.len(), 1);
    assert_eq!(branch[0].id, new_root[0].id);
}

#[test]
fn cursor_compaction_none_is_explicit_root_not_physical_eof() {
    let (_guard, store) = isolated_store();
    let path = store
        .create(Path::new("/tmp/cursor-compact-root"), &"p/m".into())
        .unwrap();
    let mut existing = [ev(user("existing"))];
    append_events(&path, &mut existing, None).unwrap();

    let cursor = SessionCursor::new(path.clone(), None);
    cursor
        .append_compaction(
            &[assistant("kept")],
            "summary",
            &["first".into(), "last".into()],
            CompactionCounts {
                summarized: 1,
                represented: 1,
                kept: 1,
            },
        )
        .unwrap();

    let (_, events, _, _) = load(&path).unwrap();
    assert!(events[1].parent_id.is_none());
    assert_eq!(events[2].parent_id.as_deref(), Some(events[1].id.as_str()));
}

#[test]
fn append_then_load_messages() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/proj2");
    let path = store.create(cwd, &"openai/gpt-4o".into()).unwrap();
    let mut batch = [ev(user("hello")), ev(assistant("hi there"))];
    append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(&events[0].kind, SessionEventKind::Message(m) if m.role == Role::User));
    assert!(matches!(&events[1].kind, SessionEventKind::Message(m) if m.role == Role::Assistant));
    assert!(events[0].parent_id.is_none());
    assert_eq!(events[1].parent_id.as_deref(), Some(events[0].id.as_str()));
}

#[test]
fn events_round_trip_with_timings_and_cost() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/events");
    let path = store.create(cwd, &"m".into()).unwrap();
    let mut batch = [
        ev(user("hi")),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::ToolTiming {
                tool_call_id: "t1".into(),
                elapsed_ms: 5,
            },
        },
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/m:medium".into(),
                elapsed_ms: 1234,
                cost: 0.01,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 20,
                    ..Usage::default()
                },
                stop_reason: None,
            },
        },
    ];
    append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[0].kind, SessionEventKind::Message(m) if m.role == Role::User));
    assert!(
        matches!(&events[1].kind, SessionEventKind::ToolTiming { tool_call_id, elapsed_ms: 5 } if tool_call_id == "t1")
    );
    assert!(
        matches!(&events[2].kind, SessionEventKind::TurnEnd { model, elapsed_ms: 1234, cost, usage, .. }
        if model.label() == "p/m:medium" && (*cost - 0.01).abs() < 1e-9 && usage.input_tokens == 10)
    );
}

#[test]
fn append_with_parent_hint_branches_off_target() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/branch");
    let path = store.create(cwd, &"m".into()).unwrap();
    let mut first = [ev(user("a")), ev(assistant("b"))];
    append_events(&path, &mut first, None).unwrap();
    let (_meta, base, _, _) = load(&path).unwrap();
    let root_id = base[0].id.clone();
    let mut branch = [ev(user("alt"))];
    append_events(&path, &mut branch, Some(&root_id)).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    assert_eq!(events.len(), 3);
    let branch_ev = events
        .iter()
        .find(|e| {
            matches!(&e.kind, SessionEventKind::Message(m) if m.role == Role::User
            && m.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "alt")))
        })
        .expect("branch event present");
    assert_eq!(branch_ev.parent_id.as_deref(), Some(root_id.as_str()));
    let path_idx = active_path(&events, &branch_ev.id);
    assert_eq!(path_idx.len(), 2);
    assert!(
        matches!(&events[path_idx[0]].kind, SessionEventKind::Message(m) if m.role == Role::User)
    );
    assert!(
        matches!(&events[path_idx[1]].kind, SessionEventKind::Message(m) if m.role == Role::User)
    );
}

#[test]
fn last_run_model_is_final_turn_on_active_path() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/lrm-final");
    let path = store.create(cwd, &"p/orig:medium".into()).unwrap();
    let mut batch = [
        ev(user("hi")),
        ev(assistant("hey")),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/switched:high".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        },
    ];
    append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    let m = last_run_model(&events).expect("a turn-end marker is present");
    assert_eq!(m.provider, "p");
    assert_eq!(m.id, "switched");
    assert_eq!(m.thinking, ThinkingLevel::High);
}

#[test]
fn last_run_model_uses_turn_failed() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/lrm-failed");
    let path = store.create(cwd, &"p/orig".into()).unwrap();
    let mut batch = [
        ev(user("hi")),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnFailed {
                model: "p/boom:low".into(),
                elapsed_ms: 1,
                error: "oops".into(),
                cost: 0.0,
                usage: Usage::default(),
            },
        },
    ];
    append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    let m = last_run_model(&events).expect("a turn-failed marker is present");
    assert_eq!(m.id, "boom");
    assert_eq!(m.thinking, ThinkingLevel::Low);
}

#[test]
fn last_run_model_none_without_a_turn_marker() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/lrm-none");
    let path = store.create(cwd, &"p/orig".into()).unwrap();
    let mut batch = [ev(user("hi")), ev(assistant("partial"))];
    append_events(&path, &mut batch, None).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    assert!(last_run_model(&events).is_none());
}

#[test]
fn last_run_model_skips_sibling_branch() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/lrm-branch");
    let path = store.create(cwd, &"p/orig".into()).unwrap();
    let mut first = [
        ev(user("a")),
        ev(assistant("b")),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/A:medium".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        },
    ];
    append_events(&path, &mut first, None).unwrap();
    let (_meta, base, _, _) = load(&path).unwrap();
    let root_id = base[0].id.clone();
    let mut branch = [
        ev(user("alt")),
        ev(assistant("alt2")),
        SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::TurnEnd {
                model: "p/B:high".into(),
                elapsed_ms: 1,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
            },
        },
    ];
    append_events(&path, &mut branch, Some(&root_id)).unwrap();
    let (_meta, events, _, _) = load(&path).unwrap();
    let m = last_run_model(&events).expect("active leaf has a turn-end");
    assert_eq!(m.id, "B");
    assert_eq!(m.thinking, ThinkingLevel::High);
}

#[test]
fn list_newest_first_and_counts() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/listy");
    let p1 = store.create(cwd, &"m".into()).unwrap();
    let mut batch = [ev(user("a"))];
    append_events(&p1, &mut batch, None).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let p2 = store.create(cwd, &"m".into()).unwrap();
    let list = store.list_for_cwd(cwd).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].file.path, p2);
    assert_eq!(list[1].file.path, p1);
    assert_eq!(list[1].message_count, 1);
    assert_eq!(list[0].message_count, 0);
}

#[test]
fn session_state_permissions_are_private_and_repaired() {
    use std::os::unix::fs::PermissionsExt as _;

    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/private-session");
    let path = store.create(cwd, &"p/m".into()).unwrap();
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    store.list_files_for_cwd(cwd).unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn legacy_user_bash_event_loads_as_unknown() {
    let event = parse_event(
        r#"{"id":"a","parent_id":null,"type":"user_bash","command":"echo old","output":"old\n","exit_code":0,"signal":null,"duration_ms":1,"truncated":false,"cancelled":false,"exclude_from_context":false}"#,
    )
    .unwrap();
    assert!(matches!(event.kind, SessionEventKind::Unknown));
}

#[test]
fn user_message_kind_field_roundtrips_through_disk() {
    // Typed (default) messages omit `kind` from the wire; notice turns
    // carry `kind: "notice"`. Both must deserialize back to the right
    // `PromptKind` so replay can rebuild each turn faithfully.
    let dir = tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    let jsonl = concat!(
        "{\"type\":\"meta\",\"version\":1,\"created\":0,\"cwd\":\"\",\"model\":{\"provider\":\"p\",\"id\":\"m\",\"thinking\":\"medium\"}}\n",
        "{\"id\":\"\",\"type\":\"cursor\"}\n",
        "{\"id\":\"a\",\"parent_id\":null,\"type\":\"message\",\"role\":\"user\",\"blocks\":[{\"type\":\"text\",\"text\":\"typed\"}]}\n",
        "{\"id\":\"b\",\"parent_id\":\"a\",\"type\":\"message\",\"role\":\"user\",\"kind\":\"notice\",\"blocks\":[{\"type\":\"text\",\"text\":\"job done\"}]}\n",
    );
    std::fs::write(&path, jsonl).unwrap();
    let cursor = SessionCursor::new(path, None);
    let events = cursor.load_tree_events().unwrap();
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|ev| match &ev.kind {
            SessionEventKind::Message(m) if matches!(m.role, Role::User) => Some(m.kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        vec![lofi_types::PromptKind::User, lofi_types::PromptKind::Notice]
    );
}

#[test]
fn resume_preview_coalesces_streamed_assistant_chunks_and_skips_system_messages() {
    let (_guard, store) = isolated_store();
    let cwd = Path::new("/tmp/resume-preview");
    let path = store.create(cwd, &"p/m".into()).unwrap();
    let long_system = "S".repeat(6000);
    let mut batch = [
        ev(user("what is lofi?")),
        ev(Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![ContentBlock::Thinking {
                text: "considering".into(),
                signature: None,
                redacted: false,
            }],
            kind: PromptKind::default(),
        }),
        ev(assistant("## About")),
        ev(assistant(" Lofi\n\nDetailed body.")),
        ev(assistant(" trailing fragment.")),
        ev(Message {
            origin: None,
            role: Role::System,
            blocks: vec![ContentBlock::Text { text: long_system }],
            kind: PromptKind::default(),
        }),
    ];
    append_events(&path, &mut batch, None).unwrap();

    let files = store.list_files_for_cwd(cwd).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].quick_preview().as_deref(),
        Some("agent: ## About Lofi")
    );
    let entry = files[0].inspect().unwrap();
    assert_eq!(entry.last_message, "agent: ## About Lofi");
    assert_eq!(entry.message_count, 3);
}
