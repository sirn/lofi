//! Pure-data shapes for session-history recall, shared between
//! `lofi-code` (which binds the `lofi.recall` native tool and declares the
//! `RecallFn` callback type) and `lofi-core` (which owns the engine and
//! supplies the implementation that reads the transcript).

/// Which part of the transcript a recall covers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RecallScope {
    /// The active lineage only (root -> current leaf). Default.
    #[default]
    Lineage,
    /// Every message event in the file, including off-lineage branches.
    All,
    /// The summarized range of one compaction on the active path. The index
    /// is 0-based in active-path order; `Latest` targets the most recent.
    Compaction(CompactionTarget),
}

/// A compaction to scope recall to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactionTarget {
    /// 0-based index into the active path's compaction markers.
    Index(usize),
    /// The most recent compaction on the active path.
    Latest,
}

/// A recall request — the union of the `/recall` command's args and the
/// `lofi.recall` tool's parameters.
#[derive(Debug, Clone, Default)]
pub struct RecallRequest {
    /// Search terms or a regex pattern. `None`/empty = browse mode (recent
    /// entries, flat).
    pub query: Option<String>,
    pub scope: RecallScope,
    /// 1-based results page (search mode only).
    pub page: usize,
    /// Global message indices to render at full (unclipped) content. Works
    /// alone or alongside `query` (expands matches on the current page).
    pub expand: Vec<usize>,
}

/// The outcome of a recall — the rendered text plus a short status for the
/// caller (shown in the TUI's notify line, or appended to the tool result).
#[derive(Debug, Clone)]
pub struct RecallOutcome {
    pub text: String,
    pub status: String,
}
