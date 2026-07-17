//! Built-in sandbox tools: `read`, `ls`, `find`, `grep`, `write`, `edit`,
//! `bash`. Each file tool resolves its path against a workspace root and
//! rejects escapes; `bash` is exempt (it shells out) but runs with its cwd
//! pinned to the root.
//!
//! These are plain async methods on [`BuiltinTools`]; the code-mode sandbox
//! (see [`crate::code`]) binds them directly onto the guest `lofi` object
//! rather than going through a trait dispatch. The workspace root is held
//! canonicalized so `starts_with` checks are reliable after `..` traversal.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use globset::Glob;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::error::{Error, Result};

/// Default `bash` timeout in milliseconds (120s).
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;

/// Per-stream byte cap for captured `bash` output. Prevents a runaway
/// command from exhausting memory before the wall-clock timeout fires.
const MAX_BASH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum bytes returned by `read` before truncation.
const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
/// Maximum file size accepted by `edit`; larger files are rejected before the
/// replacement is built so a huge target cannot exhaust memory.
const MAX_EDIT_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of paths `find` returns before truncation.
const MAX_FIND_RESULTS: usize = 4096;
/// Maximum bytes of grep output before truncation.
const MAX_GREP_OUTPUT_BYTES: usize = 1024 * 1024;
/// Files larger than this are skipped by `grep` to bound memory.
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Default match cap for `grep` when the caller omits `max`.
const DEFAULT_GREP_MAX: usize = 1000;
/// Maximum filesystem entries `find` traverses before signaling truncation.
const MAX_FIND_VISITED: usize = 65_536;
/// Maximum filesystem entries `grep` scans before signaling truncation.
const MAX_GREP_VISITED: usize = 65_536;

/// The builtin tool bundle.
///
/// Holds the workspace root (canonicalized in [`new`](Self::new)) so every
/// file operation can be confined to it. Methods are async and return JSON
/// values ready to hand back to the sandbox.
#[derive(Debug, Clone)]
pub struct BuiltinTools {
    root: PathBuf,
}

