//! Built-in sandbox tools: `read`, `ls`, `find`, `grep`, `write`, `edit`,
//! `bash`. Each file tool resolves its path against a workspace root and
//! rejects escapes; `bash` is exempt (it shells out) but runs with its cwd
//! pinned to the root.
//!
//! These are plain async methods on [`BuiltinTools`]; the code-mode sandbox
//! (the crate root) binds them directly onto the guest `lofi` object rather
//! than going through a trait dispatch. The workspace root is held
//! canonicalized so `starts_with` checks are reliable after `..` traversal.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::ToolEvent;
use lofi_error::{Error, Result};
pub mod bash;
pub mod env;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod read;
pub mod bash_read;
pub mod skills;
pub mod truncate;
pub mod util;
pub mod write;
mod fs;

pub use env::BashEnv;
pub use truncate::{format_size, truncate_head, truncate_head_with, truncate_tail, truncate_tail_with, Truncated};
pub use util::{read_capped, PgrpKillGuard};
use fs::{default_tmp_dir, find_walk, parse_grep_args, reject_non_regular, reject_symlink_leaf, resolve_under, walk_files_capped, WalkLimit};

/// Default `bash` timeout in milliseconds (120s).
const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;

/// Per-stream byte ceiling for captured `bash` output. Captured bytes are also
/// the most `bash_read` can page back; output past this is discarded at the pipe.
const MAX_BASH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// File-size ceiling for `read` and `bash_read`; larger files error.
const MAX_READ_BYTES: usize = 32 * 1024 * 1024;
/// File-size ceiling for `edit`; larger files error before replacement.
const MAX_EDIT_BYTES: usize = 8 * 1024 * 1024;
/// Complete `grep` output byte ceiling; exceeding it errors.
const MAX_GREP_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Per-file `grep` size ceiling; larger files are reported as skipped.
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Complete `grep` row ceiling, including context rows; exceeding it errors.
const MAX_GREP_ROWS: usize = 10_000;
/// Complete `ls` entry ceiling; exceeding it errors.
const MAX_LS_ENTRIES: usize = 50_000;
/// Complete `find` result ceiling; exceeding it errors.
const MAX_FIND_RESULTS: usize = 50_000;
/// `find` traversal ceiling; exceeding it errors.
const MAX_FIND_VISITED: usize = 65_536;
/// `grep` traversal ceiling; exceeding it errors.
const MAX_GREP_VISITED: usize = 65_536;

/// The builtin tool bundle.
///
/// Holds the workspace root (canonicalized in [`new`](Self::new)) so every
/// file operation can be confined to it. Methods are async and return JSON
/// values ready to hand back to the sandbox.
#[derive(Clone)]
pub struct BuiltinTools {
    root: PathBuf,
    /// Per-session tmp directory for full-output logs and other
    /// agent-produced artifacts. Sandbox `bash_read` is rooted here.
    tmp_dir: PathBuf,
    /// Optional sink for native tool-call events (Start/End per call).
    tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    /// Per-exec counter assigning ids to native tool calls.
    tool_counter: Arc<AtomicU64>,
    /// Resolved `bash` child-env policy + output-redaction set.
    bash_env: BashEnv,
    /// Optional skills directory (`<config_dir>/skills`). When set,
    /// `lofi.skills()` / `lofi.skill(name)` discover and read markdown
    /// skill files from here and from `<root>/.lofi/skills/`.
    skills_dir: Option<PathBuf>,
}

