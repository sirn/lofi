//! JSONL transcript store for interactive sessions.
//!
//! Files live at '<state>/sessions/<cwd-slug>/<ms>_<id>.jsonl' and are
//! append-only after the header line. Sessions are scoped to the working
//! directory they were created
//! in.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lofi_error::{Error, Result};
use lofi_types::{Message, RunModel, SessionEvent, SessionEventKind};
use serde::{Deserialize, Serialize};

mod index;
use index::{
    compaction_index_suffix, load_collapsed_events_at, load_compaction_path, load_event_at,
    load_event_by_id, load_event_range, load_events_at, load_index, load_index_range,
    load_indexed_path, visit_event_values, visit_events,
};
pub use index::{EventIndex, IndexId, IndexKind};

/// Transcript format version. Bumped only on a breaking on-disk change;
/// older files are rejected (no migration yet — lofi has no shipped sessions
/// to migrate).
pub const SESSION_VERSION: u32 = 3;

/// The lowest version the store can still load (older files are migrated
/// in-memory to [`SESSION_VERSION`]).
pub const SESSION_MIN_VERSION: u32 = 1;

/// Metadata written as the first JSONL line of every session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    /// Unix milliseconds at creation time.
    pub created: u64,
    /// Absolute working directory the session belongs to.
    pub cwd: String,
    /// Raw model identity active when the session started (rendered to
    /// `provider/id:level` only at display; deserializes from a legacy
    /// `provider/id[:level]` string too).
    pub model: RunModel,
}

/// The first-line wrapper. `type: "meta"` distinguishes it from message lines
/// if a future format ever interleaves other entry kinds.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    meta: SessionMeta,
}

/// A session file discovered using directory metadata only. This is cheap
/// enough for an interactive picker to sort and draw before transcript
/// metadata is indexed.
#[derive(Debug, Clone)]
pub struct SessionFile {
    path: PathBuf,
    last_active: std::time::SystemTime,
}

impl SessionFile {
    /// Wall-clock time of the file's last modification.
    #[must_use]
    pub fn last_active(&self) -> std::time::SystemTime {
        self.last_active
    }

    /// Read a bounded provisional preview without exposing the transcript path.
    #[must_use]
    pub fn quick_preview(&self) -> Option<String> {
        quick_entry_preview(&self.path)
    }

    /// Enrich this discovered file with selected-lineage metadata.
    #[must_use]
    pub fn inspect(&self) -> Option<SessionEntry> {
        parse_entry(&self.path, self.last_active)
    }

    /// Open this transcript and return its selected-lineage snapshot in the
    /// same index pass.
    ///
    /// # Errors
    /// Propagates transcript indexing and validation failures.
    pub fn open_snapshot(&self) -> Result<(SessionCursor, SessionSnapshot)> {
        SessionCursor::open_snapshot(self.path.clone())
    }
}

/// A discoverable session on disk: its metadata, file path, and message count.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub meta: SessionMeta,
    file: SessionFile,
    /// Number of message lines (excluding the header).
    pub message_count: usize,
    /// Wall-clock time of the file's last modification (last activity).
    pub last_active: std::time::SystemTime,
    /// One-line preview of the last meaningful event (user prompt,
    /// assistant text, tool result, or compaction marker).
    pub last_message: String,
}

impl SessionEntry {
    /// The session id — the file stem (`<ms>_<id>`), used by `--resume <id>`.
    #[must_use]
    pub fn id(&self) -> String {
        self.file
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// Open this session and return its selected-lineage snapshot in one pass.
    ///
    /// # Errors
    /// Propagates transcript indexing and validation failures.
    pub fn open_snapshot(&self) -> Result<(SessionCursor, SessionSnapshot)> {
        self.file.open_snapshot()
    }
}

/// A filesystem-safe slug for `cwd` (path separators -> `-`, leading `-`
/// trimmed). Absolute paths collapse to a stable single-segment directory name.
fn slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_string()
}

/// Wall-clock milliseconds since the Unix epoch; 0 if the clock is before it.
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// Generate an opaque event/session identifier.
fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Stable in-memory ID for a legacy event that had no persisted tree ID.
/// Byte offsets are unique within one append-only transcript and remain
/// identical across full-load and lightweight-index scans.
#[cfg(test)]
fn legacy_event_id(offset: u64) -> String {
    format!("legacy-{offset:016x}")
}

/// One consistent read snapshot of a cursor's selected lineage.
///
/// The index is already projected root-to-leaf. Callers never need to infer a
/// head from physical JSONL order, and all cursor-sensitive resume/replay code
/// consumes this shape.
#[derive(Debug)]
pub struct SessionSnapshot {
    pub meta: SessionMeta,
    pub index: Vec<EventIndex>,
    pub file_size: u64,
}

/// A full-tree snapshot for branch selection. Unlike a lineage snapshot,
/// this retains sibling nodes, but its selected head still comes exclusively
/// from the cursor rather than physical file order.
#[derive(Debug)]
pub struct SessionTreeSnapshot {
    pub meta: SessionMeta,
    pub index: Vec<EventIndex>,
    pub leaf_id: Option<String>,
    pub file_size: u64,
}