impl BuiltinTools {
    /// Construct a new bundle rooted at `root`.
    ///
    /// `root` is canonicalized on construction; if that fails (the directory
    /// does not yet exist) the original path is kept and path checks fall
    /// back to lexical resolution.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        Self { root }
    }

    /// The canonicalized workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Read a file under the root as a UTF-8 string.
    ///
    /// The blocking read runs on `spawn_blocking` so a huge file can't freeze
    /// the TUI event loop, and is truncated at [`MAX_READ_BYTES`].
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the root or the file
    /// cannot be read.
    pub async fn read(&self, path: &str) -> Result<Value> {
        reject_symlink_leaf(&self.root, path, &format!("read {path}"))?;
        let resolved = resolve_under(&self.root, path)?;
        reject_non_regular(&format!("read {path}"), &resolved)?;
        let label = path.to_string();
        let text = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
            use std::io::Read as _;
            // Stream at most MAX+1 bytes so truncation is detectable without
            // reading an entire huge file into memory.
            let file = std::fs::File::open(&resolved)?;
            let mut buf = Vec::new();
            file.take(MAX_READ_BYTES as u64 + 1).read_to_end(&mut buf)?;
            let truncated = buf.len() > MAX_READ_BYTES;
            let slice = if truncated {
                &buf[..MAX_READ_BYTES]
            } else {
                &buf[..]
            };
            let mut text = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                text.push_str("\n<output truncated>");
            }
            Ok(text)
        })
        .await
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("read {label}: {e}")))?;
        Ok(json!(text))
    }

    /// List directory entries under `dir` (empty/`.` means the root).
    ///
    /// Entries are returned as paths relative to the root, sorted,
    /// newline-joined.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root.
    #[allow(clippy::unused_async)]
    pub async fn ls(&self, dir: &str) -> Result<Value> {
        let resolved = resolve_under(&self.root, dir)?;
        let root = self.root.clone();
        let label = dir.to_string();
        let entries =
            tokio::task::spawn_blocking(move || -> std::io::Result<(Vec<String>, bool)> {
                let mut entries = Vec::new();
                // Collect one past the cap so truncation is detectable without
                // traversing the whole (potentially huge) directory.
                for entry in std::fs::read_dir(&resolved)? {
                    let entry = entry?;
                    let rel = entry
                        .path()
                        .strip_prefix(&root)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    entries.push(rel);
                    if entries.len() > MAX_FIND_RESULTS {
                        break;
                    }
                }
                let truncated = entries.len() > MAX_FIND_RESULTS;
                if truncated {
                    entries.truncate(MAX_FIND_RESULTS);
                }
                entries.sort();
                Ok((entries, truncated))
            })
            .await
            .map_err(|e| Error::Tool(format!("ls {label}: {e}")))?
            .map_err(|e| Error::Tool(format!("ls {label}: {e}")))?;
        let mut out = entries.0.join("\n");
        if entries.1 {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("<truncated>");
        }
        Ok(json!(out))
    }

    /// Recursively find files under `dir` (default root) matching `glob`.
    ///
    /// Returns newline-joined relative paths, sorted.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `dir` escapes the root or the glob is
    /// invalid.
    #[allow(clippy::unused_async)]
    pub async fn find(&self, glob: &str, dir: Option<&str>) -> Result<Value> {
        let base = resolve_under(&self.root, dir.unwrap_or(""))?;
        let matcher = Glob::new(glob)
            .map_err(|e| Error::Tool(format!("invalid glob {glob:?}: {e}")))?
            .compile_matcher();
        let root = self.root.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String> {
            let mut hits = Vec::new();
            let mut visited = 0usize;
            // Match during traversal and stop once we have enough hits, so a
            // matching file is not missed because an earlier non-matching
            // region filled a candidate cap. A separate visited cap bounds
            // runtime; `truncated` signals the result is incomplete.
            let truncated = find_walk(
                &base,
                &root,
                &matcher,
                &mut hits,
                MAX_FIND_RESULTS,
                &mut visited,
                MAX_FIND_VISITED,
            )?;
            hits.sort();
            let mut out = hits.join("\n");
            if truncated {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("<truncated>");
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Tool(format!("find {glob}: {e}")))??;
        Ok(json!(out))
    }

    /// Grep files under `path` (default root, recursive) for `pattern`.
    ///
    /// `pattern` is either a string or an object `{ regex, ic?, ctx?, max? }`.
    /// Matching lines are emitted as `file:line:content`; `ctx` surrounding
    /// lines (if requested) get the same prefix and groups are separated by
    /// `--`.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] on an invalid pattern, an escaped path, or a
    /// read failure.
    #[allow(clippy::unused_async)]
    pub async fn grep(&self, pattern: Value, path: Option<&str>) -> Result<Value> {
        let (re_src, ic, ctx, max) = parse_grep_args(pattern)?;
        let mut builder = regex::RegexBuilder::new(&re_src);
        builder.case_insensitive(ic);
        let re = builder
            .build()
            .map_err(|e| Error::Tool(format!("invalid regex {re_src:?}: {e}")))?;
        let base = resolve_under(&self.root, path.unwrap_or(""))?;
        let root = self.root.clone();
        let out = tokio::task::spawn_blocking(move || -> Result<String> {
            // A file path scans just that file; a directory recurses with a
            // visited cap so a huge tree can't exhaust memory or hang.
            let mut files = Vec::new();
            let mut visited = 0usize;
            let truncated_walk = if base.is_file() {
                files.push(base.clone());
                false
            } else {
                walk_files_capped(&base, &mut files, &mut visited, MAX_GREP_VISITED)?
            };
            files.sort();

            let budget = MAX_GREP_OUTPUT_BYTES;
            let mut out = String::new();
            let mut emitted = 0usize;
            let max = if max == 0 { DEFAULT_GREP_MAX } else { max };
            let mut truncated_output = false;
            // Append a line only if it fits the remaining output budget;
            // otherwise mark truncated and stop. This bounds total output to
            // `budget` plus at most one line length, rather than overshooting
            // by a multi-MiB context group.
            let mut push_line = |out: &mut String, s: &str| -> bool {
                if out.len() + s.len() > budget {
                    truncated_output = true;
                    false
                } else {
                    out.push_str(s);
                    true
                }
            };
            'outer: for file in &files {
                // Skip oversized files so a single huge artifact can't blow
                // memory by being fully read into a String.
                let Ok(meta) = std::fs::metadata(file) else {
                    continue;
                };
                if meta.len() > MAX_GREP_FILE_BYTES {
                    continue;
                }
                let rel = file
                    .strip_prefix(&root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let Ok(text) = std::fs::read_to_string(file) else {
                    continue;
                };
                let lines: Vec<&str> = text.lines().collect();
                let mut i = 0;
                let mut last_group_end: Option<usize> = None;
                while i < lines.len() {
                    if re.is_match(lines[i]) {
                        let start = i.saturating_sub(ctx);
                        let end = i.saturating_add(ctx).min(lines.len().saturating_sub(1));
                        if let Some(prev_end) = last_group_end {
                            if prev_end + 1 < start && !push_line(&mut out, "--\n") {
                                break 'outer;
                            }
                        }
                        for (j, line) in lines.iter().enumerate().take(end + 1).skip(start) {
                            let rendered = format!("{}:{}:{}\n", rel, j + 1, line);
                            if !push_line(&mut out, &rendered) {
                                break 'outer;
                            }
                        }
                        last_group_end = Some(end);
                        emitted += 1;
                        if emitted >= max {
                            break 'outer;
                        }
                    }
                    i += 1;
                }
            }
            if truncated_output {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("<output truncated>");
            } else if truncated_walk {
                // The file list was truncated; signal that the search was
                // incomplete even when some matches were found.
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("<truncated>");
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Tool(format!("grep {re_src}: {e}")))??;
        Ok(json!(out))
    }

    /// Write `text` to `path`, creating parent directories as needed.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `path` escapes the root or the write fails.
    #[allow(clippy::unused_async)]
    pub async fn write(&self, args: Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("write: missing 'path'".into()))?
            .to_owned();
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("write: missing 'text'".into()))?
            .to_owned();
        reject_symlink_leaf(&self.root, &path, &format!("write {path}"))?;
        let resolved = resolve_under(&self.root, &path)?;
        reject_non_regular(&format!("write {path}"), &resolved)?;
        let label = path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&resolved, text)?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?
        .map_err(|e| Error::Tool(format!("write {label}: {e}")))?;
        Ok(json!({ "ok": true }))
    }

    /// Replace the single occurrence of `old` with `new` in `path`.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if `path` escapes the root, if `old` is absent,
    /// or if `old` occurs more than once (an ambiguous edit).
    #[allow(clippy::unused_async)]
    pub async fn edit(&self, args: Value) -> Result<Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("edit: missing 'path'".into()))?
            .to_owned();
        let old = args
            .get("old")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("edit: missing 'old'".into()))?
            .to_owned();
        let new = args
            .get("new")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("edit: missing 'new'".into()))?
            .to_owned();
        reject_symlink_leaf(&self.root, &path, &format!("edit {path}"))?;
        let resolved = resolve_under(&self.root, &path)?;
        reject_non_regular(&format!("edit {path}"), &resolved)?;
        let label = path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            use std::io::Read as _;
            let file = std::fs::File::open(&resolved)
                .map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            let mut buf = Vec::new();
            file.take(MAX_EDIT_BYTES as u64 + 1)
                .read_to_end(&mut buf)
                .map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            if buf.len() > MAX_EDIT_BYTES {
                return Err(Error::Tool(format!(
                    "edit {label}: file exceeds {MAX_EDIT_BYTES} bytes"
                )));
            }
            let content =
                String::from_utf8(buf).map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            let count = content.matches(&old).count();
            if count == 0 {
                return Err(Error::Tool(format!("edit {label}: 'old' not found")));
            }
            if count > 1 {
                return Err(Error::Tool(format!(
                    "edit {label}: 'old' found {count} times; expected exactly one"
                )));
            }
            let updated = content.replacen(&old, &new, 1);
            std::fs::write(&resolved, updated)
                .map_err(|e| Error::Tool(format!("edit {label}: {e}")))?;
            Ok(())
        })
        .await
        .map_err(|e| Error::Tool(format!("edit {path}: {e}")))??;
        Ok(json!({ "ok": true }))
    }

    /// Run `cmd` via `sh -c` with cwd pinned to the root.
    ///
    /// stdout and stderr are merged. `timeoutMs` bounds the run (default
    /// 120s); on timeout the child is killed and `{ ok: false, output:
    /// "<timeout>", code: null }` is returned.
    ///
    /// # Errors
    /// Returns [`Error::Io`] only if the process cannot be spawned.
    pub async fn bash(&self, args: Value) -> Result<Value> {
        let cmd = args
            .get("cmd")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("bash: missing 'cmd'".into()))?
            .to_owned();
        let timeout_ms = args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_BASH_TIMEOUT_MS);
        let dur = Duration::from_millis(timeout_ms);

        // `bash` is intentionally host-level (mirrors Pi's `lofi.bash`): it
        // runs the user's project commands and is *not* a security sandbox.
        // The two real risks a model-controlled shell poses here — leaking
        // inherited credentials and leaving orphans on timeout — are handled
        // below: secret-like env vars are scrubbed from the child, and the
        // child runs in its own process group (`process_group(0)`) so a
        // timeout or cancellation can kill the *entire* tree — background
        // children and grandchildren included — rather than just the `sh`
        // leader. The `PgrpKillGuard` makes that robust against early return
        // or future cancellation.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(&cmd)
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for (k, _) in std::env::vars() {
            if looks_secret(&k) {
                command.env_remove(&k);
            }
        }

        let mut child = command.spawn()?;
        // `process_group(0)` makes the child its own session/group leader,
        // so its pid is the process-group id. Killing `-pgid` reaches every
        // descendant the shell spawned.
        let mut guard = PgrpKillGuard::new(child.id());
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tool("bash: stdout pipe unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Tool("bash: stderr pipe unavailable".into()))?;

        let result = tokio::time::timeout(
            dur,
            Box::pin(async {
                // Read both pipes AND wait for the child inside the timeout,
                // so a command that closes its pipes early but keeps running
                // (e.g. `exec >/dev/null 2>&1; sleep 1000`) can't outlive
                // `timeoutMs` by deferring the wait past the timed future.
                let (out, err, status) = tokio::try_join!(
                    read_capped(&mut stdout, MAX_BASH_OUTPUT_BYTES),
                    read_capped(&mut stderr, MAX_BASH_OUTPUT_BYTES),
                    child.wait(),
                )?;
                Ok::<_, std::io::Error>((out, err, status))
            }),
        )
        .await;

        match result {
            Ok(Ok((out, err, status))) => {
                // The child exited and was reaped inside the timed future;
                // disarm the guard so the completed group isn't signaled.
                guard.disarm();
                let (out_bytes, out_truncated) = out;
                let (err_bytes, err_truncated) = err;
                let mut merged = out_bytes;
                merged.extend_from_slice(&err_bytes);
                let mut output = String::from_utf8_lossy(&merged).into_owned();
                if out_truncated || err_truncated {
                    output.push_str("\n<output truncated>");
                }
                Ok(json!({
                    "ok": status.success(),
                    "output": output,
                    "code": status.code(),
                }))
            }
            Ok(Err(e)) => {
                // A read error may leave the child running; kill the whole
                // group and reap before surfacing the error so we don't
                // orphan the shell or its descendants.
                drop(guard);
                let _ = child.wait().await;
                Err(Error::Io(e))
            }
            Err(_) => {
                // Timeout: drop the guard to SIGKILL the whole process group, then
                // reap the leader so we don't leave a zombie.
                drop(guard);
                let _ = child.wait().await;
                Ok(json!({
                    "ok": false,
                    "output": "<timeout>",
                    "code": Value::Null,
                }))
            }
        }
    }
}