impl BuiltinTools {
    /// Construct a new bundle rooted at `root`.
    ///
    /// `root` is canonicalized on construction; if that fails (the directory
    /// does not yet exist) the original path is kept and path checks fall
    /// back to lexical resolution. The per-session tmp dir defaults to a
    /// fresh directory under the system temp dir.
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self::with_tool_cb(root, None, default_tmp_dir(), BashEnv::default())
    }

    /// Like [`new`](Self::new) but also forwards native tool-call events to
    /// `cb` so the UI can render each tool under its `exec` block.
    #[must_use]
    pub fn with_tool_cb(
        root: PathBuf,
        tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
        tmp_dir: PathBuf,
        bash_env: BashEnv,
    ) -> Self {
        Self::with_skills_dir(root, tool_cb, tmp_dir, bash_env, None)
    }

    /// Like [`with_tool_cb`](Self::with_tool_cb) but also sets the skills
    /// directory for `lofi.skills()` / `lofi.skill(name)`.
    #[must_use]
    pub fn with_skills_dir(
        root: PathBuf,
        tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
        tmp_dir: PathBuf,
        bash_env: BashEnv,
        skills_dir: Option<PathBuf>,
    ) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            tmp_dir,
            tool_cb,
            tool_counter: Arc::new(AtomicU64::new(0)),
            bash_env,
            skills_dir,
        }
    }

    /// The canonicalized workspace root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The per-session tmp directory (for full-output logs, etc.).
    #[must_use]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    /// The resolved `bash` child-env policy + redaction set.
    #[must_use]
    pub fn bash_env(&self) -> &BashEnv {
        &self.bash_env
    }

    /// The skills directory, if configured.
    #[must_use]
    pub fn skills_dir(&self) -> Option<&Path> {
        self.skills_dir.as_deref()
    }

    /// Allocate the next native tool-call id.
    pub(crate) fn next_tool_id(&self) -> u64 {
        self.tool_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Forward a tool event to the sink, if any.
    pub(crate) fn emit(&self, ev: ToolEvent) {
        if let Some(cb) = &self.tool_cb {
            cb(ev);
        }
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

    /// Format a `grep` result's matches as `file:line:content` strings.
    fn grep_lines(v: &Value) -> Vec<String> {
        v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                format!(
                    "{}:{}:{}",
                    m["file"].as_str().unwrap_or(""),
                    m["line"].as_u64().unwrap_or(0),
                    m["content"].as_str().unwrap_or("")
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn bash_read_reads_from_tmp_dir() {
        let (_dir, tools) = tools();
        std::fs::write(tools.tmp_dir().join("log.txt"), "first\nsecond\nthird").unwrap();
        let v = tools.bash_read("log.txt", None, None).await.unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["content"], json!("first\nsecond\nthird"));
    }

    #[tokio::test]
    async fn bash_read_offset_and_limit() {
        let (_dir, tools) = tools();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(tools.tmp_dir().join("big.log"), content).unwrap();
        let v = tools.bash_read("big.log", Some(3), Some(2)).await.unwrap();
        let s = v["content"].as_str().unwrap();
        assert!(s.starts_with("line2\nline3"));
        assert_eq!(v["truncated"], json!(true));
    }

    #[tokio::test]
    async fn bash_read_rejects_workspace_escape() {
        let (_dir, tools) = tools();
        // The tmp dir is separate from the workspace root; a path that
        // escapes the tmp dir is rejected.
        let err = tools.bash_read("../escape", None, None).await.unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn read_file() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "hello").unwrap();
        let v = tools.read("a.txt", None, None).await.unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["content"], json!("hello"));
    }

    #[tokio::test]
    async fn read_offset_skips_lines() {
        let (_dir, tools) = tools();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();
        let v = tools.read("big.txt", Some(3), None).await.unwrap();
        let s = v["content"].as_str().unwrap();
        assert!(s.starts_with("line2"), "got: {s}");
        assert!(!s.contains("line1\n"));
    }

    #[tokio::test]
    async fn read_limit_caps_line_count() {
        let (_dir, tools) = tools();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();
        let v = tools.read("big.txt", Some(1), Some(3)).await.unwrap();
        // 3 lines kept; truncation is signalled structurally.
        assert_eq!(v["content"], json!("line0\nline1\nline2"));
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["total_lines"], json!(10));
    }

    #[tokio::test]
    async fn read_offset_beyond_end_errors() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "one\ntwo").unwrap();
        let err = tools.read("a.txt", Some(99), None).await.unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn read_escape_rejected() {
        let (_dir, tools) = tools();
        let err = tools.read("../escape", None, None).await.unwrap_err();
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
        let err = tools.read("/etc/passwd", None, None).await.unwrap_err();
        assert!(matches!(err, Error::Tool(_)));
    }

    #[tokio::test]
    async fn ls_lists_entries() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("b.txt"), "b").unwrap();
        std::fs::create_dir(tools.root().join("sub")).unwrap();
        std::fs::write(tools.root().join("sub/c.txt"), "c").unwrap();
        let v = tools.ls("").await.unwrap();
        let entries: Vec<&str> = v["entries"].as_array().unwrap().iter().filter_map(|e| e.as_str()).collect();
        assert_eq!(v["ok"], json!(true));
        assert!(entries.contains(&"b.txt"));
        assert!(entries.contains(&"sub"));
    }

    #[tokio::test]
    async fn find_glob() {
        let (_dir, tools) = tools();
        std::fs::create_dir_all(tools.root().join("src")).unwrap();
        std::fs::write(tools.root().join("src/a.rs"), "").unwrap();
        std::fs::write(tools.root().join("src/b.txt"), "").unwrap();
        std::fs::write(tools.root().join("root.rs"), "").unwrap();
        let v = tools.find("**/*.rs", None).await.unwrap();
        let matches: Vec<&str> = v["matches"].as_array().unwrap().iter().filter_map(|e| e.as_str()).collect();
        assert_eq!(v["ok"], json!(true));
        assert!(matches.contains(&"root.rs"));
        assert!(matches.contains(&"src/a.rs"));
        assert!(!matches.contains(&"src/b.txt"));
    }

    #[tokio::test]
    async fn grep_basic_and_case_insensitive() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "Foo\nbar\nFOO\n").unwrap();
        let v = tools.grep(json!("FOO"), None).await.unwrap();
        let lines = grep_lines(&v);
        assert!(lines.contains(&"a.txt:3:FOO".to_string()));
        assert!(!lines.contains(&"a.txt:1:Foo".to_string()));

        let v = tools
            .grep(json!({ "regex": "foo", "ic": true }), None)
            .await
            .unwrap();
        let lines = grep_lines(&v);
        assert!(lines.contains(&"a.txt:1:Foo".to_string()));
        assert!(lines.contains(&"a.txt:3:FOO".to_string()));
    }

    #[tokio::test]
    async fn grep_object_with_context() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "l1\nl2\nMATCH\nl4\nl5\n").unwrap();
        let v = tools
            .grep(json!({ "regex": "MATCH", "ctx": 1 }), None)
            .await
            .unwrap();
        let lines = grep_lines(&v);
        assert!(lines.contains(&"a.txt:2:l2".to_string()));
        assert!(lines.contains(&"a.txt:3:MATCH".to_string()));
        assert!(lines.contains(&"a.txt:4:l4".to_string()));
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
        let lines = grep_lines(&v);
        assert!(lines.contains(&"sub/a.txt:1:alpha".to_string()));
        assert!(lines.iter().all(|l| !l.contains("b.txt")));
    }

    #[tokio::test]
    async fn write_then_read_round_trip() {
        let (_dir, tools) = tools();
        let v = tools
            .write(json!({ "path": "nested/x.txt", "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(v["content"], json!("hi"));
        assert_eq!(v["ok"], json!(true));
        let v = tools.read("nested/x.txt", None, None).await.unwrap();
        assert_eq!(v["content"], json!("hi"));
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
        let err = tools.read("link.txt", None, None).await.unwrap_err();
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
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["old"], json!("beta"));
        assert_eq!(v["new"], json!("BETA"));
        let v = tools.read("a.txt", None, None).await.unwrap();
        assert_eq!(v["content"], json!("alpha BETA gamma"));
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
        assert_eq!(v["command"], json!("echo hello"));
        assert_eq!(v["directory"], json!(tools.root().display().to_string()));
        assert_eq!(v["signal"], Value::Null);
        assert_eq!(v["status"], json!("exited"));
        assert!(v["duration_ms"].as_u64().is_some());
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
        assert_eq!(v["status"], json!("timeout"));
        assert_eq!(v["duration_ms"], json!(50));
        assert_eq!(v["signal"], Value::Null);
        assert_eq!(v["command"], json!("sleep 5"));
    }

    #[tokio::test]
    async fn bash_signal_death_reports_signal() {
        // `kill -9 $$` terminates the shell with SIGKILL (9); the result
        // carries the signal number and a null exit code.
        let (_dir, tools) = tools();
        let v = tools.bash(json!({ "cmd": "kill -9 $$" })).await.unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["code"], Value::Null);
        assert_eq!(v["signal"], json!(9));
        assert_eq!(v["status"], json!("signaled"));
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
