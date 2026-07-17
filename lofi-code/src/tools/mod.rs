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
pub mod truncate;
pub mod util;
pub mod write;
mod fs;

pub use env::BashEnv;
pub use truncate::{format_size, truncate_head, truncate_head_with, truncate_line, truncate_tail, truncate_tail_with, Truncated};
pub use util::{read_capped, PgrpKillGuard};
use fs::{default_tmp_dir, find_walk, parse_grep_args, reject_non_regular, reject_symlink_leaf, resolve_under, walk_files_capped};

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
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            tmp_dir,
            tool_cb,
            tool_counter: Arc::new(AtomicU64::new(0)),
            bash_env,
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

    #[tokio::test]
    async fn bash_read_reads_from_tmp_dir() {
        let (_dir, tools) = tools();
        std::fs::write(tools.tmp_dir().join("log.txt"), "first\nsecond\nthird").unwrap();
        let v = tools.bash_read("log.txt", None, None).await.unwrap();
        assert_eq!(v, json!("first\nsecond\nthird"));
    }

    #[tokio::test]
    async fn bash_read_offset_and_limit() {
        let (_dir, tools) = tools();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(tools.tmp_dir().join("big.log"), content).unwrap();
        let v = tools.bash_read("big.log", Some(3), Some(2)).await.unwrap();
        let s = v.as_str().unwrap();
        assert!(s.starts_with("line2\nline3"));
        assert!(s.contains("6 more lines in file. Use offset=5 to continue."));
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
        assert_eq!(v, json!("hello"));
    }

    #[tokio::test]
    async fn read_offset_skips_lines() {
        let (_dir, tools) = tools();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();
        let v = tools.read("big.txt", Some(3), None).await.unwrap();
        let s = v.as_str().unwrap();
        assert!(s.starts_with("line2"), "got: {s}");
        assert!(!s.contains("line1\n"));
    }

    #[tokio::test]
    async fn read_limit_caps_line_count() {
        let (_dir, tools) = tools();
        let content = (0..10).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();
        let v = tools.read("big.txt", Some(1), Some(3)).await.unwrap();
        let s = v.as_str().unwrap();
        // 3 lines kept + continuation notice.
        assert!(s.contains("line0\nline1\nline2"));
        assert!(s.contains("7 more lines in file. Use offset=4 to continue."));
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
        let v = tools.ls("", None).await.unwrap();
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
        let v = tools.find("**/*.rs", None, None).await.unwrap();
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
        let v = tools.read("nested/x.txt", None, None).await.unwrap();
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
        assert_eq!(v, json!({ "ok": true }));
        let v = tools.read("a.txt", None, None).await.unwrap();
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
