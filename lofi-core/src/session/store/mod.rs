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
pub use index::{
    load_compaction_path, load_event_at, load_event_by_id, load_events_at, load_index,
    load_indexed_path, visit_event_lines, EventIndex, IndexKind,
};

/// Transcript format version. Bumped only on a breaking on-disk change;
/// older files are rejected (no migration yet — lofi has no shipped sessions
/// to migrate).
pub const SESSION_VERSION: u32 = 2;

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

/// A discoverable session on disk: its metadata, file path, and message count.
#[derive(Debug, Clone)]
pub struct SessionEntry {
    pub meta: SessionMeta,
    pub path: PathBuf,
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
        self.path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(ToString::to_string)
            .unwrap_or_default()
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
fn legacy_event_id(offset: u64) -> String {
    format!("legacy-{offset:016x}")
}

/// A shared logical write cursor for one append-only transcript.
///
/// Clones share the active leaf. Every durable append holds the cursor lock,
/// writes one file-locked batch, and advances the leaf before releasing it.
/// Physical EOF is therefore never used after cursor construction, even when
/// multiple processes append sibling branches to the same JSONL file.
#[derive(Debug, Clone)]
pub struct SessionCursor {
    path: PathBuf,
    leaf_id: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl SessionCursor {
    /// Construct a cursor at a known logical leaf. `None` denotes the
    /// explicit position before all roots, whether or not the file has events.
    #[must_use]
    pub fn new(path: PathBuf, leaf_id: Option<String>) -> Self {
        Self {
            path,
            leaf_id: std::sync::Arc::new(std::sync::Mutex::new(
                leaf_id.filter(|id| !id.is_empty()),
            )),
        }
    }

    /// Transcript path owned by this cursor.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Snapshot the current logical leaf.
    #[must_use]
    pub fn leaf_id(&self) -> Option<String> {
        self.lock_leaf().clone()
    }

    /// Move the cursor to an explicit branch point selected by the user.
    pub fn branch_from(&self, id: String) {
        *self.lock_leaf() = (!id.is_empty()).then_some(id);
    }

    /// Append one event batch to this cursor's lineage and advance its leaf.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn append_events(&self, events: &mut [SessionEvent]) -> Result<(u64, u64)> {
        let mut leaf = self.lock_leaf();
        let range =
            append_events_from(&self.path, events, AppendParent::Explicit(leaf.as_deref()))?;
        if let Some(event) = events.last() {
            *leaf = Some(event.id.clone());
        }
        Ok(range)
    }

    /// Append a complete compaction checkpoint and advance to its marker.
    ///
    /// # Errors
    /// Propagates transcript serialization and I/O failures.
    pub fn append_compaction(
        &self,
        kept_messages: &[Message],
        summary: String,
        summarized_range: [String; 2],
        counts: CompactionCounts,
    ) -> Result<(u64, u64)> {
        let mut leaf = self.lock_leaf();
        let (start, end, marker_id) = append_compaction_from(
            &self.path,
            kept_messages,
            AppendParent::Explicit(leaf.as_deref()),
            &summary,
            &summarized_range,
            counts,
        )?;
        *leaf = Some(marker_id);
        Ok((start, end))
    }