/// RAII guard that SIGKILLs a child's process group on drop unless disarmed.
///
/// `bash` runs the child in its own process group (`process_group(0)`); on
/// timeout or future cancellation dropping this guard kills every descendant
/// the shell spawned, not just the `sh` leader. Disarm once the child has been
/// reaped normally.
pub(crate) struct PgrpKillGuard {
    pid: Option<u32>,
}

impl PgrpKillGuard {
    pub(crate) fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }
    pub(crate) fn disarm(&mut self) {
        self.pid = None;
    }
}

#[allow(clippy::cast_possible_wrap)]
impl Drop for PgrpKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            // `kill(-pgid, SIGKILL)` signals the whole process group. The
            // child was made group leader by `process_group(0)`, so its pid
            // is the group id. `nix` wraps the FFI behind a safe API.
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-(pid as i32)),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
    }
}

/// Heuristic for env vars that carry credentials and should not be inherited
/// by model-controlled shell commands.
fn looks_secret(name: &str) -> bool {
    let u = name.to_ascii_uppercase();
    u.contains("API_KEY")
        || u.contains("SECRET")
        || u.contains("PASSWORD")
        || u.contains("CREDENTIAL")
        || u.contains("_TOKEN")
        || u == "TOKEN"
}

/// Read up to `cap` bytes from `r` into a buffer, then drain any remainder
/// to EOF (without storing it) so a full pipe can't deadlock the child. The
/// caller is told whether truncation occurred.
pub(crate) async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        let room = cap.saturating_sub(buf.len());
        if room == 0 {
            drain(r).await?;
            return Ok((buf, true));
        }
        let take = n.min(room);
        buf.extend_from_slice(&tmp[..take]);
        if take < n {
            drain(r).await?;
            return Ok((buf, true));
        }
    }
    Ok((buf, false))
}

