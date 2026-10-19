//! Used by the `/tree` picker to avoid a full [`super::load`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::index_parser::{read_index_event, read_projected_value, ProjectionBudget};
use super::{Header, SessionMeta, SESSION_VERSION};
use lofi_error::{Error, Result};
use lofi_types::{SessionEvent, SessionEventKind};

mod events;
mod load;
mod preview;
mod scan;

#[allow(clippy::wildcard_imports)]
pub(in crate::session::store) use self::{events::*, load::*, preview::*, scan::*};
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct IndexId(IndexIdRepr);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
enum IndexIdRepr {
    #[default]
    Empty,
    Uuid([u8; 16]),
    Other(Box<str>),
}

impl IndexId {
    /// Parse from an owned string, keeping the allocation for heap ids.
    #[must_use]
    pub fn parse(value: String) -> Self {
        if value.is_empty() {
            return Self(IndexIdRepr::Empty);
        }
        if value.len() == 32 {
            if let Ok(id) = u128::from_str_radix(&value, 16) {
                return Self(IndexIdRepr::Uuid(id.to_be_bytes()));
            }
        }
        Self(IndexIdRepr::Other(value.into_boxed_str()))
    }

    /// Parse from a borrowed str. UUID ids become `u128` with no allocation;
    /// heap ids allocate one boxed str. This is the scan path's zero-copy
    /// constructor: callers feed it the `&str` out of the line buffer.
    #[must_use]
    pub fn borrow(value: &str) -> Self {
        if value.is_empty() {
            return Self(IndexIdRepr::Empty);
        }
        if value.len() == 32 {
            if let Ok(id) = u128::from_str_radix(value, 16) {
                return Self(IndexIdRepr::Uuid(id.to_be_bytes()));
            }
        }
        Self(IndexIdRepr::Other(value.into()))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.0, IndexIdRepr::Empty)
    }

    #[must_use]
    pub fn matches(&self, value: &str) -> bool {
        match &self.0 {
            IndexIdRepr::Empty => value.is_empty(),
            IndexIdRepr::Uuid(id) => {
                value.len() == 32
                    && u128::from_str_radix(value, 16).is_ok_and(|value| value.to_be_bytes() == *id)
            }
            IndexIdRepr::Other(id) => id.as_ref() == value,
        }
    }

    #[must_use]
    pub fn to_event_id(&self) -> String {
        match &self.0 {
            IndexIdRepr::Empty => String::new(),
            IndexIdRepr::Uuid(id) => format!("{:032x}", u128::from_be_bytes(*id)),
            IndexIdRepr::Other(id) => id.to_string(),
        }
    }
}

/// Lightweight per-event index entry: just enough to build the event tree
/// structure (id, `parent_id`, offset) and identify tree-node kinds, without
/// deserializing message content. Used by `/tree` to avoid a full `load`.
///
/// The struct is kept small deliberately: a full-tree index holds one entry
/// per transcript event (tens of thousands on long sessions) and both `/tree`
/// and resume materialize lineage copies, so per-entry width directly scales
/// resident memory. A cursor record carries its selected leaf in the `id`
/// slot (cursor records have no event id of their own) rather than a separate
/// mostly-None `cursor_leaf: Option<IndexId>` field.
#[derive(Debug, Clone)]
pub struct EventIndex {
    pub id: IndexId,
    pub parent_id: Option<IndexId>,
    pub offset: u64,
    /// Byte offset immediately after this event line. Keeping the exact line
    /// end lets file-backed branch replay stop at the selected lineage event
    /// instead of reading later sibling branches from the append-only file.
    pub end_offset: u64,
    pub kind: IndexKind,
}

