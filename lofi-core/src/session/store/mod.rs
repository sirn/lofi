use std::io::Write;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use lofi_error::{Error, Result};
use lofi_types::{Message, RunModel, SessionEvent, SessionEventKind};
use serde::{Deserialize, Serialize};

mod index;
use index::{
    compaction_index_suffix, index_kind_for_event, load_collapsed_events_at, load_compaction_path,
    load_event_at, load_event_by_id, load_event_range, load_events_at, load_index,
    load_index_range, load_indexed_path, visit_event_values, visit_events,
};
pub use index::{EventIndex, IndexId, IndexKind};

/// Pre-release: there is one transcript format and it is versioned 1.
pub const SESSION_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    pub created: u64,
    pub cwd: String,
    pub model: RunModel,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Header {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    meta: SessionMeta,
}

#[derive(Debug, Clone)]
pub struct SessionFile {
    path: PathBuf,
    last_active: std::time::SystemTime,
}

impl SessionFile {
    #[must_use]
    pub fn last_active(&self) -> std::time::SystemTime {
        self.last_active
    }

    #[must_use]
    pub fn quick_preview(&self) -> Option<String> {
        quick_entry_preview(&self.path)
    }

    #[must_use]
    pub fn inspect(&self) -> Option<SessionEntry> {
        parse_entry(&self.path, self.last_active)
    }

    /// # Errors
    /// Propagates transcript indexing and validation failures.
    pub fn open_snapshot(&self) -> Result<(SessionCursor, SessionSnapshot)> {
        SessionCursor::open_snapshot(self.path.clone())
    }
}

#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub meta: SessionMeta,
    file: SessionFile,
    pub message_count: usize,
    pub last_active: std::time::SystemTime,
    pub last_message: String,
}