/// A shared logical cursor for one append-only transcript.
///
/// Clones share the active leaf. Every durable append holds the cursor lock,
/// writes one file-locked batch, persists the selected head, and advances the
/// in-memory leaf before releasing it. Reads snapshot that same leaf. Physical
/// EOF is consulted only once when opening a legacy transcript that has no
/// durable cursor head yet.
#[derive(Debug, Clone)]
pub struct SessionCursor {
    path: PathBuf,
    leaf_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Active-lineage suffix from the latest compaction checkpoint onward.
    /// Shared by cursor clones and extended from known append byte ranges, so
    /// ordinary compaction never indexes the complete append-only transcript.
    compaction_index: std::sync::Arc<std::sync::Mutex<Option<Vec<EventIndex>>>>,
}

impl SessionCursor {
    /// Construct a cursor at a known logical leaf for a newly created
    /// transcript. Existing transcripts must be opened with `open` so their
    /// durable selected head is restored.
    #[must_use]
    pub fn new(path: PathBuf, leaf_id: Option<String>) -> Self {
        Self {
            path,
            leaf_id: std::sync::Arc::new(std::sync::Mutex::new(
                leaf_id.filter(|id| !id.is_empty()),
            )),
            compaction_index: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// Open an existing transcript at its durable selected head. Legacy files
    /// without a cursor record fall back once to their physically last event.
    ///
    /// # Errors
    /// Returns an error when the transcript cannot be indexed or its selected
    /// head no longer exists.
    pub fn open(path: PathBuf) -> Result<Self> {
        Self::open_snapshot(path).map(|(cursor, _snapshot)| cursor)
    }

    /// Open an existing transcript and return its selected-lineage snapshot
    /// from the same index scan. Resume callers need both values; keeping this
    /// operation together avoids scanning a large append-only transcript twice
    /// and avoids a second allocator high-water mark during startup.
    ///
    /// # Errors
    /// Returns an error when the transcript cannot be indexed or its selected
    /// head no longer exists.
    pub fn open_snapshot(path: PathBuf) -> Result<(Self, SessionSnapshot)> {
        let (meta, index, file_size) = load_index(&path)?;
        let cursor_record = index
            .iter()
            .rev()
            .find(|event| event.kind == IndexKind::Cursor);
        let leaf = match cursor_record {
            Some(event) => event.cursor_leaf.as_ref().map(IndexId::to_event_id),
            None => index
                .iter()
                .rev()
                .find(|event| event.kind != IndexKind::Cursor)
                .map(|event| event.id.to_event_id()),
        };
        let selected = indexed_lineage(index, leaf.as_deref())?;
        let cursor = Self::new(path, leaf);
        *cursor.lock_compaction_index() = Some(compaction_index_suffix(&cursor.path, &selected)?);
        Ok((
            cursor,
            SessionSnapshot {
                meta,
                index: selected,
                file_size,
            },
        ))
    }

    /// Transcript path owned by this cursor.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current transcript length in bytes. Keeping metadata access on the
    /// cursor lets callers remain independent of the storage backend.
    #[must_use]
    pub fn len(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |metadata| metadata.len())
    }

    /// Whether the transcript currently contains no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Stable session id derived from the store-owned transcript name.
    #[must_use]
    pub fn id(&self) -> String {
        self.path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// Snapshot the current logical leaf.
    #[must_use]
    pub fn leaf_id(&self) -> Option<String> {
        self.lock_leaf().clone()
    }

    /// Move the cursor to an explicit branch point selected by the user and
    /// persist that selection before returning.
    ///
    /// # Errors
    /// Returns an error when the selected event is absent or the cursor record
    /// cannot be written durably.
    pub fn branch_from(&self, id: String) -> Result<()> {
        let next = (!id.is_empty()).then_some(id);
        let mut leaf = self.lock_leaf();
        persist_cursor(&self.path, next.as_deref(), true)?;
        *leaf = next;
        // The old suffix belongs to a different selected lineage. The next
        // snapshot reseeds this with only the new branch's compactable tail.
        *self.lock_compaction_index() = None;
        Ok(())
    }

    /// Atomically index the transcript and project it onto this cursor's
    /// selected lineage.
    ///
    /// # Errors
    /// Returns an error when the transcript cannot be indexed or the selected
    /// head is absent/cyclic.
    pub fn snapshot(&self) -> Result<SessionSnapshot> {
        let leaf = self.lock_leaf();
        let (meta, index, file_size) = load_index(&self.path)?;
        let index = indexed_lineage(index, leaf.as_deref())?;
        *self.lock_compaction_index() = Some(compaction_index_suffix(&self.path, &index)?);
        Ok(SessionSnapshot {
            meta,
            index,
            file_size,
        })
    }

    /// Atomically index the full transcript tree together with this cursor's
    /// selected head. Intended only for branch/tree UI.
    ///
    /// # Errors
    /// Returns an error when indexing fails or the selected head is absent.
    pub fn tree_snapshot(&self) -> Result<SessionTreeSnapshot> {
        let leaf = self.lock_leaf();
        let (meta, index, file_size) = load_index(&self.path)?;
        lineage_indices(&index, leaf.as_deref())?;
        Ok(SessionTreeSnapshot {
            meta,
            index,
            leaf_id: leaf.clone(),
            file_size,
        })
    }

    /// Read one event at a byte offset obtained from this cursor's snapshot.
    ///
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn event_at(&self, offset: u64) -> Result<SessionEvent> {
        load_event_at(&self.path, offset)
    }

    /// Read events for collapsed transcript display while skipping successful
    /// result bodies that are not rendered in that mode.
    ///
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn collapsed_events_at(&self, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
        load_collapsed_events_at(&self.path, offsets)
    }

    /// Read events at byte offsets obtained from this cursor's snapshot.
    ///
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn events_at(&self, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
        load_events_at(&self.path, offsets)
    }

    /// Read an event by its durable ID from this transcript.
    ///
    /// This intentionally searches the complete append-only tree, not only the
    /// selected lineage: result recovery may target compacted or abandoned
    /// content by an immutable transcript ID.
    ///
    /// # Errors
    /// Propagates transcript indexing and event parsing failures.
    pub fn event_by_id(&self, id: &str) -> Result<Option<SessionEvent>> {
        load_event_by_id(&self.path, id)
    }

    /// Visit complete events at snapshot-derived offsets one at a time. Unlike
    /// `events_at`, this does not collect the selected payloads in memory.
    ///
    /// # Errors
    /// Propagates transcript seek/read/parse failures and callback errors.
    pub fn visit_events(
        &self,
        offsets: &[u64],
        visit: impl FnMut(SessionEvent) -> Result<()>,
    ) -> Result<()> {
        visit_events(&self.path, offsets, visit)
    }

    /// Deserialize a small projection at snapshot-derived event offsets
    /// without exposing the transcript path or allocating skipped payload
    /// fields. Used by picker/recall metadata scans.
    ///
    /// # Errors
    /// Propagates transcript seek/read/parse failures and callback errors.
    pub fn visit_event_values<T: serde::de::DeserializeOwned>(
        &self,
        offsets: &[u64],
        visit: impl FnMut(T) -> Result<()>,
    ) -> Result<()> {
        visit_event_values(&self.path, offsets, visit)
    }

    /// Read only the display metadata for native calls at snapshot-derived
    /// offsets. The native result field is skipped by serde's streaming
    /// deserializer, so a large tool result is neither allocated nor retained
    /// merely to render an exec summary.
    ///
    /// # Errors
    /// Propagates transcript seek/read/parse failures.
    pub fn native_tool_summaries(&self, offsets: &[u64]) -> Result<Vec<(String, String, String)>> {
        #[derive(Deserialize)]
        struct NativeToolSummary {
            parent: String,
            name: String,
            args: String,
        }

        let mut summaries = Vec::with_capacity(offsets.len());
        visit_event_values::<NativeToolSummary>(&self.path, offsets, |record| {
            summaries.push((record.parent, record.name, record.args));
            Ok(())
        })?;
        Ok(summaries)
    }

    /// Read only the first text block from user-message events. Assistant and
    /// tool payloads in the same record shape are skipped without allocation;
    /// startup replay uses this to build file-backed historical turn shells.
    ///
    /// # Errors
    /// Propagates transcript seek/read/parse failures.
    pub fn prompt_texts(&self, offsets: &[u64]) -> Result<Vec<String>> {
        #[derive(Deserialize)]
        struct PromptProjection {
            #[serde(default)]
            blocks: Vec<PromptBlockProjection>,
        }
        #[derive(Deserialize)]
        struct PromptBlockProjection {
            #[serde(default, rename = "type")]
            kind: String,
            #[serde(default)]
            text: String,
        }

        let mut prompts = Vec::with_capacity(offsets.len());
        visit_event_values::<PromptProjection>(&self.path, offsets, |event| {
            prompts.push(
                event
                    .blocks
                    .into_iter()
                    .find_map(|block| (block.kind == "text").then_some(block.text))
                    .unwrap_or_default(),
            );
            Ok(())
        })?;
        Ok(prompts)
    }

    /// Read all non-cursor events physically contained in one committed byte
    /// range. The range is a durable handle recorded by this same cursor.
    ///
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn events_in_range(&self, start: u64, end: u64) -> Result<Vec<SessionEvent>> {
        load_event_range(&self.path, start, end)
    }

    /// Materialize every non-cursor event in the append-only transcript tree.
    ///
    /// This is intentionally distinct from `load_events`, which only returns
    /// the selected lineage. Full-tree consumers still read through the cursor
    /// so path access and legacy ID migration cannot diverge from the rest of
    /// the session API.
    ///
    /// # Errors
    /// Propagates indexing and event parsing failures.
    pub fn load_tree_events(&self) -> Result<Vec<SessionEvent>> {
        let _leaf = self.lock_leaf();
        let (_meta, index, _size) = load_index(&self.path)?;
        let selected: Vec<&EventIndex> = index
            .iter()
            .filter(|event| event.kind != IndexKind::Cursor)
            .collect();
        let offsets: Vec<u64> = selected.iter().map(|event| event.offset).collect();
        let mut events = load_events_at(&self.path, &offsets)?;
        for (event, entry) in events.iter_mut().zip(selected) {
            event.id = entry.id.to_event_id();
            event.parent_id = entry.parent_id.as_ref().map(IndexId::to_event_id);
        }
        Ok(events)
    }

    /// Materialize this cursor's selected event lineage.
    ///
    /// # Errors
    /// Propagates indexing and event parsing failures.
    pub fn load_events(&self) -> Result<Vec<SessionEvent>> {
        let leaf = self.lock_leaf();
        let (_meta, index, _size) = load_index(&self.path)?;
        load_indexed_path(&self.path, &index, leaf.as_deref())
    }

    /// Materialize only the selected lineage suffix needed by compaction.
    ///
    /// # Errors
    /// Propagates indexing and event parsing failures.
    pub fn load_compaction_events(&self) -> Result<Vec<SessionEvent>> {
        let leaf = self.lock_leaf();
        if let Some(index) = self.lock_compaction_index().as_ref() {
            return load_compaction_path(&self.path, index, leaf.as_deref());
        }
        // Compatibility fallback for cursors manually constructed around an
        // existing file. Production open/resume paths seed the suffix once.
        let (_meta, index, _size) = load_index(&self.path)?;
        let index = indexed_lineage(index, leaf.as_deref())?;
        let suffix = compaction_index_suffix(&self.path, &index)?;
        let events = load_compaction_path(&self.path, &suffix, leaf.as_deref())?;
        *self.lock_compaction_index() = Some(suffix);
        Ok(events)
    }

    /// Append one event batch to this cursor's lineage and advance its leaf.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn append_events(&self, events: &mut [SessionEvent]) -> Result<(u64, u64)> {
        let mut leaf = self.lock_leaf();
        let (start, end, next) = append_cursor_events(&self.path, events, leaf.as_deref())?;
        let appended = load_index_range(&self.path, start, end).ok();
        *leaf = next;
        let mut index = self.lock_compaction_index();
        match (index.as_mut(), appended) {
            (Some(index), Some(appended)) => index.extend(appended),
            (_, None) => *index = None,
            (None, Some(_)) => {}
        }
        Ok((start, end))
    }

    /// Append a complete compaction checkpoint and advance to its marker.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn append_compaction(
        &self,
        kept_messages: &[Message],
        summary: &str,
        summarized_range: &[String; 2],
        counts: CompactionCounts,
    ) -> Result<(u64, u64)> {
        let mut leaf = self.lock_leaf();
        let (start, end, marker_id) = append_compaction_from(
            &self.path,
            kept_messages,
            AppendParent::Explicit(leaf.as_deref()),
            summary,
            summarized_range,
            counts,
            true,
        )?;
        let suffix = load_index_range(&self.path, start, end).ok();
        *leaf = Some(marker_id);
        *self.lock_compaction_index() = suffix;
        Ok((start, end))
    }

