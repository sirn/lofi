//! /tree "branched off" branches: forked conversations must appear as tree
//! branches whether or not the branched-off line is the active leaf, and
//! however deeply nested. Guards the branch-detection walk against silently
//! dropping forks when the active leaf moves between branches.
#![allow(clippy::unwrap_used)]

use lofi_core::session::store::SessionStore;
use lofi_core::session::tree::build_tree_rows;
use lofi_types::{ContentBlock, Message, Role, SessionEvent, SessionEventKind, Usage};
use std::path::Path;
use tempfile::tempdir;

fn user_ev(text: &str) -> SessionEvent {
    SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }),
    }
}

fn assistant_ev(text: &str) -> SessionEvent {
    SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::Message(Message {
            role: Role::Assistant,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }),
    }
}

fn turn_end() -> SessionEvent {
    SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "m".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
        },
    }
}

fn row_labels(cursor: &lofi_core::session::store::SessionCursor) -> Vec<String> {
    let snap = cursor.tree_snapshot().unwrap();
    build_tree_rows(&snap.index, snap.leaf_id.as_deref(), cursor)
        .into_iter()
        .map(|r| r.label)
        .collect()
}

/// Shared two-branch topology rooted at p1: trunk continues into branch A
/// (p2) and branch B forks off t1 (pB, then nested pB2). Returns the cursor
/// plus both branch tips.
fn two_branch_fixture(
    dir: &tempfile::TempDir,
) -> (lofi_core::session::store::SessionCursor, String, String) {
    let store = SessionStore::new(dir.path().join("sessions"));
    let cursor = store.create_cursor(Path::new("/x"), &"m".into()).unwrap();
    let mut trunk = vec![user_ev("p1"), assistant_ev("a1"), turn_end()];
    cursor.append_events(&mut trunk).unwrap();
    let t1 = trunk[2].id.clone();

    let mut a = vec![user_ev("p2"), assistant_ev("a2"), turn_end()];
    cursor.append_events(&mut a).unwrap();
    let a_tip = a[2].id.clone();

    cursor.branch_from(t1).unwrap();
    let mut b = vec![user_ev("pB"), assistant_ev("aB"), turn_end()];
    cursor.append_events(&mut b).unwrap();
    let mut b2 = vec![user_ev("pB2"), assistant_ev("aB2"), turn_end()];
    cursor.append_events(&mut b2).unwrap();
    let b_tip = b2[2].id.clone();
    (cursor, a_tip, b_tip)
}

#[test]
fn tree_lists_branched_off_branch_when_active_is_original() {
    let dir = tempdir().unwrap();
    let (cursor, a_tip, _b_tip) = two_branch_fixture(&dir);
    cursor.branch_from(a_tip).unwrap();
    let labels = row_labels(&cursor);
    for expect in ["user: p1", "user: p2", "user: pB", "user: pB2"] {
        assert!(
            labels.iter().any(|l| l.contains(expect)),
            "missing {expect}: {labels:?}"
        );
    }
}

#[test]
fn tree_lists_original_branch_when_active_is_branched_off() {
    let dir = tempdir().unwrap();
    let (cursor, _a_tip, b_tip) = two_branch_fixture(&dir);
    cursor.branch_from(b_tip).unwrap();
    let labels = row_labels(&cursor);
    for expect in ["user: p1", "user: p2", "user: pB", "user: pB2"] {
        assert!(
            labels.iter().any(|l| l.contains(expect)),
            "missing {expect}: {labels:?}"
        );
    }
}

#[test]
fn tree_lists_all_root_sibling_branches() {
    let dir = tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("s"));
    let cursor = store.create_cursor(Path::new("/x"), &"m".into()).unwrap();
    let mut root = vec![user_ev("p0"), assistant_ev("a0"), turn_end()];
    cursor.append_events(&mut root).unwrap();
    let t0 = root[2].id.clone();

    let mut a = vec![user_ev("P2"), assistant_ev("a2"), turn_end()];
    cursor.append_events(&mut a).unwrap();
    let a_tip = a[2].id.clone();
    cursor.branch_from(t0.clone()).unwrap();
    let mut b = vec![user_ev("P3"), assistant_ev("a3"), turn_end()];
    cursor.append_events(&mut b).unwrap();
    cursor.branch_from(t0).unwrap();
    let mut c = vec![user_ev("P4"), assistant_ev("a4"), turn_end()];
    cursor.append_events(&mut c).unwrap();

    cursor.branch_from(a_tip).unwrap();
    let labels = row_labels(&cursor);
    for expect in ["user: P2", "user: P3", "user: P4"] {
        assert!(
            labels.iter().any(|l| l.contains(expect)),
            "missing {expect}: {labels:?}"
        );
    }
}