    fn lock_leaf(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.leaf_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

    /// Create a new session file under the cwd's sessions dir and return its path.
    ///
    /// The directory is created if needed; the header is written atomically via
    /// a temp file + rename so a partial file is never visible.
    ///
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure or [`Error::State`] on a
    /// header-serialization failure.
    pub fn create(&self, cwd: &Path, model: &RunModel) -> Result<PathBuf> {
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
        let dir = self.dir_for_cwd(cwd);
        let mut entries = Vec::new();
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
            if let Some(entry) = parse_entry(&path) {
                entries.push(entry);
            }
        }
        // Newest activity first: descending by file mtime (last write).
        entries.sort_by_key(|e| std::cmp::Reverse(e.last_active));
        Ok(entries)
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
pub fn load(path: &Path) -> Result<(SessionMeta, Vec<SessionEvent>, Vec<u64>, u64)> {
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
    PhysicalEof,
}

impl AppendParent<'_> {
    fn resolve(self, path: &Path) -> Result<Option<String>> {
        match self {
            Self::Explicit(parent) => Ok(parent.map(str::to_string)),
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
pub fn append_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent_hint: Option<&str>,
) -> Result<(u64, u64)> {
    let parent = parent_hint.map_or(AppendParent::PhysicalEof, |id| {
        AppendParent::Explicit(Some(id))
    });
    append_events_from(path, events, parent)
}

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
pub fn append_compaction(
    path: &Path,
    kept_messages: &[Message],
    parent_hint: Option<&str>,
    summary: String,
    summarized_range: [String; 2],
    counts: CompactionCounts,
) -> Result<(u64, u64, String)> {
    let parent = parent_hint.map_or(AppendParent::PhysicalEof, |id| {
        AppendParent::Explicit(Some(id))
    });
    append_compaction_from(
        path,
        kept_messages,
        parent,
        &summary,
        &summarized_range,
        counts,
    )
}

fn append_compaction_from(
    path: &Path,
    kept_messages: &[Message],
    parent: AppendParent<'_>,
    summary: &str,
    summarized_range: &[String; 2],
    counts: CompactionCounts,
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
pub fn last_event_id(path: &Path) -> Result<Option<String>> {
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
            last = Some(ev.id);
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
///
/// This is the tree walk that builds the agent's conversation history and
/// the UI's visible turn list: branching changes the leaf, and the walk
/// excludes sibling branches automatically.
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
        let ev = &events[i];
        cur = ev.parent_id.as_deref().and_then(|p| by_id.get(p).copied());
        // Guard against a cycle (malformed parent linkage).
        if path.len() > events.len() {
            return Vec::new();
        }
    }
    path.reverse();
    path
}

/// Indices of the events on the path from the last event (the active leaf)
/// to the root, in root-first order. Convenience for the common resume case
/// where the leaf is the file's last-appended entry.
#[must_use]
pub fn active_path_from_leaf(events: &[SessionEvent]) -> Vec<usize> {
    match leaf_id(events) {
        Some(id) => active_path(events, id),
        None => Vec::new(),
    }
}

/// The raw model+thinking of the last completed turn on the active path, so
/// a resumed session can restore the model the user last ran with rather than
/// the config default. Walks the active path from the leaf backward so a
/// sibling branch never supplies a stale model; `None` when the session has
/// no `TurnEnd`/`TurnFailed` marker (fresh, only a pending prompt, or a
/// force-stopped turn with no completed turn before it).
#[must_use]
pub fn last_run_model(events: &[SessionEvent]) -> Option<RunModel> {
    let path = active_path_from_leaf(events);
    for &i in path.iter().rev() {
        match &events[i].kind {
            SessionEventKind::TurnEnd { model, .. }
            | SessionEventKind::TurnFailed { model, .. } => return Some(model.clone()),
            _ => {}
        }
    }
    None
}

/// Minimal borrowed shape used by the session picker. Unknown fields, including
/// large tool-result and native-tool result bodies, are skipped.
#[derive(Deserialize)]
struct EntryPreview<'a> {
    #[serde(default, rename = "type")]
    kind: &'a str,
    #[serde(default)]
    role: &'a str,
    #[serde(default, borrow)]
    blocks: Vec<EntryPreviewBlock<'a>>,
    #[serde(default, borrow)]
    error: std::borrow::Cow<'a, str>,
    #[serde(default, borrow)]
    name: std::borrow::Cow<'a, str>,
    #[serde(default, borrow)]
    args: std::borrow::Cow<'a, str>,
    #[serde(default)]
    summarized: usize,
    #[serde(default)]
    kept: usize,
}

#[derive(Deserialize)]
struct EntryPreviewBlock<'a> {
    #[serde(default, rename = "type")]
    kind: &'a str,
    #[serde(default, borrow)]
    text: std::borrow::Cow<'a, str>,
}

/// Truncate to one line without allocating a Vec containing every character.
fn one_line(s: &str) -> String {
    let line = s.split('\n').next().unwrap_or("");
    line.char_indices()
        .nth(80)
        .map_or_else(|| line.to_string(), |(end, _)| format!("{}…", &line[..end]))
}

fn entry_preview(ev: &EntryPreview<'_>) -> String {
    match ev.kind {
        "message" | "" => {
            let Some(text) = ev
                .blocks
                .iter()
                .find_map(|b| (b.kind == "text" && !b.text.is_empty()).then_some(b.text.as_ref()))
            else {
                return String::new();
            };
            let prefix = match ev.role {
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

fn parse_entry(path: &Path) -> Option<SessionEntry> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let header: Header = serde_json::from_str(line.trim_end_matches(['\n', '\r'])).ok()?;
    if !(SESSION_MIN_VERSION..=SESSION_VERSION).contains(&header.meta.version) {
        return None;
    }
    let mut count = 0usize;
    let mut last_message = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let raw = line.trim_end_matches(['\n', '\r']);
        if raw.is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<EntryPreview<'_>>(raw) else {
            continue;
        };
        if ev.kind == "message" || (ev.kind.is_empty() && !ev.role.is_empty()) {
            count += 1;
        }
        let preview = entry_preview(&ev);
        if !preview.is_empty() {
            last_message = preview;
        }
    }
    let last_active = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    Some(SessionEntry {
        meta: header.meta,
        path: path.to_path_buf(),
        message_count: count,
        last_active,
        last_message,
    })
}

/// Write `contents` to `path` atomically: write a temp sibling, then rename.
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
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]

