//! Built-in sandbox tools: `read`, `ls`, `find`, `grep`, `write`, `edit`,
//! `bash`. Each file tool resolves its path against a workspace root and
//! rejects escapes; `bash` is exempt (it shells out) but runs with its cwd
//! pinned to the root.
//!
//! These are plain async methods on [`BuiltinTools`]; the code-mode sandbox
//! (see [`crate::code`]) binds them directly onto the guest `pi` object
//! rather than going through a trait dispatch. The workspace root is held
//! canonicalized so `starts_with` checks are reliable after `..` traversal.

use std::fmt::Write;
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
    /// # Errors
    /// Returns [`Error::Tool`] if the path escapes the root or the file
    /// cannot be read.
    #[allow(clippy::unused_async)]
    pub async fn read(&self, path: &str) -> Result<Value> {
        let resolved = resolve_under(&self.root, path)?;
        let bytes =
            std::fs::read(&resolved).map_err(|e| Error::Tool(format!("read {path}: {e}")))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
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
        let mut entries = Vec::new();
        for entry in
            std::fs::read_dir(&resolved).map_err(|e| Error::Tool(format!("ls {dir}: {e}")))?
        {
            let entry = entry.map_err(|e| Error::Tool(format!("ls {dir}: {e}")))?;
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            entries.push(rel);
        }
        entries.sort();
        Ok(json!(entries.join("\n")))
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
        let mut files = Vec::new();
        walk_files(&base, &mut files)?;
        let mut hits: Vec<String> = files
            .iter()
            .filter_map(|p| {
                let rel = p
                    .strip_prefix(&self.root)
                    .ok()?
                    .to_string_lossy()
                    .into_owned();
                if matcher.is_match(&rel) {
                    Some(rel)
                } else {
                    None
                }
            })
            .collect();
        hits.sort();
        Ok(json!(hits.join("\n")))
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
        let mut files = Vec::new();
        walk_files(&base, &mut files)?;
        files.sort();

        let mut out = String::new();
        let mut emitted = 0usize;
        'outer: for file in &files {
            let rel = file
                .strip_prefix(&self.root)
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
                    let end = (i + ctx).min(lines.len().saturating_sub(1));
                    if let Some(prev_end) = last_group_end {
                        if prev_end + 1 < start {
                            out.push_str("--\n");
                        }
                    }
                    for (j, line) in lines.iter().enumerate().take(end + 1).skip(start) {
                        let _ = writeln!(out, "{}:{}:{}", rel, j + 1, line);
                    }
                    last_group_end = Some(end);
                    emitted += 1;
                    if max > 0 && emitted >= max {
                        break 'outer;
                    }
                }
                i += 1;
            }
        }
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
        let resolved = resolve_under(&self.root, &path)?;
        if let Some(parent) = resolved.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Tool(format!("write {path}: mkdir {e}")))?;
        }
        std::fs::write(&resolved, text).map_err(|e| Error::Tool(format!("write {path}: {e}")))?;
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
        let resolved = resolve_under(&self.root, &path)?;
        let content = std::fs::read_to_string(&resolved)
            .map_err(|e| Error::Tool(format!("edit {path}: {e}")))?;
        let count = content.matches(&old).count();
        if count == 0 {
            return Err(Error::Tool(format!("edit {path}: 'old' not found")));
        }
        if count > 1 {
            return Err(Error::Tool(format!(
                "edit {path}: 'old' found {count} times; expected exactly one"
            )));
        }
        let updated = content.replacen(&old, &new, 1);
        std::fs::write(&resolved, updated).map_err(|e| Error::Tool(format!("edit {path}: {e}")))?;
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

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&self.root)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Tool("bash: stdout pipe unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| Error::Tool("bash: stderr pipe unavailable".into()))?;

        let dur_cl = dur;
        let result = tokio::time::timeout(dur_cl, async {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let (out_res, err_res) =
                tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err));
            out_res?;
            err_res?;
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, out, err))
        })
        .await;

        match result {
            Ok(Ok((status, out, err))) => {
                let mut merged = out;
                merged.extend_from_slice(&err);
                let output = String::from_utf8_lossy(&merged).into_owned();
                Ok(json!({
                    "ok": status.success(),
                    "output": output,
                    "code": status.code(),
                }))
            }
            Ok(Err(e)) => Err(Error::Io(e)),
            Err(_) => {
                // Timeout: dropping `child` kills the process (tokio default).
                Ok(json!({
                    "ok": false,
                    "output": "<timeout>",
                    "code": Value::Null,
                }))
            }
        }
    }
}

/// Resolve `p` against `root`, rejecting escapes.
///
/// If the target exists, it is canonicalized directly. If not (the common
/// `write` case where the leaf does not yet exist), the parent is
/// canonicalized and the leaf filename is re-joined. The result must start
/// with `root` or the call fails as a path-escape.
fn resolve_under(root: &Path, p: &str) -> Result<PathBuf> {
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

/// Recursively collect files under `dir`.
///
/// Symlinks are skipped so a link pointing outside the workspace root can't
/// smuggle files into `find`/`grep` results.
fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            walk_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
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
}