    fn lock_leaf(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.leaf_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_compaction_index(&self) -> std::sync::MutexGuard<'_, Option<Vec<EventIndex>>> {
        self.compaction_index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn lineage_indices(index: &[EventIndex], leaf_id: Option<&str>) -> Result<Vec<usize>> {
    use std::collections::HashMap;
    let by_id: HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind != IndexKind::Cursor)
        .map(|(i, event)| (&event.id, i))
        .collect();
    let leaf = leaf_id.map(|id| IndexId::parse(id.to_string()));
    let mut current = leaf.as_ref().and_then(|id| by_id.get(id).copied());
    let mut selected = Vec::new();
    while let Some(i) = current {
        selected.push(i);
        if selected.len() > index.len() {
            return Err(Error::State("cycle in session event lineage".to_string()));
        }
        current = index[i]
            .parent_id
            .as_ref()
            .and_then(|id| by_id.get(id).copied());
    }
    if leaf_id.is_some() && selected.is_empty() {
        return Err(Error::State("session branch leaf not found".to_string()));
    }
    selected.reverse();
    Ok(selected)
}

fn indexed_lineage(index: Vec<EventIndex>, leaf_id: Option<&str>) -> Result<Vec<EventIndex>> {
    let selected = lineage_indices(&index, leaf_id)?;
    let mut wanted = selected.into_iter().peekable();
    let mut next = wanted.next();
    let mut lineage = Vec::with_capacity(wanted.size_hint().0 + usize::from(next.is_some()));
    for (i, event) in index.into_iter().enumerate() {
        if next == Some(i) {
            lineage.push(event);
            next = wanted.next();
        }
    }
    Ok(lineage)
}

fn cursor_event(leaf_id: Option<&str>) -> SessionEvent {
    SessionEvent {
        id: String::new(),
        parent_id: None,
        kind: SessionEventKind::Cursor {
            leaf_id: leaf_id.map(str::to_string),
        },
    }
}

fn persist_cursor(path: &Path, leaf_id: Option<&str>, validate: bool) -> Result<()> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    if validate
        && leaf_id.is_some_and(|id| {
            load_index(path).map_or(true, |(_, index, _)| {
                !index
                    .iter()
                    .any(|event| event.kind != IndexKind::Cursor && event.id.matches(id))
            })
        })
    {
        return Err(Error::State("session branch leaf not found".to_string()));
    }
    append_prepared_events(path, &[cursor_event(leaf_id)])?;
    Ok(())
}

