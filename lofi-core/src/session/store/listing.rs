use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use lofi_error::{Error, Result};
use lofi_types::RunModel;
use serde::Deserialize;

use super::cursor::{SessionCursor, SessionSnapshot};
use super::{now_ms, short_id, Header, SessionMeta, SESSION_VERSION};

#[derive(Debug, Clone)]
pub struct SessionFile {
    pub(super) path: PathBuf,
    // Read when the file was listed, so callers filter on it without a
    // second header read per file.
    meta: SessionMeta,
    last_active: std::time::SystemTime,
}

impl SessionFile {
    #[must_use]
    pub fn last_active(&self) -> std::time::SystemTime {
        self.last_active
    }

    /// Session id: the file stem (`<millis>_<random>`), matching
    /// `SessionEntry::id` without indexing the transcript.
    #[must_use]
    pub fn id(&self) -> String {
        self.path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(ToString::to_string)
            .unwrap_or_default()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn byte_len(&self) -> u64 {
        std::fs::metadata(&self.path).map_or(u64::MAX, |meta| meta.len())
    }

    #[must_use]
    pub fn quick_preview(&self) -> Option<String> {
        quick_entry_preview(&self.path)
    }

    /// The session meta header, captured when the file was listed.
    #[must_use]
    pub fn meta(&self) -> &SessionMeta {
        &self.meta
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
    pub(super) file: SessionFile,
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

impl SessionStore {
    /// Transcript headers of subagents spawned by the session `parent` in
    /// `cwd`, newest first. `/subagent` lists these beside the live registry
    /// so settled children survive eviction and restarts.
    ///
    /// # Errors
    /// Propagates directory scan failures.
    pub fn list_subagent_files_for_cwd(
        &self,
        cwd: &Path,
        parent: &str,
    ) -> Result<Vec<SessionFile>> {
        let mut files: Vec<SessionFile> = self
            .list_files_for_cwd(cwd)?
            .into_iter()
            .filter(|file| {
                let meta = file.meta();
                meta.subagent && meta.parent.as_deref() == Some(parent)
            })
            .collect();
        files.sort_by_key(|file| std::cmp::Reverse(file.meta().created));
        Ok(files)
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
        let store = Self { root: p };
        crate::state::ensure_private_dir(&store.root)?;
        Ok(store)
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
        self.create_cursor_marked(cwd, model, false, None, None)
    }

    /// A subagent transcript: recorded for resumability, but excluded from
    /// default session selection so it never becomes the `--continue` target.
    /// `run_id` is the registry id of the run; rehydration restores the entry
    /// under it so resume calls from the parent history survive a restart.
    /// # Errors
    /// Returns [`Error::Io`] on filesystem failure or [`Error::State`] on a
    /// header-serialization failure.
    pub fn create_subagent_cursor(
        &self,
        cwd: &Path,
        model: &RunModel,
        parent: Option<&str>,
        run_id: &str,
    ) -> Result<SessionCursor> {
        self.create_cursor_marked(cwd, model, true, parent, Some(run_id))
    }

    fn create_cursor_marked(
        &self,
        cwd: &Path,
        model: &RunModel,
        subagent: bool,
        parent: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<SessionCursor> {
        let path = self.create_marked(cwd, model, subagent, parent, run_id)?;
        let cursor = SessionCursor::new(path, None);
        *cursor.lock_compaction_index() = Some(Vec::new());
        Ok(cursor)
    }

    #[cfg(test)]
    pub(super) fn create(&self, cwd: &Path, model: &RunModel) -> Result<PathBuf> {
        self.create_marked(cwd, model, false, None, None)
    }

    pub(super) fn create_marked(
        &self,
        cwd: &Path,
        model: &RunModel,
        subagent: bool,
        parent: Option<&str>,
        run_id: Option<&str>,
    ) -> Result<PathBuf> {
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
                subagent,
                parent: parent.map(str::to_string),
                run_id: run_id.map(str::to_string),
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
            // Subagent transcripts exist for resumability; they are not user
            // sessions and stay out of the picker. Filter on the listed
            // header: inspect reads the whole file.
            if file.meta().subagent {
                continue;
            }
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
            files.push(SessionFile {
                path,
                meta,
                last_active,
            });
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.last_active));
        Ok(files)
    }
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

fn entry_text(ev: &EntryPreview) -> String {
    let mut text = String::new();
    for b in &ev.blocks {
        if b.kind == "text" && !b.text.is_empty() {
            text.push_str(&b.text);
        }
    }
    text
}

fn entry_preview(ev: &EntryPreview) -> String {
    match ev.kind.as_str() {
        "message" | "" => {
            let prefix = match ev.role.as_str() {
                "user" => "user: ",
                "assistant" => "agent: ",
                _ => return String::new(),
            };
            let text = entry_text(ev);
            if text.is_empty() {
                return String::new();
            }
            format!("{prefix}{}", one_line(&text))
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

fn entry_preview_slice(events: &[EntryPreview]) -> String {
    if let [single] = events {
        return entry_preview(single);
    }
    let mut text = String::new();
    for ev in events {
        for b in &ev.blocks {
            if b.kind == "text" && !b.text.is_empty() {
                text.push_str(&b.text);
            }
        }
        if text.contains('\n') || text.chars().count() > 80 {
            break;
        }
    }
    if text.is_empty() {
        String::new()
    } else {
        format!("agent: {}", one_line(&text))
    }
}

fn finish_assistant_chunks(chunks: &[String]) -> Option<String> {
    let mut combined = String::new();
    for chunk in chunks.iter().rev() {
        combined.push_str(chunk);
        if combined.contains('\n') || combined.chars().count() > 80 {
            break;
        }
    }
    (!combined.is_empty()).then(|| format!("agent: {}", one_line(&combined)))
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
    let tail_floor = len.saturating_sub(MAX_TAIL);
    let mut end = len;
    let mut carry: Vec<u8> = Vec::new();
    let mut assistant_chunks: Vec<String> = Vec::new();
    while end > tail_floor {
        let start = end.saturating_sub(CHUNK).max(tail_floor);
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut chunk = vec![0u8; usize::try_from(end - start).ok()?];
        file.read_exact(&mut chunk).ok()?;
        chunk.extend_from_slice(&carry);
        let lines_slice = if start > tail_floor {
            if let Some(first_nl) = chunk.iter().position(|byte| *byte == b'\n') {
                let complete = chunk.split_off(first_nl + 1);
                carry = chunk;
                complete
            } else {
                carry = chunk;
                end = start;
                continue;
            }
        } else {
            carry.clear();
            chunk
        };
        for line in lines_slice.rsplit(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_slice::<EntryPreview>(line) else {
                continue;
            };
            if matches!(event.kind.as_str(), "message" | "") && event.role == "assistant" {
                let text = entry_text(&event);
                if !text.is_empty() {
                    assistant_chunks.push(text);
                }
                continue;
            }
            if matches!(
                event.kind.as_str(),
                "cursor"
                    | "backend_usage"
                    | "tool_timing"
                    | "thinking_timing"
                    | "job_started"
                    | "job_finished"
            ) || (matches!(event.kind.as_str(), "message" | "") && event.role == "system")
            {
                continue;
            }
            if !assistant_chunks.is_empty() {
                return finish_assistant_chunks(&assistant_chunks);
            }
            let preview = entry_preview(&event);
            if !preview.is_empty() {
                return Some(preview);
            }
        }
        end = start;
    }
    finish_assistant_chunks(&assistant_chunks)
}

/// Derive the resume-pick entry (message count + last preview) from the shared
/// event index, resolving the selected lineage with the same helper compaction
/// uses instead of a dedicated per-transcript map. The picker runs this per
/// session at open; reusing `load_index` avoids the per-call map that kept
/// worker-arena pages mapped at the high-water mark.
fn parse_entry(path: &Path, last_active: std::time::SystemTime) -> Option<SessionEntry> {
    let (meta, message_count, last_message) =
        super::index::scan_entry_summary::<EntryPreview>(path, entry_preview_slice).ok()?;
    Some(SessionEntry {
        file: SessionFile {
            path: path.to_path_buf(),
            meta: meta.clone(),
            last_active,
        },
        meta,
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