async fn drain<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> std::io::Result<()> {
    let mut tmp = [0u8; 8192];
    loop {
        let n = r.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
    }
    Ok(())
}

/// Resolve `p` against `root`, rejecting escapes.
///
/// If the target exists, it is canonicalized directly. If not (the common
/// `write` case where the leaf does not yet exist), the parent is
/// canonicalized and the leaf filename is re-joined. The result must start
/// with `root` or the call fails as a path-escape.
fn resolve_under(root: &Path, p: &str) -> Result<PathBuf> {
    use std::path::Component;
    // Reject any component that could escape the root lexically — parent
    // references, absolute roots, and Windows prefixes — before touching the
    // filesystem. `..` is never needed for a workspace-relative tool path,
    // and allowing it would let a non-existent tail (e.g. `new/../../out`)
    // bypass the canonicalization check by collapsing to an ancestor that
    // `starts_with(root)` while the re-joined path resolves outside it.
    let path = Path::new(p);
    for c in path.components() {
        match c {
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(Error::Tool(format!("path escapes workspace root: {p}")));
            }
            Component::CurDir | Component::Normal(_) => {}
        }
    }
    let joined = if p.is_empty() || p == "." {
        root.to_path_buf()
    } else {
        root.join(p)
    };
    // Fast path: the target exists, so canonicalize directly.
    if let Ok(c) = joined.canonicalize() {
        if !c.starts_with(root) {
            return Err(Error::Tool(format!("path escapes workspace root: {p}")));
        }
        return Ok(c);
    }
    // Slow path: some tail of the path does not exist yet (the common
    // `write` case where neither the file nor its directory exists). Walk
    // up to the nearest existing ancestor, canonicalize that, then re-join
    // the missing components — checking `starts_with(root)` at each step so
    // a `..` in the tail cannot escape after the ancestor check passes.
    let mut existing = joined.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| Error::Tool(format!("invalid path: {p}")))?
            .to_owned();
        tail.push(name);
        existing = existing
            .parent()
            .ok_or_else(|| Error::Tool(format!("invalid path: {p}")))?
            .to_path_buf();
    }
    let canon = existing
        .canonicalize()
        .map_err(|_| Error::Tool(format!("cannot resolve parent: {p}")))?;
    if !canon.starts_with(root) {
        return Err(Error::Tool(format!("path escapes workspace root: {p}")));
    }
    let mut resolved = canon;
    for name in tail.into_iter().rev() {
        resolved.push(name);
        if !resolved.starts_with(root) {
            return Err(Error::Tool(format!("path escapes workspace root: {p}")));
        }
    }
    Ok(resolved)
}