fn append_cursor_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent: Option<&str>,
) -> Result<(u64, u64, Option<String>)> {
    if events.is_empty() {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        return Ok((len, len, parent.map(str::to_string)));
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    let mut next = parent.map(str::to_string);
    for event in events.iter_mut() {
        event.id = short_id();
        if event.parent_id.is_none() {
            event.parent_id.clone_from(&next);
        }
        next = Some(event.id.clone());
    }
    let byte_start = std::fs::metadata(path).map_or(0, |m| m.len());
    let head = cursor_event(next.as_deref());
    match append_events_with_cursor(path, events, &head) {
        Ok((_, end)) => Ok((byte_start, end, next)),
        Err(error) => {
            let _ = std::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|file| file.set_len(byte_start));
            Err(error)
        }
    }
}

/// Owns the sessions root directory and scopes all per-cwd listings/creates
/// under it. Carrying the root explicitly (rather than re-resolving the state
/// dir via an env var on every call) keeps tests parallel-safe and lets the
/// TUI construct one store up front.
#[derive(Debug, Clone)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    /// Open the default store at `<state>/sessions` (see [`crate::state`]).
    ///
    /// # Errors
    /// Propagates [`crate::state::state_dir`] if the base state dir cannot be
    /// resolved.
    pub fn open() -> Result<Self> {
        let mut p = crate::state::state_dir()?;
        p.push("sessions");
        Ok(Self { root: p })
    }

    /// Construct a store at an explicit sessions root (tests / custom layouts).
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir_for_cwd(&self, cwd: &Path) -> PathBuf {
        self.root.join(slug(cwd))
    }

    /// Create a new session and return its shared logical cursor at the root.
    ///
    /// The directory is created if needed; the header is written atomically via
    /// a temp file + rename so a partial file is never visible. Returning the
    /// cursor directly prevents active callers from constructing independent
    /// head state around the same path.
    ///
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure or [`Error::State`] on a
    /// header-serialization failure.
    pub fn create_cursor(&self, cwd: &Path, model: &RunModel) -> Result<SessionCursor> {
        let path = self.create(cwd, model)?;
        persist_cursor(&path, None, false)?;
        let cursor = SessionCursor::new(path, None);
        // This transcript is known to have no conversation events yet. Start
        // the suffix index empty so even its first compaction never needs a
        // complete-file index scan.
        *cursor.lock_compaction_index() = Some(Vec::new());
        Ok(cursor)
    }

    /// Low-level path-returning creation helper for store tests. Active
    /// session code must use [`Self::create_cursor`].
    fn create(&self, cwd: &Path, model: &RunModel) -> Result<PathBuf> {
        let dir = self.dir_for_cwd(cwd);
        std::fs::create_dir_all(&dir)?;
        let file_name = format!("{}_{}.jsonl", now_ms(), short_id());
        let path = dir.join(file_name);
        let header = Header {
            kind: "meta".to_string(),
            meta: SessionMeta {
                version: SESSION_VERSION,
                created: now_ms(),
                cwd: cwd.to_string_lossy().into_owned(),
                model: model.clone(),
            },
        };
        let line =
            serde_json::to_string(&header).map_err(|e| Error::State(format!("json: {e}")))?;
        write_atomic(&path, &format!("{line}\n"))?;
        Ok(path)
    }

    /// List sessions for `cwd`, newest-first (by file stem's leading timestamp).
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the per-cwd directory cannot be read for a reason
    /// other than not existing.
    pub fn list_for_cwd(&self, cwd: &Path) -> Result<Vec<SessionEntry>> {
        let mut entries = Vec::new();
        for file in self.list_files_for_cwd(cwd)? {
            if let Some(entry) = file.inspect() {
                entries.push(entry);
            }
        }
        Ok(entries)
    }

    /// Discover session files using directory metadata only, newest first.
    /// No transcript contents are read or indexed.
    ///
    /// # Errors
    /// Returns [`Error::Io`] if the per-cwd directory cannot be read for a reason
    /// other than not existing.
    pub fn list_files_for_cwd(&self, cwd: &Path) -> Result<Vec<SessionFile>> {
        let dir = self.dir_for_cwd(cwd);
        let mut files = Vec::new();
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::Io(e)),
        };
        for ent in read {
            let ent = ent?;
            let path = ent.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            let last_active = ent
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.push(SessionFile { path, last_active });
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.last_active));
        Ok(files)
    }

    /// The most recent session for `cwd`, or `None` if none exist.
    ///
    /// # Errors
    /// Propagates [`list_for_cwd`](Self::list_for_cwd).
    pub fn most_recent(&self, cwd: &Path) -> Result<Option<SessionEntry>> {
        Ok(self.list_for_cwd(cwd)?.into_iter().next())
    }

    /// Find a session whose id starts with `prefix` (case-sensitive).
    ///
    /// # Errors
    /// Returns [`Error::State`] if `prefix` matches more than one session.
    pub fn find(&self, cwd: &Path, prefix: &str) -> Result<Option<SessionEntry>> {
        let matches: Vec<_> = self
            .list_for_cwd(cwd)?
            .into_iter()
            .filter(|e| e.id().starts_with(prefix))
            .collect();
        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => Err(Error::State(format!("ambiguous session id '{prefix}'"))),
        }
    }
}

