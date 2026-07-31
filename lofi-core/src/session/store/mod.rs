use std::io::Write;
use std::os::unix::fs::OpenOptionsExt as _;
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

pub const SESSION_VERSION: u32 = 3;

pub const SESSION_MIN_VERSION: u32 = 1;

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

fn legacy_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .replace('/', "-")
        .trim_start_matches('-')
        .to_string()
}

fn workspace_key(cwd: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;

    let label = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| {
            name.chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                        ch
                    } else {
                        '-'
                    }
                })
                .take(40)
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "root".to_string());
    let id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, cwd.as_os_str().as_bytes());
    format!("{label}-{id}")
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

#[cfg(test)]
fn legacy_event_id(offset: u64) -> String {
    format!("legacy-{offset:016x}")
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

    /// Open an existing transcript at its durable selected head. Legacy files
    /// without a cursor record fall back once to their physically last event.
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
    /// newly selected lineage. Bundles the head move with the re-read the UI
    /// always performs immediately after, so branch switching is a single
    /// core-side operation rather than a UI-driven write followed by a read.
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
    /// selected lineage."""
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
    /// selected head. Intended only for branch/tree UI.
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
    /// so path access and legacy ID migration cannot diverge from the rest of
    /// the session API.
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
        self.root.join(workspace_key(cwd))
    }

    fn dirs_for_cwd(&self, cwd: &Path) -> [PathBuf; 2] {
        [self.dir_for_cwd(cwd), self.root.join(legacy_slug(cwd))]
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
        for dir in self.dirs_for_cwd(cwd) {
            let read = match std::fs::read_dir(&dir) {
                Ok(read) => {
                    crate::state::ensure_private_dir(&dir)?;
                    read
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
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
    if !(SESSION_MIN_VERSION..=SESSION_VERSION).contains(&header.meta.version) {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let legacy_v1 = header.meta.version == 1;
    let mut events = Vec::new();
    let mut offsets = Vec::new();
    let mut i = 0usize;
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
/// Returns [`Error::State`] if the line is neither a tagged event nor a
/// legacy `Message` object.
pub fn parse_event(line: &str) -> Result<SessionEvent> {
    if let Ok(ev) = serde_json::from_str::<SessionEvent>(line) {
        return Ok(ev);
    }
    serde_json::from_str::<Message>(line)
        .map(|m| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(m),
        })
        .map_err(|e| Error::State(format!("parse event: {e}")))
}

#[must_use]
pub fn leaf_id(events: &[SessionEvent]) -> Option<&str> {
    events.last().map(|e| e.id.as_str())
}

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

#[must_use]
pub fn active_path_from_leaf(events: &[SessionEvent]) -> Vec<usize> {
    leaf_id(events).map_or_else(Vec::new, |id| active_path(events, id))
}

#[must_use]
pub fn last_run_model(events: &[SessionEvent]) -> Option<RunModel> {
    active_path_from_leaf(events)
        .into_iter()
        .rev()
        .find_map(|i| match &events[i].kind {
            SessionEventKind::TurnEnd { model, .. }
            | SessionEventKind::TurnFailed { model, .. }
            | SessionEventKind::TurnCancelled { model, .. } => Some(model.clone()),
            _ => None,
        })
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
                    | IndexKind::SystemMessage
            )
        })
        .count();
    // Usually the first candidate is meaningful. Read newest-to-oldest and
    // stop immediately instead of parsing every selected event.
    let mut last_message = String::new();
    for &i in selected.iter().rev() {
        if matches!(index[i].kind, IndexKind::Cursor | IndexKind::Other) {
            continue;
        }
        let offset = index[i].offset;
        if visit_event_values::<EntryPreview>(path, &[offset], |event| {
            last_message = entry_preview(&event);
            Ok(())
        })
        .is_err()
        {
            return None;
        }
        if !last_message.is_empty() {
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;
    use lofi_types::{ContentBlock, Role, SessionEventKind, ThinkingLevel, Usage};

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

    /// A store rooted at a fresh temp dir, so tests never touch real state
    /// and never race on the process-global `XDG_STATE_HOME` env var.
    fn isolated_store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        (dir, store)
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn collapsed_loader_skips_only_invisible_success_bodies() {
        let (_guard, store) = isolated_store();
        let path = store
            .create(Path::new("/tmp/collapsed-projection"), &"p/m".into())
            .unwrap();
        let large = "x".repeat(128 * 1024);
        let mut events = vec![
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text {
                        text: "  raw prompt  \n".to_string(),
                    }],
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::ToolUse {
                        id: "exec-1".to_string(),
                        name: "exec".to_string(),
                        input: serde_json::json!({"code": "x"}),
                    }],
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(Message {
                    role: Role::Tool,
                    blocks: vec![ContentBlock::ToolResult {
                        tool_use_id: "exec-1".to_string(),
                        content: large.clone(),
                        is_error: false,
                    }],
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::NativeTool(lofi_types::NativeToolRecord {
                    parent: "exec-1".to_string(),
                    call_id: 0,
                    name: "read".to_string(),
                    args: "a.txt".to_string(),
                    result: large.clone(),
                    is_error: false,
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::NativeTool(lofi_types::NativeToolRecord {
                    parent: "exec-1".to_string(),
                    call_id: 1,
                    name: "write".to_string(),
                    args: "b.txt".to_string(),
                    result: "visible write result".to_string(),
                    is_error: false,
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::NativeTool(lofi_types::NativeToolRecord {
                    parent: "exec-1".to_string(),
                    call_id: 2,
                    name: "read".to_string(),
                    args: "missing".to_string(),
                    result: "visible error".to_string(),
                    is_error: true,
                }),
            },
        ];
        append_events(&path, &mut events, None).unwrap();
        let (_, index, _) = load_index(&path).unwrap();
        let offsets: Vec<_> = index
            .iter()
            .filter(|entry| entry.kind != IndexKind::Cursor)
            .map(|entry| entry.offset)
            .collect();
        let collapsed = load_collapsed_events_at(&path, &offsets).unwrap();
        let complete = load_events_at(&path, &offsets).unwrap();

        let tool_result = match &collapsed[2].kind {
            SessionEventKind::Message(message) => match &message.blocks[0] {
                ContentBlock::ToolResult { content, .. } => content,
                _ => panic!("expected tool result"),
            },
            _ => panic!("expected tool message"),
        };
        assert!(tool_result.is_empty());
        let natives: Vec<_> = collapsed
            .iter()
            .filter_map(|event| match &event.kind {
                SessionEventKind::NativeTool(record) => Some(record),
                _ => None,
            })
            .collect();
        assert!(natives[0].result.is_empty());
        assert_eq!(natives[1].result, "visible write result");
        assert_eq!(natives[2].result, "visible error");
        let full_read = complete.iter().find_map(|event| match &event.kind {
            SessionEventKind::NativeTool(record) if record.name == "read" && !record.is_error => {
                Some(&record.result)
            }
            _ => None,
        });
        assert_eq!(full_read.map(String::len), Some(large.len()));
        assert_eq!(
            SessionCursor::new(path, None)
                .prompt_texts(&[offsets[0]])
                .unwrap(),
            vec!["  raw prompt  \n".to_string()]
        );
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
            header.contains("\"model\":{\"provider\":\"openai\",\"id\":\"gpt-5.6-sol\",\"thinking\":\"medium\"}"),
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
            },
        }];
        append_events(&path, &mut batch, None).unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        let turn_end_line = body.lines().last().unwrap();
        assert!(
            turn_end_line.contains(
                "\"model\":{\"provider\":\"anthropic\",\"id\":\"claude\",\"thinking\":\"high\"}"
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
        let root_id = trunk[0].id.clone();
        let old_leaf = trunk[1].id.clone();

        cursor.branch_from(root_id.clone()).unwrap();
        drop(cursor);

        let resumed = SessionCursor::open(path.clone()).unwrap();
        assert_eq!(resumed.leaf_id().as_deref(), Some(root_id.as_str()));
        let snapshot = resumed.snapshot().unwrap();
        assert_eq!(snapshot.index.len(), 1);
        assert!(snapshot.index[0].id.matches(&root_id));

        let mut branch = [ev(user("new branch"))];
        resumed.append_events(&mut branch).unwrap();
        assert_eq!(branch[0].parent_id.as_deref(), Some(trunk[0].id.as_str()));
        assert_ne!(branch[0].parent_id.as_deref(), Some(old_leaf.as_str()));

        let reopened = SessionCursor::open(path).unwrap();
        assert_eq!(reopened.leaf_id().as_deref(), Some(branch[0].id.as_str()));
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
        assert!(
            matches!(&events[1].kind, SessionEventKind::Message(m) if m.role == Role::Assistant)
        );
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
            matches!(&events[2].kind, SessionEventKind::TurnEnd { model, elapsed_ms: 1234, cost, usage }
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
        let branch_ev = events.iter().find(|e| {
            matches!(&e.kind, SessionEventKind::Message(m) if m.role == Role::User
                && m.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "alt")))
        }).expect("branch event present");
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
    fn legacy_bare_message_lines_still_load() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/legacy");
        let path = store.create(cwd, &"m".into()).unwrap();
        let legacy = serde_json::to_string(&user("old")).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(format!("{legacy}\n").as_bytes())
            .unwrap();
        let (_meta, events, _, _) = load(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0].kind, SessionEventKind::Message(m) if m.role == Role::User));

        let (_, index, _) = load_index(&path).unwrap();
        assert!(index[0].id.matches(&events[0].id));
        let recovered = load_event_by_id(&path, &index[0].id.to_event_id())
            .unwrap()
            .unwrap();
        assert!(index[0].id.matches(&recovered.id));
        assert!(matches!(&recovered.kind, SessionEventKind::Message(m) if m.role == Role::User));
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
    fn most_recent_and_find_prefix() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/y");
        let _p1 = store.create(cwd, &"m".into()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let p2 = store.create(cwd, &"m".into()).unwrap();
        let mr = store.most_recent(cwd).unwrap().unwrap();
        assert_eq!(mr.file.path, p2);
        let id2 = p2.file_stem().unwrap().to_str().unwrap();
        let prefix = &id2[..id2.find('_').unwrap()];
        let found = store.find(cwd, prefix).unwrap().unwrap();
        assert_eq!(found.file.path, p2);
        assert!(store.find(cwd, "zzz").unwrap().is_none());
    }

    #[test]
    fn legacy_collision_does_not_cross_workspace_boundary() {
        let (_guard, store) = isolated_store();
        let first = Path::new("/work/a-b/c");
        let second = Path::new("/work/a/b-c");
        assert_eq!(legacy_slug(first), legacy_slug(second));

        let original = store.create(first, &"p/m".into()).unwrap();
        let legacy_dir = store.root.join(legacy_slug(first));
        crate::state::ensure_private_dir(&legacy_dir).unwrap();
        let legacy = legacy_dir.join(original.file_name().unwrap());
        std::fs::rename(original, legacy).unwrap();

        assert_eq!(store.list_for_cwd(first).unwrap().len(), 1);
        assert!(store.list_for_cwd(second).unwrap().is_empty());
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
    fn workspace_key_is_readable_and_collision_resistant() {
        let first = workspace_key(Path::new("/work/a-b/c"));
        let second = workspace_key(Path::new("/work/a/b-c"));
        assert!(first.starts_with("c-"));
        assert!(second.starts_with("b-c-"));
        assert_ne!(first, second);
        assert_eq!(workspace_key(Path::new("/work/a-b/c")), first);
    }

    #[test]
    fn legacy_slug_collapses_separators() {
        assert_eq!(legacy_slug(Path::new("/home/sirn/dev")), "home-sirn-dev");
        assert_eq!(legacy_slug(Path::new("/")), "");
    }
}
