use std::path::{Path, PathBuf};

use lofi_error::{Error, Result};
use lofi_types::{Message, PromptKind, SessionEvent, SessionEventKind};
use serde::Deserialize;

use super::append::{
    append_compaction_from, append_prepared_events, AppendParent, CompactionCounts,
};
use super::index::{
    compaction_index_suffix, load_compaction_path, load_display_event_range,
    load_display_events_at, load_event_at, load_event_range, load_event_values, load_events_at,
    load_index, load_index_range, load_indexed_path, load_linear_session_index,
    session_index_from_lineage, visit_display_events, visit_events,
};
use super::{
    short_id, EventIndex, IndexId, IndexKind, RecoveryMessage, SessionIndexEntry, SessionMeta,
};

#[derive(Debug)]
pub struct SessionSnapshot {
    pub meta: SessionMeta,
    pub index: Vec<SessionIndexEntry>,
    pub file_size: u64,
    pub history_start: usize,
    /// The selected lineage is a physical file prefix. Consumers can replay
    /// byte ranges directly instead of retaining one offset vector per turn.
    pub contiguous: bool,
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
        if let Some(linear) = load_linear_session_index(&path)? {
            let cursor = Self::new(path, linear.leaf_id);
            *cursor.lock_compaction_index() = Some(linear.compaction_suffix);
            return Ok((
                cursor,
                SessionSnapshot {
                    meta: linear.meta,
                    index: linear.entries,
                    file_size: linear.file_size,
                    history_start: linear.history_start,
                    contiguous: true,
                },
            ));
        }