/// Reject `path` if it exists but is not a regular file (FIFO, socket,
/// device) or is a symlink. Rejecting symlinks with no-follow semantics stops
/// a dangling leaf symlink whose target escapes the workspace from being
/// followed by a later `write`. A nonexistent target is allowed so `write`
/// can create new files. `symlink_metadata` never blocks.
fn reject_non_regular(label: &str, path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(Error::Tool(format!("{label}: not a regular file")));
        }
    }
    Ok(())
}

/// Reject a leaf symlink at the un-canonicalized `root/p` before
/// [`resolve_under`] follows it. `resolve_under` canonicalizes existing paths,
/// so a symlink to a regular file inside the workspace would otherwise be
/// observed only as its target and silently followed.
fn reject_symlink_leaf(root: &Path, p: &str, label: &str) -> Result<()> {
    let leaf = root.join(p);
    if std::fs::symlink_metadata(&leaf).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::Tool(format!("{label}: symlink not allowed")));
    }
    Ok(())
}

/// Recursively collect files under `dir`.
///
/// Symlinks are skipped so a link pointing outside the workspace root can't
/// smuggle files into `find`/`grep` results.
/// Walk `dir` recursively, collecting regular-file paths into `out`.
/// Every `read_dir` entry (files, directories, skipped specials) increments
/// `visited`, and the traversal stops at `max_visited` entries so a tree of
/// many empty directories can't make the walk effectively unbounded.
/// Non-regular entries (FIFOs, sockets, devices) are skipped so a special
/// file can't block in a later read. Returns `true` when truncated.
fn walk_files_capped(
    dir: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut usize,
    max_visited: usize,
) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(dir)? {
        if *visited >= max_visited {
            return Ok(true);
        }
        *visited += 1;
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        if ft.is_dir() {
            if walk_files_capped(&entry.path(), out, visited, max_visited)? {
                return Ok(true);
            }
        } else if ft.is_file() {
            out.push(entry.path());
        }
    }
    Ok(false)
}