/// Load a session file: its metadata and the full event log.
///
/// # Errors
/// Returns [`Error::State`] if the file is missing a header, has an
/// unsupported version, or an event line fails to parse; [`Error::Io`] on a
/// read failure.
#[cfg(test)]
fn load(path: &Path) -> Result<(SessionMeta, Vec<SessionEvent>, Vec<u64>, u64)> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut buf = String::new();
    let mut pos: u64 = 0;
    // The first non-empty line is the session header.
    let header_line = loop {
        buf.clear();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            return Err(Error::State(format!(
                "session file has no header: {}",
                path.display()
            )));
        }
        pos += n as u64;
        let line = buf.trim_end_matches(['\n', '\r']);
        if !line.is_empty() {
            break line.to_string();
        }
    };
    let header: Header =
        serde_json::from_str(&header_line).map_err(|e| Error::State(format!("json: {e}")))?;
    if !(SESSION_MIN_VERSION..=SESSION_VERSION).contains(&header.meta.version) {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let legacy_v1 = header.meta.version == 1;
    let mut events = Vec::new();
    // Byte offset of each event's line in the file (parallel to `events`).
    let mut offsets = Vec::new();
    let mut i = 0usize;
    // V1 had no event tree, so migrate it to one linear chain. V2
    // `parent_id: null` is a meaningful root branch and must be preserved.
    let mut prev_id: Option<String> = None;
    loop {
        buf.clear();
        let line_start = pos;
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            break;
        }
        pos += n as u64;
        let line = buf.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let mut ev = parse_event(line).map_err(|e| {
            Error::State(format!("parse event {} in {}: {e}", i + 1, path.display()))
        })?;
        if matches!(ev.kind, SessionEventKind::Cursor { .. }) {
            continue;
        }
        let migrated = legacy_v1 || ev.id.is_empty();
        if migrated {
            ev.id = legacy_event_id(line_start);
            ev.parent_id.clone_from(&prev_id);
        }
        prev_id = Some(ev.id.clone());
        events.push(ev);
        offsets.push(line_start);
        i += 1;
    }
    Ok((header.meta, events, offsets, pos))
}