        let (meta, index, file_size) = load_index(&path)?;
        let cursor_record = index
            .iter()
            .rev()
            .find(|event| event.kind == IndexKind::Cursor);
        let leaf = match cursor_record {
            Some(event) => Some(event.id.to_event_id()).filter(|id| !id.is_empty()),
            None => index
                .iter()
                .rev()
                .find(|event| event.kind != IndexKind::Cursor)
                .map(|event| event.id.to_event_id()),
        };
        let selected = indexed_lineage(index, leaf.as_deref())?;
        let (resume_index, history_start) = session_index_from_lineage(&path, &selected)?;
        let cursor = Self::new(path, leaf);
        *cursor.lock_compaction_index() = Some(compaction_index_suffix(&cursor.path, &selected)?);
        drop(selected);
        lofi_code::memory::release_freed_memory();
        Ok((
            cursor,
            SessionSnapshot {
                meta,
                index: resume_index,
                file_size,
                history_start,
                contiguous: false,
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
        let contiguous = leaf
            .as_deref()
            .is_some_and(|leaf| linear_lineage_end(&index, leaf).is_some());
        let lineage = indexed_lineage(index, leaf.as_deref())?;
        let (index, history_start) = session_index_from_lineage(&self.path, &lineage)?;
        *self.lock_compaction_index() = Some(compaction_index_suffix(&self.path, &lineage)?);
        drop(lineage);
        lofi_code::memory::release_freed_memory();
        Ok(SessionSnapshot {
            meta,
            index,
            file_size,
            history_start,
            contiguous,
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
    pub fn events_at(&self, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
        load_events_at(&self.path, offsets)
    }

    /// Read one event through the bounded transcript-display projection.
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn display_event_at(&self, offset: u64) -> Result<SessionEvent> {
        let mut events = load_display_events_at(&self.path, &[offset])?;
        events.pop().ok_or_else(|| {
            Error::State(format!(
                "no event at offset {offset} in {}",
                self.path.display()
            ))
        })
    }

    /// Read selected events through the bounded transcript-display projection.
    /// The raw transcript stays unchanged for model context and result recovery.
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn display_events_at(&self, offsets: &[u64]) -> Result<Vec<SessionEvent>> {
        load_display_events_at(&self.path, offsets)
    }

    /// Read only the bounded message payload needed by result recovery.
    /// This searches the complete append-only tree, so compacted and abandoned
    /// content remains addressable by its immutable transcript ID.
    /// # Errors
    /// Propagates transcript indexing and event parsing failures.
    pub fn recovery_message_by_id(&self, id: &str) -> Result<Option<RecoveryMessage>> {
        let (_meta, index, _size) = load_index(&self.path)?;
        let Some(entry) = index.iter().find(|entry| entry.id.matches(id)) else {
            return Ok(None);
        };
        if !matches!(
            entry.kind,
            IndexKind::UserPrompt
                | IndexKind::AssistantMessage
                | IndexKind::SystemMessage
                | IndexKind::ToolResult
        ) {
            return Ok(Some(RecoveryMessage::NotMessage));
        }
        let mut events = load_display_events_at(&self.path, &[entry.offset])?;
        let Some(event) = events.pop() else {
            return Ok(None);
        };
        match event.kind {
            SessionEventKind::Message(message) => Ok(Some(RecoveryMessage::Message(message))),
            _ => Ok(Some(RecoveryMessage::NotMessage)),
        }
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

    /// Visit selected events through a fresh bounded projection per event.
    /// Use this for streaming scans that do not retain event payloads.
    /// # Errors
    /// Propagates transcript seek/read/parse failures and callback errors.
    pub fn visit_display_events(
        &self,
        offsets: &[u64],
        visit: impl FnMut(SessionEvent) -> Result<()>,
    ) -> Result<()> {
        visit_display_events(&self.path, offsets, visit)
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

        Ok(load_event_values::<NativeToolSummary>(&self.path, offsets)?
            .into_iter()
            .map(|record| (record.parent, record.name, record.args))
            .collect())
    }

    /// Read only the first text block and the prompt kind from user-message
    /// events. Assistant and tool payloads in the same record shape are
    /// skipped without allocation; startup replay uses this to build
    /// file-backed historical turn shells, and notice turns must come back
    /// marked so they do not render as typed input.
    /// # Errors
    /// Propagates transcript seek/read/parse failures.
    pub fn prompt_texts(&self, offsets: &[u64]) -> Result<Vec<(String, lofi_types::PromptKind)>> {
        #[derive(Deserialize)]
        struct PromptProjection {
            #[serde(default)]
            blocks: Vec<PromptBlockProjection>,
            #[serde(default)]
            kind: lofi_types::PromptKind,
        }
        #[derive(Deserialize)]
        struct PromptBlockProjection {
            #[serde(default, rename = "type")]
            kind: String,
            #[serde(default)]
            text: String,
        }

        Ok(load_event_values::<PromptProjection>(&self.path, offsets)?
            .into_iter()
            .map(|event| {
                (
                    event
                        .blocks
                        .into_iter()
                        .find_map(|block| (block.kind == "text").then_some(block.text))
                        .unwrap_or_default(),
                    event.kind,
                )
            })
            .collect())
    }

    /// Read all non-cursor events physically contained in one committed byte
    /// range. The range is a durable handle recorded by this same cursor.
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn events_in_range(&self, start: u64, end: u64) -> Result<Vec<SessionEvent>> {
        load_event_range(&self.path, start, end)
    }

    /// Read one committed range through the bounded transcript-display
    /// projection. The bound is shared across the complete range.
    /// # Errors
    /// Propagates transcript seek, read, and parsing failures.
    pub fn display_events_in_range(&self, start: u64, end: u64) -> Result<Vec<SessionEvent>> {
        load_display_event_range(&self.path, start, end)
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

    /// Collect which of the given job ids have a `JobFinished` marker
    /// anywhere in the transcript tree (not just the selected lineage).
    /// Used by branch-switch reconciliation to avoid reporting a job as
    /// stale when it already completed on a pruned branch.
    /// # Errors
    /// Propagates indexing and event parsing failures.
    pub fn finished_job_ids_anywhere(
        &self,
        job_ids: &[u64],
    ) -> Result<std::collections::HashSet<u64>> {
        if job_ids.is_empty() {
            return Ok(std::collections::HashSet::new());
        }
        let _leaf = self.lock_leaf();
        let (_meta, index, _size) = load_index(&self.path)?;
        let offsets: Vec<u64> = index
            .iter()
            .filter(|entry| entry.kind == IndexKind::JobLifecycle)
            .map(|entry| entry.offset)
            .collect();
        let mut finished = std::collections::HashSet::new();
        for offset in offsets {
            let event = load_event_at(&self.path, offset)?;
            if let SessionEventKind::JobFinished { job_id } = event.kind {
                if job_ids.contains(&job_id) {
                    finished.insert(job_id);
                }
            }
        }
        Ok(finished)
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
        self.append_events_at_leaf(events, &mut leaf)
    }

    fn append_events_at_leaf(
        &self,
        events: &mut [SessionEvent],
        leaf: &mut Option<String>,
    ) -> Result<(u64, u64)> {
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

    /// Append events only while `ancestor_id` remains on the selected
    /// lineage. The lineage check and append share the cursor-head lock, so a
    /// branch switch cannot land between them.
    pub(crate) fn append_events_if_ancestor(
        &self,
        ancestor_id: &str,
        events: &mut [SessionEvent],
    ) -> Result<Option<(u64, u64)>> {
        let mut leaf = self.lock_leaf();
        let (_meta, index, _file_size) = load_index(&self.path)?;
        let selected = indexed_lineage(index, leaf.as_deref())?;
        if !selected.iter().any(|entry| entry.id.matches(ancestor_id)) {
            return Ok(None);
        }
        self.append_events_at_leaf(events, &mut leaf).map(Some)
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
                origin: None,
                role: lofi_types::Role::System,
                blocks: vec![lofi_types::ContentBlock::Text {
                    text: system_prompt.to_string(),
                }],
                kind: PromptKind::default(),
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

    pub(super) fn lock_compaction_index(
        &self,
    ) -> std::sync::MutexGuard<'_, Option<Vec<EventIndex>>> {
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

pub(super) fn lineage_indices(index: &[EventIndex], leaf_id: Option<&str>) -> Result<Vec<usize>> {
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

fn indexed_lineage(mut index: Vec<EventIndex>, leaf_id: Option<&str>) -> Result<Vec<EventIndex>> {
    let Some(leaf_id) = leaf_id else {
        index.clear();
        return Ok(index);
    };
    if let Some(end) = linear_lineage_end(&index, leaf_id) {
        index.truncate(end);
        return Ok(index);
    }

    let selected = lineage_indices(&index, Some(leaf_id))?;
    // `selected` from lineage_path is ascending, and `retain` visits in order,
    // so keeping the next wanted index preserves the original ordering while
    // filtering in place. Copying into a fresh Vec would briefly hold both
    // buffers, doubling peak index memory on branched sessions.
    let mut wanted = selected.into_iter().peekable();
    let mut next = wanted.next();
    let mut position = 0usize;
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

/// Return the selected prefix length when events form one physical parent
/// chain through `leaf_id`. This is the normal transcript shape. It avoids
/// allocating a full id map and position vector merely to keep the existing
/// index. A branch or malformed parent falls back to the general graph walk.
fn linear_lineage_end(index: &[EventIndex], leaf_id: &str) -> Option<usize> {
    let mut previous = None;
    for (position, event) in index.iter().enumerate() {
        if event.kind == IndexKind::Cursor {
            continue;
        }
        let linked = match (event.parent_id.as_ref(), previous) {
            (None, None) => true,
            (Some(parent), Some(previous)) => parent == previous,
            _ => false,
        };
        if !linked {
            return None;
        }
        if event.id.matches(leaf_id) {
            return Some(position + 1);
        }
        previous = Some(&event.id);
    }
    None
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
    let (byte_start, end) = append_prepared_events(path, events)?;
    Ok((byte_start, end, next))
}