    use super::*;
    use lofi_types::{ContentBlock, Role, SessionEventKind, ThinkingLevel, Usage};

    /// Wrap a message as a `Message` session event (`id/parent_id` left empty;
    /// `append_events` assigns and chains them).
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
            "summary".into(),
            ["first".into(), "last".into()],
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
            "summary".into(),
            [original[0].id.clone(), original[0].id.clone()],
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
                "summary".into(),
                [root_id.clone(), reply[0].id.clone()],
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
            .find(|event| event.id == new_root[0].id)
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
                "summary".into(),
                ["first".into(), "last".into()],
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
        // Linear chain: first event is root, second chains to first.
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
        // Root chain: user -> assistant.
        let mut first = [ev(user("a")), ev(assistant("b"))];
        append_events(&path, &mut first, None).unwrap();
        let (_meta, base, _, _) = load(&path).unwrap();
        let root_id = base[0].id.clone();
        // Branch a sibling user message off the root (not off the assistant).
        let mut branch = [ev(user("alt"))];
        append_events(&path, &mut branch, Some(&root_id)).unwrap();
        let (_meta, events, _, _) = load(&path).unwrap();
        // 3 events total; the branch's parent is the root, not the assistant.
        assert_eq!(events.len(), 3);
        let branch_ev = events.iter().find(|e| {
            matches!(&e.kind, SessionEventKind::Message(m) if m.role == Role::User
                && m.blocks.iter().any(|b| matches!(b, ContentBlock::Text { text } if text == "alt")))
        }).expect("branch event present");
        assert_eq!(branch_ev.parent_id.as_deref(), Some(root_id.as_str()));
        // active_path from the branch leaf is [root, branch] — the assistant
        // is a sibling and excluded.
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
        // Root chain ends with model A; a sibling branch off the root user
        // ends with model B and is the active leaf, so B is restored (not A).
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
        // Pre-event-log files stored bare Message JSON, one per line.
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
        assert_eq!(index[0].id, events[0].id);
        let recovered = load_event_by_id(&path, &index[0].id).unwrap().unwrap();
        assert_eq!(recovered.id, index[0].id);
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
        // p2 was created after p1 was last written, so p2 is most recently
        // active and should be listed first.
        assert_eq!(list[0].path, p2);
        assert_eq!(list[1].path, p1);
        assert_eq!(list[1].message_count, 1);
        assert_eq!(list[0].message_count, 0);
    }

    #[test]
    fn most_recent_and_find_prefix() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/findy");
        let _p1 = store.create(cwd, &"m".into()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let p2 = store.create(cwd, &"m".into()).unwrap();
        let mr = store.most_recent(cwd).unwrap().unwrap();
        assert_eq!(mr.path, p2);
        let id2 = p2.file_stem().unwrap().to_str().unwrap();
        let prefix = &id2[..id2.find('_').unwrap()];
        let found = store.find(cwd, prefix).unwrap().unwrap();
        assert_eq!(found.path, p2);
        assert!(store.find(cwd, "zzz").unwrap().is_none());
    }

    #[test]
    fn slug_collapses_separators() {
        assert_eq!(slug(Path::new("/home/sirn/dev")), "home-sirn-dev");
        assert_eq!(slug(Path::new("/")), "");
    }
}