/// Internal parent selection for a durable append.
///
/// Active session cursors always use `Explicit`, where `None` means root.
/// The physical-EOF mode is retained only for low-level legacy callers.
enum AppendParent<'a> {
    Explicit(Option<&'a str>),
    #[cfg(test)]
    PhysicalEof,
}

impl AppendParent<'_> {
    #[allow(unused_variables)]
    fn resolve(self, path: &Path) -> Result<Option<String>> {
        match self {
            Self::Explicit(parent) => Ok(parent.map(str::to_string)),
            #[cfg(test)]
            Self::PhysicalEof => last_event_id(path),
        }
    }
}

/// Append session events to a transcript file (one JSON line each). The
/// file is synchronized before returning so a crash after the turn still has
/// the data.
///
/// Each event is stamped with a fresh `id` (any incoming `id` is
/// overwritten) and chained to the previous one. A supplied `parent_hint`
/// selects an explicit branch parent; otherwise this low-level compatibility
/// helper continues from physical EOF. Active apps use [`SessionCursor`],
/// which always supplies its explicit logical parent instead.
///
/// # Errors
/// Returns [`Error::Io`] on open/write failure or [`Error::State`] on a
/// serialization failure.
#[cfg(test)]
pub(crate) fn append_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent_hint: Option<&str>,
) -> Result<(u64, u64)> {
    let parent = parent_hint.map_or(AppendParent::PhysicalEof, |id| {
        AppendParent::Explicit(Some(id))
    });
    append_events_from(path, events, parent)
}

#[cfg(test)]
fn append_events_from(
    path: &Path,
    events: &mut [SessionEvent],
    parent: AppendParent<'_>,
) -> Result<(u64, u64)> {
    if events.is_empty() {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        return Ok((len, len));
    }
    // Serialize parent resolution and the complete batch append across
    // processes. Logical branches may share a file, but their JSONL records
    // must never physically interleave.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    let mut parent = parent.resolve(path)?;
    // Assign a fresh id to each event. `parent_id` is auto-filled only when
    // the event did not set one explicitly — the recorder uses an explicit
    // `parent_id` on a `TurnFailed` marker to branch it off the turn's
    // checkpoint instead of off the preceding message.
    for ev in events.iter_mut() {
        ev.id = short_id();
        if ev.parent_id.is_none() {
            ev.parent_id.clone_from(&parent);
        }
        parent = Some(ev.id.clone());
    }
    append_prepared_events(path, events)
}

/// Message-count bookkeeping persisted with a compaction checkpoint.
#[derive(Debug, Clone, Copy)]
pub struct CompactionCounts {
    /// Messages folded by this compaction, used by the visible marker.
    pub summarized: usize,
    /// Original messages represented by the merged summary across compactions.
    pub represented: usize,
    /// Messages retained verbatim in the tail.
    pub kept: usize,
}

/// Append a compaction checkpoint as one batch: edited kept-tail messages
/// followed by the marker. A failed write is rolled back to the original file
/// length so the transcript cannot expose a partial checkpoint as its leaf.
///
/// # Errors
/// Propagates transcript read, serialization, and write failures.
#[cfg(test)]
pub(crate) fn append_compaction(
    path: &Path,
    kept_messages: &[Message],
    parent_hint: Option<&str>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: CompactionCounts,
) -> Result<(u64, u64, String)> {
    let parent = parent_hint.map_or(AppendParent::PhysicalEof, |id| {
        AppendParent::Explicit(Some(id))
    });
    append_compaction_from(
        path,
        kept_messages,
        parent,
        summary,
        summarized_range,
        counts,
        false,
    )
}