impl SessionEntry {
    #[must_use]
    pub fn id(&self) -> String {
        self.file
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    /// # Errors
    /// Propagates transcript indexing and validation failures.
    pub fn open_snapshot(&self) -> Result<(SessionCursor, SessionSnapshot)> {
        self.file.open_snapshot()
    }
}

fn metadata_matches_cwd(meta: &SessionMeta, cwd: &Path) -> bool {
    let stored = Path::new(&meta.cwd);
    stored == cwd
        || stored
            .canonicalize()
            .ok()
            .zip(cwd.canonicalize().ok())
            .is_some_and(|(stored, requested)| stored == requested)
}

fn read_session_meta(path: &Path) -> Option<SessionMeta> {
    use std::io::BufRead as _;

    let file = std::fs::File::open(path).ok()?;
    let mut lines = std::io::BufReader::new(file).lines();
    let line = lines.find_map(|line| line.ok().filter(|line| !line.is_empty()))?;
    serde_json::from_str::<Header>(&line)
        .ok()
        .map(|header| header.meta)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

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

#[derive(Debug, Clone)]
pub struct SessionCursor {
    path: PathBuf,
    leaf_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
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

    /// Open an existing transcript at its durable selected head. A transcript
    /// that has never selected an explicit branch head has no cursor record and
    /// falls back once to its physically last event.
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
    /// # Errors
    /// Returns an error when the transcript cannot be indexed or its selected
    /// head no longer exists.
    pub fn open_snapshot(path: PathBuf) -> Result<(Self, SessionSnapshot)> {
        crate::state::ensure_private_file(&path)?;
        let (meta, index, file_size) = load_index(&path)?;
        let cursor_record = index
            .iter()
            .rev()
            .find(|event| event.kind == IndexKind::Cursor);
        let leaf = match cursor_record {
            // The cursor record carries its selected leaf in `id`; an empty id
            // means the record had no leaf, falling back to the latest event.
            Some(event) => Some(event.id.to_event_id()).filter(|id| !id.is_empty()),
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

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn len(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(0, |metadata| metadata.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[must_use]
    pub fn id(&self) -> String {
        self.path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn leaf_id(&self) -> Option<String> {
        self.lock_leaf().clone()
    }

    /// # Errors
    /// Returns an error when the selected event is absent or the cursor record
    /// cannot be written durably.
    pub fn branch_from(&self, id: String) -> Result<()> {
        let next = (!id.is_empty()).then_some(id);
        let mut leaf = self.lock_leaf();
        persist_cursor(&self.path, next.as_deref(), true)?;
        *leaf = next;
        *self.lock_compaction_index() = None;
        Ok(())
    }

    /// Move the session head to `entry_id` and return the snapshot of the
    /// newly selected lineage. Bundles the head move with the re-read of the
    /// new lineage, so branch switching stays a single core-side operation
    /// rather than a separate write and read.
    /// # Errors
    /// Returns an error when the head cannot be moved or re-indexed.
    pub fn switch_branch(&self, entry_id: String) -> Result<SessionSnapshot> {
        self.branch_from(entry_id)?;
        self.snapshot()
    }

    /// Move the session head to `leaf` and return the snapshot of the
    /// restored lineage, undoing a failed [`switch_branch`](Self::switch_branch).
    /// Pass `None` to restore the detached root, or the previous leaf id to
    /// restore a real head.
    /// # Errors
    /// Returns an error when the head cannot be moved or re-indexed.
    pub fn restore_branch(&self, leaf: Option<String>) -> Result<SessionSnapshot> {
        self.branch_from(leaf.unwrap_or_default())?;
        self.snapshot()
    }

    /// Atomically index the transcript and project it onto this cursor's
    /// selected lineage.
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
    /// selected head.
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

    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn event_at(&self, offset: u64) -> Result<SessionEvent> {
        load_event_at(&self.path, offset)
    }

    /// Read the compaction marker at offset as a
    /// (`summarized`, `kept`, `checkpointed_tail`, `first_kept_entry_id`) tuple.
    /// Returns the zero tuple when the event is missing or is not a
    /// compaction, so projections can treat a corrupt marker as absent.
    #[must_use]
    pub fn compaction_details_at(&self, offset: u64) -> (usize, usize, bool, String) {
        let Ok(ev) = self.event_at(offset) else {
            return (0, 0, false, String::new());
        };
        if let SessionEventKind::Compaction {
            summarized,
            kept,
            checkpointed_tail,
            first_kept_entry_id,
            ..
        } = ev.kind
        {
            (summarized, kept, checkpointed_tail, first_kept_entry_id)
        } else {
            (0, 0, false, String::new())
        }
    }

    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn collapsed_events_at(&self, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
        load_collapsed_events_at(&self.path, offsets)
    }

    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn events_at(&self, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
        load_events_at(&self.path, offsets)
    }

    /// Read an event by its durable ID from this transcript.
    /// This intentionally searches the complete append-only tree, not only the
    /// selected lineage: result recovery may target compacted or abandoned
    /// content by an immutable transcript ID.
    /// # Errors
    /// Propagates transcript indexing and event parsing failures.
    pub fn event_by_id(&self, id: &str) -> Result<Option<SessionEvent>> {
        load_event_by_id(&self.path, id)
    }

    /// # Errors
    /// Propagates transcript seek/read/parse failures and callback errors.
    pub fn visit_events(
        &self,
        offsets: &[u64],
        visit: impl FnMut(SessionEvent) -> Result<()>,
    ) -> Result<()> {
        visit_events(&self.path, offsets, visit)
    }

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
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn events_in_range(&self, start: u64, end: u64) -> Result<Vec<SessionEvent>> {
        load_event_range(&self.path, start, end)
    }

    /// Materialize every non-cursor event in the append-only transcript tree.
    /// This is intentionally distinct from `load_events`, which only returns
    /// the selected lineage. Full-tree consumers still read through the cursor
    /// so tree and lineage access cannot diverge from the rest of the session
    /// API.
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

    /// # Errors
    /// Propagates indexing and event parsing failures.
    pub fn load_events(&self) -> Result<Vec<SessionEvent>> {
        let leaf = self.lock_leaf();
        let (_meta, index, _size) = load_index(&self.path)?;
        load_indexed_path(&self.path, &index, leaf.as_deref())
    }

    /// # Errors
    /// Propagates indexing and event parsing failures.
    pub fn load_compaction_events(&self) -> Result<Vec<SessionEvent>> {
        let leaf = self.lock_leaf();
        if let Some(index) = self.lock_compaction_index().as_ref() {
            return load_compaction_path(&self.path, index, leaf.as_deref());
        }
        let (_meta, index, _size) = load_index(&self.path)?;
        let index = indexed_lineage(index, leaf.as_deref())?;
        let suffix = compaction_index_suffix(&self.path, &index)?;
        let events = load_compaction_path(&self.path, &suffix, leaf.as_deref())?;
        *self.lock_compaction_index() = Some(suffix);
        Ok(events)
    }

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

    /// Append the governing system prompt as a `Message(System)` event off the
    /// current head so the next restore (and every subsequent request) reads it
    /// back from the transcript, not runtime config. Called once at session
    /// start and once right after each compaction, so the latest System event
    /// on the active lineage is always the active one.
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn append_system(&self, system_prompt: &str) -> Result<(u64, u64)> {
        if system_prompt.is_empty() {
            return Ok((0, 0));
        }
        let mut events = vec![SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role: lofi_types::Role::System,
                blocks: vec![lofi_types::ContentBlock::Text {
                    text: system_prompt.to_string(),
                }],
            }),
        }];
        self.append_events(&mut events)
    }

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

/// Walk the root→leaf lineage over a caller-built `by_id` map. This is the
/// single lineage walk; both [`lineage_indices`] and the tree projection use
/// it so the traversal order and parent resolution live in exactly one place.
// Callers build by_id internally with the std hasher; generalizing the hasher
// would only add churn for an index-internal traversal.
#[allow(clippy::implicit_hasher)]
#[must_use]
pub fn lineage_path(
    index: &[EventIndex],
    by_id: &std::collections::HashMap<&IndexId, usize>,
    leaf_id: &str,
) -> Vec<usize> {
    let leaf = IndexId::parse(leaf_id.to_string());
    let mut current = by_id.get(&leaf).copied();
    let mut selected = Vec::new();
    // A cyclic index would otherwise loop forever; cap the walk at the number
    // of events so a malformed transcript terminates rather than hanging.
    while let Some(i) = current {
        if selected.len() > index.len() {
            break;
        }
        selected.push(i);
        current = index[i]
            .parent_id
            .as_ref()
            .and_then(|id| by_id.get(id).copied());
    }
    selected.reverse();
    selected
}

fn lineage_indices(index: &[EventIndex], leaf_id: Option<&str>) -> Result<Vec<usize>> {
    use std::collections::HashMap;
    let by_id: HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .filter(|(_, event)| event.kind != IndexKind::Cursor)
        .map(|(i, event)| (&event.id, i))
        .collect();
    let selected = leaf_id.map_or_else(Vec::new, |id| lineage_path(index, &by_id, id));
    if selected.len() > index.len() {
        return Err(Error::State("cycle in session event lineage".to_string()));
    }
    if leaf_id.is_some() && selected.is_empty() {
        return Err(Error::State("session branch leaf not found".to_string()));
    }
    Ok(selected)
}

fn indexed_lineage(index: Vec<EventIndex>, leaf_id: Option<&str>) -> Result<Vec<EventIndex>> {
    let selected = lineage_indices(&index, leaf_id)?;
    // `selected` from lineage_path is ascending, and `retain` visits in order,
    // so keeping the next wanted index preserves the original ordering while
    // filtering in place. Copying into a fresh Vec would briefly hold both
    // buffers, doubling peak index memory on long single-lineage sessions
    // where nearly every event is retained.
    let mut wanted = selected.into_iter().peekable();
    let mut next = wanted.next();
    let mut position = 0usize;
    let mut index = index;
    index.retain(|_| {
        let keep = next == Some(position);
        if keep {
            next = wanted.next();
        }
        position += 1;
        keep
    });
    Ok(index)
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
    /// Root directory backing every session in this store. Used by the IO
    /// worker pool to key one background thread per store.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// # Errors
    /// Propagates [`crate::state::state_dir`] if the base state dir cannot be
    /// resolved.
    pub fn open() -> Result<Self> {
        let mut p = crate::state::state_dir()?;
        p.push("sessions");
        Ok(Self { root: p })
    }

    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn dir_for_cwd(&self, cwd: &Path) -> PathBuf {
        self.root.join(crate::state::workspace_key(cwd))
    }

    /// The directory is created if needed; the header is written atomically via
    /// a temp file + rename so a partial file is never visible. Returning the
    /// cursor directly prevents active callers from constructing independent
    /// head state around the same path.
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure or [`Error::State`] on a
    /// header-serialization failure.
    pub fn create_cursor(&self, cwd: &Path, model: &RunModel) -> Result<SessionCursor> {
        let path = self.create(cwd, model)?;
        persist_cursor(&path, None, false)?;
        let cursor = SessionCursor::new(path, None);
        *cursor.lock_compaction_index() = Some(Vec::new());
        Ok(cursor)
    }

    fn create(&self, cwd: &Path, model: &RunModel) -> Result<PathBuf> {
        crate::state::ensure_private_dir(&self.root)?;
        let dir = self.dir_for_cwd(cwd);
        crate::state::ensure_private_dir(&dir)?;
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

    /// # Errors
    /// Returns [`Error::Io`] if the per-cwd directory cannot be read for a reason
    /// other than not existing.
    pub fn list_files_for_cwd(&self, cwd: &Path) -> Result<Vec<SessionFile>> {
        let mut files = Vec::new();
        let dir = self.dir_for_cwd(cwd);
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => {
                crate::state::ensure_private_dir(&dir)?;
                read
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(files);
            }
            Err(error) => return Err(Error::Io(error)),
        };
        for entry in read {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(meta) = read_session_meta(&path) else {
                continue;
            };
            if !metadata_matches_cwd(&meta, cwd) {
                continue;
            }
            crate::state::ensure_private_file(&path)?;
            let last_active = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            files.push(SessionFile { path, last_active });
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.last_active));
        Ok(files)
    }

    /// # Errors
    /// Propagates [`list_for_cwd`](Self::list_for_cwd).
    pub fn most_recent(&self, cwd: &Path) -> Result<Option<SessionEntry>> {
        Ok(self.list_for_cwd(cwd)?.into_iter().next())
    }

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
    if header.meta.version != SESSION_VERSION {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let mut events = Vec::new();
    let mut offsets = Vec::new();
    let mut i = 0usize;
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
        let ev = parse_event(line).map_err(|e| {
            Error::State(format!("parse event {} in {}: {e}", i + 1, path.display()))
        })?;
        if matches!(ev.kind, SessionEventKind::Cursor { .. }) {
            continue;
        }
        events.push(ev);
        offsets.push(line_start);
        i += 1;
    }
    Ok((header.meta, events, offsets, pos))
}

enum AppendParent<'a> {
    Explicit(Option<&'a str>),
    #[cfg(test)]
    PhysicalEof,
}

impl AppendParent<'_> {
    #[cfg_attr(not(test), allow(unused_variables, clippy::unnecessary_wraps))]
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
/// Each event is stamped with a fresh `id` (any incoming `id` is
/// overwritten) and chained to the previous one. A supplied `parent_hint`
/// selects an explicit branch parent; otherwise this low-level compatibility
/// helper continues from physical EOF. Active apps use [`SessionCursor`],
/// which always supplies its explicit logical parent instead.
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
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock()?;
    let mut parent = parent.resolve(path)?;
    // Assign a fresh id to each event. `parent_id` is auto-filled only when
    // the event did not set one explicitly, preserving callers that construct
    // a branch inside one append batch.
    for ev in events.iter_mut() {
        ev.id = short_id();
        if ev.parent_id.is_none() {
            ev.parent_id.clone_from(&parent);
        }
        parent = Some(ev.id.clone());
    }
    append_prepared_events(path, events)
}

#[derive(Debug, Clone, Copy)]
pub struct CompactionCounts {
    pub summarized: usize,
    pub represented: usize,
    pub kept: usize,
}

/// Append a compaction checkpoint as one batch: edited kept-tail messages
/// followed by the marker. A failed write is rolled back to the original file
/// length so the transcript cannot expose a partial checkpoint as its leaf.
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

/// # Errors
/// Returns [`Error::State`] if the line is not a serialised [`SessionEvent`].
pub fn parse_event(line: &str) -> Result<SessionEvent> {
    serde_json::from_str::<SessionEvent>(line)
        .map_err(|e| Error::State(format!("parse event: {e}")))
}

#[must_use]
pub fn leaf_id(events: &[SessionEvent]) -> Option<&str> {
    events.last().map(|e| e.id.as_str())
}

/// Derive a structural [`EventIndex`] for an in-memory event slice. Both the
/// transcript view (index-backed) and the memory view (synthesized events) must
/// walk lineage through [`lineage_path`]; deriving the index here lets the
/// memory view use that same walk instead of a second algorithm.
#[must_use]
fn index_for_events(events: &[SessionEvent]) -> Vec<EventIndex> {
    events
        .iter()
        .map(|event| EventIndex {
            id: IndexId::parse(event.id.clone()),
            parent_id: event.parent_id.clone().map(IndexId::parse),
            offset: 0,
            end_offset: 0,
            kind: index_kind_for_event(&event.kind),
        })
        .collect()
}

#[must_use]
pub fn active_path(events: &[SessionEvent], leaf_id: &str) -> Vec<usize> {
    let index = index_for_events(events);
    let by_id: std::collections::HashMap<&IndexId, usize> = index
        .iter()
        .enumerate()
        .filter(|(_, entry)| !entry.id.is_empty())
        .map(|(i, entry)| (&entry.id, i))
        .collect();
    lineage_path(&index, &by_id, leaf_id)
}

#[must_use]
pub fn active_path_from_leaf(events: &[SessionEvent]) -> Vec<usize> {
    leaf_id(events).map_or_else(Vec::new, |id| active_path(events, id))
}

#[must_use]
/// The model recorded on a terminal turn marker, or None for any other event.
/// Shared by the event-slice and index projections so both agree on which
/// markers report a run's model.
pub fn turn_outcome_model(kind: &SessionEventKind) -> Option<&RunModel> {
    match kind {
        SessionEventKind::TurnEnd { model, .. }
        | SessionEventKind::TurnFailed { model, .. }
        | SessionEventKind::TurnCancelled { model, .. } => Some(model),
        _ => None,
    }
}

#[must_use]
pub fn last_run_model(events: &[SessionEvent]) -> Option<RunModel> {
    active_path_from_leaf(events)
        .into_iter()
        .rev()
        .find_map(|i| turn_outcome_model(&events[i].kind).cloned())
}

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
        "turn_cancelled" => "agent: (cancelled)".to_string(),
        "compaction" => format!(
            "compact: Compacted {} messages · kept {}",
            ev.summarized, ev.kept
        ),
        "native_tool" => format!("exec: {} {}", ev.name, one_line(&ev.args)),
        _ => String::new(),
    }
}

/// Scan the file backwards in bounded chunks and return the first complete
/// event that yields a preview. JSONL strings escape embedded newlines, so
/// physical newlines delimit records. Reading backwards lets us return as
/// soon as a parsable line is found without a fixed up-front tail; the
/// common case (last event is a small cursor/turn record) is one 4KB read,
/// and a huge tool-result line at EOF is skipped by extending the window
/// rather than allocating its full payload.
fn quick_entry_preview(path: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};