/// Selected-lineage metadata used by normal resume and branch restore.
/// Graph IDs are needed only while resolving a lineage. Keeping them out of
/// the durable-view projection makes resume memory scale with three words per
/// event instead of two IDs plus offsets.
#[derive(Debug, Clone, Copy)]
pub struct SessionIndexEntry {
    pub offset: u64,
    pub end_offset: u64,
    pub kind: IndexKind,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    UserPrompt,
    UserShell,
    AssistantMessage,
    SystemMessage,
    /// A tool-result message (`role: tool`). Distinguished from `UserPrompt`
    /// so the tree can show it as a `tool:` node and `find_turn_outcome` can
    /// follow it — the function follows non-`UserPrompt` children, so a
    /// tool-result turn's outcome is only reachable when this is a tree node.
    ToolResult,
    NativeTool,
    TurnEnd,
    TurnFailed,
    TurnCancelled,
    Compaction,
    Cursor,
    /// `JobStarted` / `JobFinished` markers; lets reconciliation skip the
    /// full event slice.
    JobLifecycle,
    BackendUsage,
    SubagentUsage,
    /// Terminal record of a subagent run; rehydration finds it as the leaf
    /// of the selected lineage. Never model- or display-visible.
    RunSettled,
    Other,
}

/// # Errors
/// Returns the underlying IO error if the session file cannot be read or
/// an event line cannot be parsed.
pub(super) fn index_kind(kind_type: &str, role: Option<&str>) -> IndexKind {
    match kind_type {
        "message" | "" => match role {
            Some("user") => IndexKind::UserPrompt,
            Some("assistant") => IndexKind::AssistantMessage,
            Some("tool") => IndexKind::ToolResult,
            Some("system") => IndexKind::SystemMessage,
            _ => IndexKind::Other,
        },
        "user_shell" => IndexKind::UserShell,
        "turn_end" => IndexKind::TurnEnd,
        "turn_failed" => IndexKind::TurnFailed,
        "turn_cancelled" => IndexKind::TurnCancelled,
        "compaction" => IndexKind::Compaction,
        "native_tool" => IndexKind::NativeTool,
        "cursor" => IndexKind::Cursor,
        "job_started" | "job_finished" => IndexKind::JobLifecycle,
        "backend_usage" => IndexKind::BackendUsage,
        "subagent_usage" => IndexKind::SubagentUsage,
        "run_settled" => IndexKind::RunSettled,
        _ => IndexKind::Other,
    }
}

/// Classify a materialized (in-memory) event into its [`IndexKind`]. This is
/// the in-memory analogue of [`index_kind`]: the file index walks serialized
/// `type`/`role` strings, while a session-view index derived from memory walks
/// the typed [`SessionEventKind`]. Routing both through one classifier keeps
/// the transcript and memory representations on the same lineage walk.
#[must_use]
pub(super) fn index_kind_for_event(kind: &SessionEventKind) -> IndexKind {
    match kind {
        SessionEventKind::Message(message) => match message.role {
            lofi_types::Role::User => IndexKind::UserPrompt,
            lofi_types::Role::Assistant => IndexKind::AssistantMessage,
            lofi_types::Role::Tool => IndexKind::ToolResult,
            lofi_types::Role::System => IndexKind::SystemMessage,
        },
        SessionEventKind::UserShell { .. } => IndexKind::UserShell,
        SessionEventKind::TurnEnd { .. } => IndexKind::TurnEnd,
        SessionEventKind::TurnFailed { .. } => IndexKind::TurnFailed,
        SessionEventKind::TurnCancelled { .. } => IndexKind::TurnCancelled,
        SessionEventKind::Compaction { .. } => IndexKind::Compaction,
        SessionEventKind::NativeTool(_) => IndexKind::NativeTool,
        SessionEventKind::BackendUsage { .. } => IndexKind::BackendUsage,
        SessionEventKind::SubagentUsage { .. } => IndexKind::SubagentUsage,
        SessionEventKind::RunSettled { .. } => IndexKind::RunSettled,
        SessionEventKind::JobStarted { .. } | SessionEventKind::JobFinished { .. } => {
            IndexKind::JobLifecycle
        }
        _ => IndexKind::Other,
    }
}