fn append_compaction_from(
    path: &Path,
    kept_messages: &[Message],
    parent: AppendParent<'_>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: CompactionCounts,
    persist_head: bool,
) -> Result<(u64, u64, String)> {
    #[derive(Serialize)]
    struct MessageCheckpoint<'a> {
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<&'a str>,
        #[serde(rename = "type")]
        kind: &'static str,
        role: lofi_types::Role,
        blocks: &'a [lofi_types::ContentBlock],
    }
    #[derive(Serialize)]
    struct CompactionCheckpoint<'a> {
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_id: Option<&'a str>,
        #[serde(rename = "type")]
        kind: &'static str,
        summary: &'a str,
        first_kept_entry_id: &'a str,
        summarized_range: &'a [String; 2],
        checkpointed_tail: bool,
        summarized: usize,
        represented: usize,
        kept: usize,
    }

    // Keep the checkpoint batch contiguous with respect to every other lofi
    // writer. This also makes the EOF fallback and append one transaction.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    let parent = parent.resolve(path)?;
    let ids: Vec<String> = (0..=kept_messages.len()).map(|_| short_id()).collect();
    let byte_start = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    let write = (|| -> Result<()> {
        for (index, message) in kept_messages.iter().enumerate() {
            let event = MessageCheckpoint {
                id: &ids[index],
                parent_id: if index == 0 {
                    parent.as_deref()
                } else {
                    Some(&ids[index - 1])
                },
                kind: "message",
                role: message.role,
                blocks: &message.blocks,
            };
            serde_json::to_writer(&mut file, &event)
                .map_err(|e| Error::State(format!("json: {e}")))?;
            file.write_all(b"\n")?;
        }
        let marker_index = kept_messages.len();
        let marker = CompactionCheckpoint {
            id: &ids[marker_index],
            parent_id: if marker_index == 0 {
                parent.as_deref()
            } else {
                Some(&ids[marker_index - 1])
            },
            kind: "compaction",
            summary,
            first_kept_entry_id: ids
                .first()
                .filter(|_| marker_index > 0)
                .map_or("", String::as_str),
            summarized_range,
            checkpointed_tail: true,
            summarized: counts.summarized,
            represented: counts.represented,
            kept: counts.kept,
        };
        serde_json::to_writer(&mut file, &marker)
            .map_err(|e| Error::State(format!("json: {e}")))?;
        file.write_all(b"\n")?;
        if persist_head {
            serde_json::to_writer(&mut file, &cursor_event(Some(&ids[marker_index])))
                .map_err(|e| Error::State(format!("json: {e}")))?;
            file.write_all(b"\n")?;
        }
        file.sync_data()?;
        Ok(())
    })();
    if let Err(error) = write {
        let _ = file.set_len(byte_start);
        return Err(error);
    }
    let byte_end = file.metadata()?.len();
    Ok((byte_start, byte_end, ids[kept_messages.len()].clone()))
}

fn append_events_with_cursor(
    path: &Path,
    events: &[SessionEvent],
    cursor: &SessionEvent,
) -> Result<(u64, u64)> {
    let byte_start = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    for event in events.iter().chain(std::iter::once(cursor)) {
        if let Err(error) = serde_json::to_writer(&mut file, event) {
            let _ = file.set_len(byte_start);
            return Err(Error::State(format!("json: {error}")));
        }
        if let Err(error) = file.write_all(
            b"
",
        ) {
            let _ = file.set_len(byte_start);
            return Err(Error::Io(error));
        }
    }
    if let Err(error) = file.sync_data() {
        let _ = file.set_len(byte_start);
        return Err(Error::Io(error));
    }
    Ok((byte_start, file.metadata()?.len()))
}

fn append_prepared_events(path: &Path, events: &[SessionEvent]) -> Result<(u64, u64)> {
    let byte_start = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    // Serialize directly to disk. A complete payload buffer duplicated the
    // whole checkpoint while its message bodies were already resident.
    for event in events {
        if let Err(error) = serde_json::to_writer(&mut file, event) {
            let _ = file.set_len(byte_start);
            return Err(Error::State(format!("json: {error}")));
        }
        if let Err(error) = file.write_all(b"\n") {
            let _ = file.set_len(byte_start);
            return Err(Error::Io(error));
        }
    }
    if let Err(error) = file.sync_data() {
        let _ = file.set_len(byte_start);
        return Err(Error::Io(error));
    }
    let byte_end = file.metadata()?.len();
    Ok((byte_start, byte_end))
}

/// Read the `id` of the last event line in `path`, or `None` if the file has
/// no events (only a header, or empty). Used by [`append_events`] to chain a
/// continuation onto the active leaf, and by the recorder to branch a
/// `TurnFailed` marker off the turn's checkpoint.
///
/// # Errors
/// Returns [`Error::Io`] on a read failure other than the file not existing.
#[cfg(test)]
fn last_event_id(path: &Path) -> Result<Option<String>> {
    use std::io::{BufRead, BufReader};
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let mut last: Option<String> = None;
    let mut first = true;
    for line in BufReader::new(file).lines() {
        let line = line?;
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            continue;
        }
        if first {
            // Skip the header line.
            first = false;
            continue;
        }
        if let Ok(ev) = parse_event(trimmed) {
            if !matches!(ev.kind, SessionEventKind::Cursor { .. }) {
                last = Some(ev.id);
            }
        }
    }
    Ok(last)
}

/// Parse a single transcript body line into a [`SessionEvent`].
///
/// Current files are tagged (`{"type":"message", ...}`); older files written
/// before the event-log format stored bare `Message` JSON per line. Those are
/// tolerated by falling back to `Message` and wrapping it, so legacy sessions
/// still resume (without timings/cost, as before).
///
/// # Errors
/// Returns [`Error::State`] if the line is neither a tagged event nor a
/// legacy `Message` object.
pub fn parse_event(line: &str) -> Result<SessionEvent> {
    if let Ok(ev) = serde_json::from_str::<SessionEvent>(line) {
        return Ok(ev);
    }
    // Legacy pre-event-log files stored bare `Message` JSON per line; wrap
    // it so those sessions still resume (without timings/cost, as before).
    // `id`/`parent_id` are left empty and resolved by the caller (the load
    // loop chains them into a linear v1 migration).
    serde_json::from_str::<Message>(line)
        .map(|m| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(m),
        })
        .map_err(|e| Error::State(format!("parse event: {e}")))
}

