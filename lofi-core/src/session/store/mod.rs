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
pub use index::{load_event_at, load_index, EventIndex, IndexKind};

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

/// A short, low-entropy disambiguator derived from nanosecond jitter. This is
/// not a security id — it only needs to keep same-millisecond file names apart.
fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    format!("{nanos:06x}")
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
    let line = serde_json::to_string(&header)
        .map_err(|e| Error::State(format!("json: {e}")))?;
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
    // Newest first: descending by the id (timestamp-led stem).
    entries.sort_by_key(|e| std::cmp::Reverse(e.id()));
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
    let header: Header = serde_json::from_str(&header_line)
        .map_err(|e| Error::State(format!("json: {e}")))?;
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
    // For v1 migration and defensively-malformed v2 files: assign ids to
    // any event missing one and chain `parent_id` to the previous event
    // (or `None` for the first), producing a linear chain that matches the
    // pre-tree semantics.
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
        if legacy_v1 || ev.id.is_empty() {
            ev.id = short_id();
        }
        if legacy_v1 || ev.parent_id.is_none() {
            ev.parent_id.clone_from(&prev_id);
        }
        prev_id = Some(ev.id.clone());
        events.push(ev);
        offsets.push(line_start);
        i += 1;
    }
    Ok((header.meta, events, offsets, pos))
}

/// Append session events to a transcript file (one JSON line each). The
/// file is flushed before returning so a crash after the turn still has the
/// data.
///
/// Each event is stamped with a fresh `id` (any incoming `id` is
/// overwritten) and chained to the previous one: the first event's
/// `parent_id` is `parent_hint` when given, otherwise the file's current
/// last event's id (so a continuation appends to the active leaf), or `None`
/// if the file has no events yet (the root). `parent_hint` is how a branch
/// is created — pass the entry id to branch from and the new turn becomes a
/// sibling of the existing children.
///
/// # Errors
/// Returns [`Error::Io`] on open/write failure or [`Error::State`] on a
/// serialization failure.
pub fn append_events(
    path: &Path,
    events: &mut [SessionEvent],
    parent_hint: Option<&str>,
) -> Result<(u64, u64)> {
    if events.is_empty() {
        let len = std::fs::metadata(path).map_or(0, |m| m.len());
        return Ok((len, len));
    }
    // Resolve the first event's parent: explicit hint (branch) > the file's
    // current last event (linear continuation) > None (root).
    let mut parent = match parent_hint {
        Some(id) => Some(id.to_string()),
        None => last_event_id(path)?,
    };
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
    let byte_start = std::fs::metadata(path).map_or(0, |m| m.len());
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    for ev in events {
        let line = serde_json::to_string(ev)
            .map_err(|e| Error::State(format!("json: {e}")))?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
    }
    file.flush()?;
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

/// Parse a session file into a [`SessionEntry`] (metadata + message count),
/// or `None` if the file is not a valid session. Only `Message` events are
/// counted, so timing/turn-end lines do not inflate the "msgs" total.
fn parse_entry(path: &Path) -> Option<SessionEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Header = serde_json::from_str(lines.next()?).ok()?;
    if !(SESSION_MIN_VERSION..=SESSION_VERSION).contains(&header.meta.version) {
        return None;
    }
    let count = lines
        .filter(|l| !l.is_empty())
        .filter(|l| {
            parse_event(l).is_ok_and(|ev| matches!(ev.kind, SessionEventKind::Message(_)))
        })
        .count();
    Some(SessionEntry {
        meta: header.meta,
        path: path.to_path_buf(),
        message_count: count,
    })
}

/// Write `contents` to `path` atomically: write a temp sibling, then rename.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::State("session path has no parent".into()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("session")
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_bytes())?;
        f.flush()?;
    }
    std::fs::rename(&tmp, path)?;
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
        let path = store.create(cwd, &"openai/gpt-5.6-sol:medium".into()).unwrap();
        let header = std::fs::read_to_string(&path).unwrap();
        assert!(
            header.contains("\"model\":{\"provider\":\"openai\",\"id\":\"gpt-5.6-sol\",\"thinking\":\"medium\"}"),
            "header model must be a raw object: {header}"
        );
        assert!(!header.contains("\"model\":\""), "header must not store a rendered model string: {header}");

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
            turn_end_line.contains("\"model\":{\"provider\":\"anthropic\",\"id\":\"claude\",\"thinking\":\"high\"}"),
            "turn-end model must be a raw object: {turn_end_line}"
        );
        assert!(!turn_end_line.contains("\"label\""), "turn-end must not carry a rendered label field: {turn_end_line}");
        assert!(!turn_end_line.contains("\"model\":\""), "turn-end must not store a rendered model string: {turn_end_line}");
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
                kind: SessionEventKind::ToolTiming { tool_call_id: "t1".into(), elapsed_ms: 5 },
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::TurnEnd {
                    model: "p/m:medium".into(),
                    elapsed_ms: 1234,
                    cost: 0.01,
                    usage: Usage { input_tokens: 10, output_tokens: 20, ..Usage::default() },
                },
            },
        ];
        append_events(&path, &mut batch, None).unwrap();
        let (_meta, events, _, _) = load(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0].kind, SessionEventKind::Message(m) if m.role == Role::User));
        assert!(matches!(&events[1].kind, SessionEventKind::ToolTiming { tool_call_id, elapsed_ms: 5 } if tool_call_id == "t1"));
        assert!(matches!(&events[2].kind, SessionEventKind::TurnEnd { model, elapsed_ms: 1234, cost, usage }
            if model.label() == "p/m:medium" && (*cost - 0.01).abs() < 1e-9 && usage.input_tokens == 10));
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
        assert!(matches!(&events[path_idx[0]].kind, SessionEventKind::Message(m) if m.role == Role::User));
        assert!(matches!(&events[path_idx[1]].kind, SessionEventKind::Message(m) if m.role == Role::User));
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
    }

    #[test]
    fn list_newest_first_and_counts() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/listy");
        let p1 = store.create(cwd, &"m".into()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let p2 = store.create(cwd, &"m".into()).unwrap();
        let mut batch = [ev(user("a"))];
        append_events(&p1, &mut batch, None).unwrap();
        let list = store.list_for_cwd(cwd).unwrap();
        assert_eq!(list.len(), 2);
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