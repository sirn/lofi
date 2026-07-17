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
use lofi_types::{Message, SessionEvent};
use serde::{Deserialize, Serialize};

/// Transcript format version. Bumped only on a breaking on-disk change;
/// older files are rejected (no migration yet — lofi has no shipped sessions
/// to migrate).
pub const SESSION_VERSION: u32 = 1;

/// Metadata written as the first JSONL line of every session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub version: u32,
    /// Unix milliseconds at creation time.
    pub created: u64,
    /// Absolute working directory the session belongs to.
    pub cwd: String,
    /// Model label (provider/id[:level]) active when the session started.
    pub model: String,
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
    pub fn create(&self, cwd: &Path, model: &str) -> Result<PathBuf> {
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
            model: model.to_string(),
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
    if header.meta.version != SESSION_VERSION {
        return Err(Error::State(format!(
            "unsupported session version {} in {}",
            header.meta.version,
            path.display()
        )));
    }
    let mut events = Vec::new();
    // Byte offset of each event's line in the file (parallel to `events`).
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
/// # Errors
/// Returns [`Error::Io`] on open/write failure or [`Error::State`] on a
/// serialization failure.
pub fn append_events(path: &Path, events: &[SessionEvent]) -> Result<(u64, u64)> {
    if events.is_empty() {
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        return Ok((len, len));
    }
    let byte_start = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
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
    serde_json::from_str::<Message>(line)
        .map(SessionEvent::Message)
        .map_err(|e| Error::State(format!("parse event: {e}")))
}



/// Parse a session file into a [`SessionEntry`] (metadata + message count),
/// or `None` if the file is not a valid session. Only `Message` events are
/// counted, so timing/turn-end lines do not inflate the "msgs" total.
fn parse_entry(path: &Path) -> Option<SessionEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let header: Header = serde_json::from_str(lines.next()?).ok()?;
    if header.meta.version != SESSION_VERSION {
        return None;
    }
    let count = lines
        .filter(|l| !l.is_empty())
        .filter(|l| {
            parse_event(l).map_or(false, |ev| matches!(ev, SessionEvent::Message(_)))
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

    use super::*;
    use lofi_types::{ContentBlock, Role, Usage};

    /// Wrap a message as a `Message` session event.
    fn ev(msg: Message) -> SessionEvent {
        SessionEvent::Message(msg)
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
        let path = store.create(cwd, "openai/gpt-5.6-sol").unwrap();
        let (meta, msgs, _, _) = load(&path).unwrap();
        assert_eq!(meta.version, SESSION_VERSION);
        assert_eq!(meta.cwd, "/tmp/project");
        assert_eq!(meta.model, "openai/gpt-5.6-sol");
        assert!(msgs.is_empty());
    }

    #[test]
    fn append_then_load_messages() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/proj2");
        let path = store.create(cwd, "openai/gpt-4o").unwrap();
        append_events(&path, &[ev(user("hello")), ev(assistant("hi there"))]).unwrap();
        let (_meta, events, _, _) = load(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], SessionEvent::Message(m) if m.role == Role::User));
        assert!(matches!(&events[1], SessionEvent::Message(m) if m.role == Role::Assistant));
    }

    #[test]
    fn events_round_trip_with_timings_and_cost() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/events");
        let path = store.create(cwd, "m").unwrap();
        append_events(
            &path,
            &[
                ev(user("hi")),
                SessionEvent::ToolTiming { id: "t1".into(), elapsed_ms: 5 },
                SessionEvent::TurnEnd {
                    label: "m · medium".into(),
                    elapsed_ms: 1234,
                    cost: 0.01,
                    usage: Usage { input_tokens: 10, output_tokens: 20, ..Usage::default() },
                },
            ],
        )
        .unwrap();
        let (_meta, events, _, _) = load(&path).unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], SessionEvent::Message(m) if m.role == Role::User));
        assert!(matches!(&events[1], SessionEvent::ToolTiming { id, elapsed_ms: 5 } if id == "t1"));
        assert!(matches!(&events[2], SessionEvent::TurnEnd { label, elapsed_ms: 1234, cost, usage }
            if label == "m · medium" && (*cost - 0.01).abs() < 1e-9 && usage.input_tokens == 10));
    }

    #[test]
    fn legacy_bare_message_lines_still_load() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/legacy");
        let path = store.create(cwd, "m").unwrap();
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
        assert!(matches!(&events[0], SessionEvent::Message(m) if m.role == Role::User));
    }

    #[test]
    fn list_newest_first_and_counts() {
        let (_guard, store) = isolated_store();
        let cwd = Path::new("/tmp/listy");
        let p1 = store.create(cwd, "m").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let p2 = store.create(cwd, "m").unwrap();
        append_events(&p1, &[ev(user("a"))]).unwrap();
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
        let _p1 = store.create(cwd, "m").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let p2 = store.create(cwd, "m").unwrap();
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