/// Traverse `dir` applying `matcher` to each regular file's path relative to
/// `root`, collecting matching relative paths into `hits`. Every `read_dir`
/// entry increments `visited`; the traversal stops at `max_hits` matches or
/// `max_visited` entries. Returns `true` when truncated.
fn find_walk(
    dir: &Path,
    root: &Path,
    matcher: &globset::GlobMatcher,
    hits: &mut Vec<String>,
    max_hits: usize,
    visited: &mut usize,
    max_visited: usize,
) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    for entry in std::fs::read_dir(dir)? {
        if hits.len() >= max_hits || *visited >= max_visited {
            return Ok(true);
        }
        *visited += 1;
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            if find_walk(&path, root, matcher, hits, max_hits, visited, max_visited)? {
                return Ok(true);
            }
        } else if ft.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel = rel.to_string_lossy().into_owned();
                if matcher.is_match(&rel) {
                    hits.push(rel);
                }
            }
        }
    }
    Ok(false)
}

/// Parse the `grep` pattern argument into (regex, ignore-case, ctx, max).
fn parse_grep_args(pattern: Value) -> Result<(String, bool, usize, usize)> {
    match pattern {
        Value::String(s) => Ok((s, false, 0, 0)),
        Value::Object(_) => {
            let re_src = pattern
                .get("regex")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool("grep: missing 'regex'".into()))?
                .to_owned();
            let ic = pattern.get("ic").and_then(Value::as_bool).unwrap_or(false);
            let ctx = usize::try_from(pattern.get("ctx").and_then(Value::as_u64).unwrap_or(0))
                .unwrap_or(0);
            let max = usize::try_from(pattern.get("max").and_then(Value::as_u64).unwrap_or(0))
                .unwrap_or(0);
            Ok((re_src, ic, ctx, max))
        }
        _ => Err(Error::Tool("grep: pattern must be string or object".into())),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn tools() -> (tempfile::TempDir, BuiltinTools) {
        let dir = tempdir().unwrap();
        let tools = BuiltinTools::new(dir.path().to_path_buf());
        (dir, tools)
    }

    #[tokio::test]
    async fn read_file() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "hello").unwrap();
        let v = tools.read("a.txt").await.unwrap();
        assert_eq!(v, json!("hello"));
    }

    #[tokio::test]
    async fn read_escape_rejected() {
        let (_dir, tools) = tools();
        let err = tools.read("../escape").await.unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn parent_dir_in_nonexistent_tail_rejected() {
        // A `..` after a not-yet-created component must not be collapsible
        // into an ancestor that passes the root check.
        let (_dir, tools) = tools();
        let err = tools
            .write(json!({ "path": "new/../../escape.txt", "text": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
        assert!(!std::path::Path::new("escape.txt").exists());
    }

    #[tokio::test]
    async fn absolute_path_rejected() {
        let (_dir, tools) = tools();
        let err = tools.read("/etc/passwd").await.unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn ls_lists_entries() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("b.txt"), "b").unwrap();
        std::fs::create_dir(tools.root().join("sub")).unwrap();
        std::fs::write(tools.root().join("sub/c.txt"), "c").unwrap();
        let v = tools.ls("").await.unwrap();
        let lines: Vec<&str> = v.as_str().unwrap().lines().collect();
        assert!(lines.contains(&"b.txt"));
        assert!(lines.contains(&"sub"));
    }

    #[tokio::test]
    async fn find_glob() {
        let (_dir, tools) = tools();
        std::fs::create_dir_all(tools.root().join("src")).unwrap();
        std::fs::write(tools.root().join("src/a.rs"), "").unwrap();
        std::fs::write(tools.root().join("src/b.txt"), "").unwrap();
        std::fs::write(tools.root().join("root.rs"), "").unwrap();
        let v = tools.find("**/*.rs", None).await.unwrap();
        let lines: Vec<&str> = v.as_str().unwrap().lines().collect();
        assert!(lines.contains(&"root.rs"));
        assert!(lines.contains(&"src/a.rs"));
        assert!(!lines.contains(&"src/b.txt"));
    }

    #[tokio::test]
    async fn grep_basic_and_case_insensitive() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "Foo\nbar\nFOO\n").unwrap();
        let v = tools.grep(json!("FOO"), None).await.unwrap();
        assert!(v.as_str().unwrap().contains("a.txt:3:FOO"));
        assert!(!v.as_str().unwrap().contains("a.txt:1:Foo"));

        let v = tools
            .grep(json!({ "regex": "foo", "ic": true }), None)
            .await
            .unwrap();
        let s = v.as_str().unwrap();
        assert!(s.contains("a.txt:1:Foo"));
        assert!(s.contains("a.txt:3:FOO"));
    }

    #[tokio::test]
    async fn grep_object_with_context_and_max() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "l1\nl2\nMATCH\nl4\nl5\n").unwrap();
        let v = tools
            .grep(json!({ "regex": "MATCH", "ctx": 1, "max": 1 }), None)
            .await
            .unwrap();
        let s = v.as_str().unwrap();
        assert!(s.contains("a.txt:2:l2"));
        assert!(s.contains("a.txt:3:MATCH"));
        assert!(s.contains("a.txt:4:l4"));
    }

    #[tokio::test]
    async fn grep_single_file_path() {
        // A regular-file `path` scans just that file instead of silently
        // returning nothing (the directory walk returns early on a file).
        let (_dir, tools) = tools();
        std::fs::create_dir_all(tools.root().join("sub")).unwrap();
        std::fs::write(tools.root().join("sub/a.txt"), "alpha\n").unwrap();
        std::fs::write(tools.root().join("sub/b.txt"), "beta\n").unwrap();
        let v = tools.grep(json!("alpha"), Some("sub/a.txt")).await.unwrap();
        let s = v.as_str().unwrap();
        assert!(s.contains("sub/a.txt:1:alpha"));
        assert!(!s.contains("b.txt"));
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let (_dir, tools) = tools();
        let v = tools
            .write(json!({ "path": "nested/x.txt", "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(v, json!({ "ok": true }));
        let v = tools.read("nested/x.txt").await.unwrap();
        assert_eq!(v, json!("hi"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_write_edit_reject_leaf_symlink() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("real.txt"), "payload").unwrap();
        // A valid in-workspace leaf symlink must be rejected (no-follow),
        // not silently followed to its target.
        symlink("real.txt", tools.root().join("link.txt")).unwrap();
        let err = tools.read("link.txt").await.unwrap_err();
        assert!(err.to_string().contains("symlink"));
        let err = tools
            .write(json!({ "path": "link.txt", "text": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink"));
        let err = tools
            .edit(json!({ "path": "link.txt", "old": "a", "new": "b" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_rejects_dangling_symlink() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        // A dangling leaf symlink must not be followed (which could create
        // its target, potentially outside the workspace).
        symlink("/nonexistent/elsewhere", tools.root().join("dangling")).unwrap();
        let err = tools
            .write(json!({ "path": "dangling", "text": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symlink"));
    }

    #[tokio::test]
    async fn write_escape_rejected() {
        let (_dir, tools) = tools();
        let err = tools
            .write(json!({ "path": "../escape", "text": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn edit_success() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "alpha beta gamma").unwrap();
        let v = tools
            .edit(json!({ "path": "a.txt", "old": "beta", "new": "BETA" }))
            .await
            .unwrap();
        assert_eq!(v, json!({ "ok": true }));
        let v = tools.read("a.txt").await.unwrap();
        assert_eq!(v, json!("alpha BETA gamma"));
    }

    #[tokio::test]
    async fn edit_error_zero_occurrences() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "alpha beta").unwrap();
        let err = tools
            .edit(json!({ "path": "a.txt", "old": "zzz", "new": "y" }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn edit_error_multiple_occurrences() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "x x x").unwrap();
        let err = tools
            .edit(json!({ "path": "a.txt", "old": "x", "new": "y" }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn edit_escape_rejected() {
        let (_dir, tools) = tools();
        let err = tools
            .edit(json!({ "path": "../escape", "old": "a", "new": "b" }))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn bash_echo() {
        let (_dir, tools) = tools();
        let v = tools.bash(json!({ "cmd": "echo hello" })).await.unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["code"], json!(0));
        assert!(v["output"].as_str().unwrap().contains("hello"));
    }

    #[tokio::test]
    async fn bash_nonzero_exit() {
        let (_dir, tools) = tools();
        let v = tools.bash(json!({ "cmd": "false" })).await.unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_ne!(v["code"], json!(0));
    }

    #[tokio::test]
    async fn bash_timeout() {
        let (_dir, tools) = tools();
        let v = tools
            .bash(json!({ "cmd": "sleep 5", "timeoutMs": 50 }))
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["output"], json!("<timeout>"));
        assert_eq!(v["code"], Value::Null);
    }

    #[tokio::test]
    async fn bash_timeout_kills_process_group() {
        // A background child that outlives the timed-out shell must be killed
        // with the process group, not orphaned to write its marker afterward.
        let (_dir, tools) = tools();
        let marker = tools.root().join("late_marker");
        let cmd = format!("(sleep 1; echo x > {}) & wait", marker.display());
        let v = tools
            .bash(json!({ "cmd": cmd, "timeoutMs": 100 }))
            .await
            .unwrap();
        assert_eq!(v["output"], json!("<timeout>"));
        // Give the background child enough time to have written the marker if
        // it had survived the group kill.
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(
            !marker.exists(),
            "background child survived timeout; process group was not killed"
        );
    }
}