/// The id of the last event in `events` (the active leaf for a freshly
/// loaded session), or `None` if there are no events.
#[must_use]
pub fn leaf_id(events: &[SessionEvent]) -> Option<&str> {
    events.last().map(|e| e.id.as_str())
}

/// Indices of the events on the path from the leaf `leaf_id` to the root,
/// in root-first order (oldest to newest). Returns an empty `Vec` if
/// `leaf_id` is not found.
#[must_use]
pub fn active_path(events: &[SessionEvent], leaf_id: &str) -> Vec<usize> {
    let mut by_id: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (i, ev) in events.iter().enumerate() {
        if !ev.id.is_empty() {
            by_id.insert(ev.id.as_str(), i);
        }
    }
    let mut path = Vec::new();
    let mut cur = by_id.get(leaf_id).copied();
    while let Some(i) = cur {
        path.push(i);
        cur = events[i]
            .parent_id
            .as_deref()
            .and_then(|parent| by_id.get(parent).copied());
        if path.len() > events.len() {
            return Vec::new();
        }
    }
    path.reverse();
    path
}

/// Active path ending at the last event.
#[must_use]
pub fn active_path_from_leaf(events: &[SessionEvent]) -> Vec<usize> {
    leaf_id(events).map_or_else(Vec::new, |id| active_path(events, id))
}

/// Raw model identity of the last completed turn on the active path.
#[must_use]
pub fn last_run_model(events: &[SessionEvent]) -> Option<RunModel> {
    active_path_from_leaf(events)
        .into_iter()
        .rev()
        .find_map(|i| match &events[i].kind {
            SessionEventKind::TurnEnd { model, .. }
            | SessionEventKind::TurnFailed { model, .. } => Some(model.clone()),
            _ => None,
        })
}

/// Minimal owned shape used by the session picker. Unknown fields, including
/// large tool-result and native-tool result bodies, are streamed past without
/// allocating them.
#[derive(Deserialize)]
struct EntryPreview {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    role: String,
    #[serde(default)]
    blocks: Vec<EntryPreviewBlock>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    args: String,
    #[serde(default)]
    summarized: usize,
    #[serde(default)]
    kept: usize,
}

#[derive(Deserialize)]
struct EntryPreviewBlock {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

/// Truncate to one line without allocating a Vec containing every character.
fn one_line(s: &str) -> String {
    let line = s.split('\n').next().unwrap_or("");
    line.char_indices()
        .nth(80)
        .map_or_else(|| line.to_string(), |(end, _)| format!("{}…", &line[..end]))
}

fn entry_preview(ev: &EntryPreview) -> String {
    match ev.kind.as_str() {
        "message" | "" => {
            let Some(text) = ev
                .blocks
                .iter()
                .find_map(|b| (b.kind == "text" && !b.text.is_empty()).then_some(b.text.as_ref()))
            else {
                return String::new();
            };
            let prefix = match ev.role.as_str() {
                "user" => "user: ",
                "assistant" => "agent: ",
                _ => "",
            };
            format!("{prefix}{}", one_line(text))
        }
        "turn_failed" => format!("agent: {} (failed)", one_line(&ev.error)),
        "compaction" => format!(
            "compact: Compacted {} messages · kept {}",
            ev.summarized, ev.kept
        ),
        "native_tool" => format!("exec: {} {}", ev.name, one_line(&ev.args)),
        _ => String::new(),
    }
}

fn quick_entry_preview(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    // JSONL strings escape embedded newlines, so physical newlines delimit
    // records. Keep this bounded: a huge tool-result line at EOF must not make
    // merely opening the picker allocate that complete payload.
    const TAIL_BYTES: u64 = 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::with_capacity(usize::try_from(len - start).ok()?);
    file.read_to_end(&mut tail).ok()?;
    // Reverse-split directly over the bounded buffer; no Vec of every line is
    // needed. A partial record at the start simply fails projection.
    for line in tail.rsplit(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<EntryPreview>(line) else {
            continue;
        };
        let preview = entry_preview(&event);
        if !preview.is_empty() {
            return Some(preview);
        }
    }
    None
}

fn parse_entry(path: &Path, last_active: std::time::SystemTime) -> Option<SessionEntry> {
    // One index pass is enough to restore the durable head and project its
    // lineage. The old picker path opened a cursor and then snapshotted it,
    // indexing every file twice.
    let (meta, index, _file_size) = load_index(path).ok()?;
    let cursor_record = index
        .iter()
        .rev()
        .find(|event| event.kind == IndexKind::Cursor);
    let leaf = match cursor_record {
        Some(event) => event.cursor_leaf.as_ref().map(IndexId::to_event_id),
        None => index
            .iter()
            .rev()
            .find(|event| event.kind != IndexKind::Cursor)
            .map(|event| event.id.to_event_id()),
    };
    let selected = lineage_indices(&index, leaf.as_deref()).ok()?;
    let message_count = selected
        .iter()
        .filter(|&&i| {
            matches!(
                index[i].kind,
                IndexKind::UserPrompt
                    | IndexKind::AssistantMessage
                    | IndexKind::ToolResult