    const CHUNK: u64 = 4096;
    const MAX_TAIL: u64 = 1024 * 1024;
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut end = len;
    let mut carry: Vec<u8> = Vec::new();
    while end > 0 {
        let start = end.saturating_sub(CHUNK).max(len.saturating_sub(MAX_TAIL));
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut chunk = vec![0u8; usize::try_from(end - start).ok()?];
        file.read_exact(&mut chunk).ok()?;
        // Prepend the previous tail (bytes after the last newline we already
        // saw in later chunks) so a line spanning chunk boundaries reassembles.
        chunk.extend_from_slice(&carry);
        // Trailing bytes after the last newline form a partial line; save for
        // next iteration. The final chunk (start == 0) has no predecessor, so
        // a leading partial line there is the file's first line and is complete.
        if let Some(nl) = chunk.iter().rposition(|byte| *byte == b'\n') {
            carry = chunk.split_off(nl + 1);
        } else {
            carry = chunk;
            if start == 0 {
                // Entire file is one line with no newline; nothing to scan.
                break;
            }
            end = start;
            continue;
        }
        for line in chunk.rsplit(|byte| *byte == b'\n') {
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
        end = start;
    }
    // Nothing parsed from any full line; try the very first bytes we carried
    // (covers a file whose first line is also its last).
    let Ok(event) = serde_json::from_slice::<EntryPreview>(&carry) else {
        return None;
    };
    let preview = entry_preview(&event);
    (!preview.is_empty()).then_some(preview)
}

/// Stream the transcript once into the shared event index and derive the
/// resume-pick entry (message count + last preview) purely from it, walking
/// the selected lineage with the same resolver compaction uses. The picker
/// runs this per session at open; earlier versions materialized a dedicated
/// HashMap<IndexId, Row> per transcript and glibc kept the worker arena's
/// pages mapped at that high-water mark, so picker opens ratcheted RSS by
/// tens of MB on workspaces with deep transcripts.
fn parse_entry(path: &Path, last_active: std::time::SystemTime) -> Option<SessionEntry> {
    let (meta, index, _size) = index::load_index(path).ok()?;
    if meta.version != SESSION_VERSION {
        return None;
    }
    let leaf = index
        .iter()
        .rev()
        .find(|e| e.kind == IndexKind::Cursor)
        .map(|e| e.id.to_event_id())
        .filter(|s| !s.is_empty())
        .or_else(|| index.iter().rev().find(|e| e.kind != IndexKind::Cursor).map(|e| e.id.to_event_id()));
    let lineage = index::lineage_indices(&index, leaf.as_deref()).ok()?;
    let message_count = lineage
        .iter()
        .filter(|&&i| matches!(index[i].kind,
            IndexKind::UserPrompt | IndexKind::AssistantMessage | IndexKind::ToolResult | IndexKind::SystemMessage))
        .count();
    let mut last_message = String::new();
    for &i in lineage.iter().rev() {
        let offset = index[i].offset;
        let mut preview = String::new();
        let mut read_err = false;
        index::visit_event_values::<EntryPreview>(path, &[offset], |ev| {
            preview = entry_preview(&ev);
            Ok(())
        })
        .unwrap_or_else(|_| read_err = true);
        if read_err {
            return None;
        }
        if !preview.is_empty() {
            last_message = preview;
            break;
        }
    }
    Some(SessionEntry {
        meta,
        file: SessionFile {
            path: path.to_path_buf(),
            last_active,
        },
        message_count,
        last_active,
        last_message,
    })
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::State("session path has no parent".into()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("session")
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    crate::state::ensure_private_file(path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}