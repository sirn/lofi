use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::Value;

use crate::ToolEvent;
use lofi_error::{Error, Result};
pub mod bash;
mod bash_env;
pub mod edit;
pub mod find;
pub mod grep;
pub mod ls;
pub mod patch;
pub mod read;

mod bash_util;
mod fs;
pub mod skills;
pub mod truncate;
pub mod write;

pub use bash_env::BashEnv;
pub use bash_util::{read_capped, wait_for_cancel, PgrpKillGuard};
use fs::{
    atomic_write, default_tmp_dir, find_walk, parse_grep_args, reject_non_regular,
    reject_symlink_leaf, resolve_for_read, resolve_under, walk_files_capped, WalkCeilings,
    WalkLimit,
};
pub use truncate::{
    format_size, truncate_head, truncate_head_with, truncate_tail, truncate_tail_with, Truncated,
};

const DEFAULT_BASH_TIMEOUT_MS: u64 = 120_000;

const MAX_BASH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_READ_BYTES: usize = 32 * 1024 * 1024;
const MAX_EDIT_BYTES: usize = 8 * 1024 * 1024;
const MAX_GREP_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_GREP_FILE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_GREP_ROWS: usize = 10_000;
const MAX_LS_ENTRIES: usize = 50_000;
const MAX_FIND_RESULTS: usize = 50_000;
const MAX_FIND_VISITED: usize = 65_536;
const MAX_GREP_VISITED: usize = 65_536;

#[derive(Clone)]
pub struct BuiltinTools {
    root: PathBuf,
    tmp_dir: PathBuf,
    tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    tool_counter: Arc<AtomicU64>,
    bash_env: BashEnv,
    shell_policy: crate::policy::ResolvedPolicy,
    confirm: Option<crate::ConfirmFn>,
    auto_mode: Option<crate::AutoModeFn>,
    skills_dir: Option<PathBuf>,
    read_roots: Vec<PathBuf>,
    cancel: Option<Arc<AtomicBool>>,
    /// Visible-output caps for tool results (file reads and bash output).
    /// Defaults to Pi values; see [`truncate::TruncatedCap`].
    truncate: truncate::TruncatedCap,
}

impl BuiltinTools {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self::with_tool_cb(
            root,
            None,
            default_tmp_dir(),
            BashEnv::default(),
            crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
            None,
        )
    }

    #[must_use]
    pub fn with_tool_cb(
        root: PathBuf,
        tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
        tmp_dir: PathBuf,
        bash_env: BashEnv,
        shell_policy: crate::policy::ResolvedPolicy,
        confirm: Option<crate::ConfirmFn>,
    ) -> Self {
        Self::with_skills_dir(
            root,
            tool_cb,
            tmp_dir,
            bash_env,
            shell_policy,
            confirm,
            None,
            None,
        )
    }

    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn with_skills_dir(
        root: PathBuf,
        tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
        tmp_dir: PathBuf,
        bash_env: BashEnv,
        shell_policy: crate::policy::ResolvedPolicy,
        confirm: Option<crate::ConfirmFn>,
        auto_mode: Option<crate::AutoModeFn>,
        skills_dir: Option<PathBuf>,
    ) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        let mut read_roots = Vec::new();
        let tmp_canon = tmp_dir.canonicalize().unwrap_or_else(|_| tmp_dir.clone());
        read_roots.push(tmp_canon);
        if let Some(sd) = &skills_dir {
            let sd_canon = sd.canonicalize().unwrap_or_else(|_| sd.clone());
            read_roots.push(sd_canon);
        }
        Self {
            root,
            tmp_dir,
            tool_cb,
            tool_counter: Arc::new(AtomicU64::new(0)),
            bash_env,
            shell_policy,
            confirm,
            auto_mode,
            skills_dir,
            read_roots,
            cancel: None,
            truncate: truncate::TruncatedCap::default(),
        }
    }

    /// Attaches the run's cooperative cancellation flag. Long-running tools
    /// (`bash`) race their work against it so user cancellation takes effect
    /// even while the guest is suspended awaiting the tool — the QuickJS
    /// interrupt handler cannot fire there, so this is the only path.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Option<Arc<AtomicBool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// Sets the visible-output cap applied to reads and bash output.
    #[must_use]
    pub fn with_truncate(mut self, truncate: truncate::TruncatedCap) -> Self {
        self.truncate = truncate;
        self
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn tmp_dir(&self) -> &Path {
        &self.tmp_dir
    }

    #[must_use]
    pub fn bash_env(&self) -> &BashEnv {
        &self.bash_env
    }

    #[must_use]
    pub fn shell_policy(&self) -> &crate::policy::ResolvedPolicy {
        &self.shell_policy
    }

    #[must_use]
    pub fn confirm(&self) -> Option<&crate::ConfirmFn> {
        self.confirm.as_ref()
    }

    #[must_use]
    pub fn auto_mode(&self) -> Option<&crate::AutoModeFn> {
        self.auto_mode.as_ref()
    }

    #[must_use]
    pub fn skills_dir(&self) -> Option<&Path> {
        self.skills_dir.as_deref()
    }

    pub(super) fn resolve_for_read(&self, p: &str) -> Result<PathBuf> {
        resolve_for_read(&self.root, &self.read_roots, p)
    }

    pub(super) fn root_for(&self, resolved: &Path) -> &Path {
        if resolved.starts_with(&self.root) {
            return &self.root;
        }
        for root in &self.read_roots {
            if resolved.starts_with(root) {
                return root;
            }
        }
        &self.root
    }

    pub(crate) fn next_tool_id(&self) -> u64 {
        self.tool_counter.fetch_add(1, Ordering::Relaxed)
    }

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
        let policy = crate::policy::ResolvedPolicy {
            allow: Vec::new(),
            ask: Vec::new(),
            deny: Vec::new(),
            wrappers: std::collections::HashMap::new(),
            redirects: lofi_types::RedirectPolicy::default(),
            heredocs: lofi_types::HeredocPolicy::default(),
            yolo: true,
            allow_by_default: true,
        };
        let tools = BuiltinTools::with_tool_cb(
            dir.path().to_path_buf(),
            None,
            default_tmp_dir(),
            BashEnv::default(),
            policy,
            None,
        );
        (dir, tools)
    }

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
    async fn read_file() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("a.txt"), "hello").unwrap();
        let v = tools.read("a.txt", None, None).await.unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["content"], json!("hello"));
    }

    #[tokio::test]
    async fn patch_applies_unified_diff() {
        let (_dir, tools) = tools();
        std::fs::write(
            tools.root().join("p.txt"),
            "alpha\nbeta\ngamma\ndelta\nepsilon\n",
        )
        .unwrap();
        let patch = "--- a/p.txt\n+++ b/p.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+BETA\n gamma\n@@ -4,2 +4,2 @@\n-delta\n+DELTA\n epsilon\n";
        let v = tools
            .patch(json!({ "path": "p.txt", "patch": patch }))
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["hunks"], json!(2));
        let content = std::fs::read_to_string(tools.root().join("p.txt")).unwrap();
        assert_eq!(content, "alpha\nBETA\ngamma\nDELTA\nepsilon\n");
    }

    #[tokio::test]
    async fn patch_errors_on_missing_context() {
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("q.txt"), "one\ntwo\n").unwrap();
        let err = tools
            .patch(json!({ "path": "q.txt", "patch": "@@ -1,2 +1,2 @@\n-nope\n+NOPE\n two\n" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("context not found"), "got: {err}");
    }

    #[tokio::test]
    async fn read_offset_skips_lines() {
        let (_dir, tools) = tools();
        let content = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();
        let v = tools.read("big.txt", Some(3), None).await.unwrap();
        let s = v["content"].as_str().unwrap();
        assert!(s.starts_with("line2"), "got: {s}");
        assert!(!s.contains("line1\n"));
    }

    #[tokio::test]
    async fn read_limit_caps_line_count() {
        let (_dir, tools) = tools();
        let content = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();
        let v = tools.read("big.txt", Some(1), Some(3)).await.unwrap();
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
    async fn read_absolute_path_in_read_root() {
        let (_dir, tools) = tools();
        std::fs::write(tools.tmp_dir().join("log.txt"), "first\nsecond\nthird").unwrap();
        let path = tools
            .tmp_dir()
            .join("log.txt")
            .to_string_lossy()
            .into_owned();
        let v = tools.read(&path, None, None).await.unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["content"], json!("first\nsecond\nthird"));
    }

    #[tokio::test]
    async fn read_absolute_path_outside_roots_rejected() {
        let (_dir, tools) = tools();
        let err = tools.read("/etc/passwd", None, None).await.unwrap_err();
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
        let entries: Vec<&str> = v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
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
        let v = tools.find("**/*.rs", None, false).await.unwrap();
        let matches: Vec<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert_eq!(v["ok"], json!(true));
        assert!(matches.contains(&"root.rs"));
        assert!(matches.contains(&"src/a.rs"));
        assert!(!matches.contains(&"src/b.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_follows_symlinked_file() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("real.rs"), "").unwrap();
        symlink("real.rs", tools.root().join("link.rs")).unwrap();
        let v = tools.find("**/*.rs", None, false).await.unwrap();
        let matches: Vec<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert!(matches.contains(&"link.rs"), "matches: {matches:?}");
        assert!(matches.contains(&"real.rs"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_follows_symlinked_dir() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        std::fs::create_dir_all(tools.root().join("realdir")).unwrap();
        std::fs::write(tools.root().join("realdir/inner.rs"), "").unwrap();
        symlink("realdir", tools.root().join("linkdir")).unwrap();
        let v = tools.find("**/*.rs", None, false).await.unwrap();
        let matches: Vec<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert!(
            matches.contains(&"linkdir/inner.rs"),
            "matches: {matches:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_does_not_follow_symlinked_dir_outside_read_root() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("secret.rs"), "secret").unwrap();
        symlink(outside.path(), tools.root().join("outside")).unwrap();

        let v = tools.find("**/*.rs", None, false).await.unwrap();
        let matches: Vec<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.as_str())
            .collect();
        assert!(
            !matches.contains(&"outside/secret.rs"),
            "matches: {matches:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn grep_does_not_follow_symlinked_dir_outside_read_root() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        let outside = tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle\n").unwrap();
        symlink(outside.path(), tools.root().join("outside")).unwrap();

        let v = tools.grep(json!("needle"), None).await.unwrap();
        assert!(v["matches"].as_array().unwrap().is_empty(), "result: {v}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_tools_keep_subdir_walk_inside_its_read_root() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        let outside = tempdir().unwrap();
        std::fs::create_dir_all(tools.root().join("sub")).unwrap();
        std::fs::write(tools.root().join("peer.txt"), "needle\n").unwrap();
        std::fs::write(outside.path().join("secret.txt"), "needle\n").unwrap();
        symlink(outside.path(), tools.root().join("sub/outside")).unwrap();

        let found = tools.find("**/*.txt", Some("sub"), false).await.unwrap();
        assert!(
            found["matches"].as_array().unwrap().is_empty(),
            "result: {found}"
        );
        let grepped = tools.grep(json!("needle"), Some("sub")).await.unwrap();
        assert!(
            grepped["matches"].as_array().unwrap().is_empty(),
            "result: {grepped}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recursive_tools_skip_symlink_cycles() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        std::fs::create_dir_all(tools.root().join("sub")).unwrap();
        std::fs::write(tools.root().join("sub/a.txt"), "needle\n").unwrap();
        symlink("..", tools.root().join("sub/loop")).unwrap();

        let found = tools.find("**/*.txt", None, false).await.unwrap();
        assert_eq!(found["matches"], json!(["sub/a.txt"]));
        let grepped = tools.grep(json!("needle"), None).await.unwrap();
        assert_eq!(grepped["matches"].as_array().unwrap().len(), 1);
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
    async fn read_follows_leaf_symlink() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("real.txt"), "payload").unwrap();
        symlink("real.txt", tools.root().join("link.txt")).unwrap();
        let v = tools.read("link.txt", None, None).await.unwrap();
        assert_eq!(v["content"], json!("payload"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_edit_reject_leaf_symlink() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        std::fs::write(tools.root().join("real.txt"), "payload").unwrap();
        symlink("real.txt", tools.root().join("link.txt")).unwrap();
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
    async fn bash_keeps_only_a_compact_tail_and_saves_full_output() {
        // A bash output cap this small forces truncation of a 30-line output
        // so the tail behavior is pinchable in a tiny fixture.
        let (_dir, tools) = {
            let (d, t) = tools();
            (
                d,
                t.with_truncate(crate::tools::truncate::TruncatedCap {
                    max_lines: 20,
                    max_bytes: 4096,
                }),
            )
        };
        let v = tools
            .bash(json!({
                "cmd": "i=1; while [ $i -le 30 ]; do echo line$i; i=$((i + 1)); done"
            }))
            .await
            .unwrap();
        let output = v["output"].as_str().unwrap();
        assert!(!output.contains("line10\n"), "output: {output}");
        assert!(output.starts_with("line11\n"), "output: {output}");
        assert!(output.contains("line30\n"), "output: {output}");
        assert!(output.contains("[Showing lines 11-30 of 30 (4.0KB limit)."));

        let path = output
            .split("Full output: ")
            .nth(1)
            .and_then(|tail| tail.split(". Use lofi.read").next())
            .unwrap();
        let full = std::fs::read_to_string(path).unwrap();
        assert!(full.starts_with("line1\n"));
        assert!(full.contains("line30\n"));
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
        let (_dir, tools) = tools();
        let v = tools.bash(json!({ "cmd": "kill -9 $$" })).await.unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["code"], Value::Null);
        assert_eq!(v["signal"], json!(9));
        assert_eq!(v["status"], json!("signaled"));
    }

    #[tokio::test]
    async fn bash_timeout_kills_process_group() {
        let (_dir, tools) = tools();
        let marker = tools.root().join("late_marker");
        let cmd = format!("(sleep 1; echo x > {}) & wait", marker.display());
        let v = tools
            .bash(json!({ "cmd": cmd, "timeoutMs": 100 }))
            .await
            .unwrap();
        assert_eq!(v["output"], json!("<timeout>"));
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
        assert!(
            !marker.exists(),
            "background child survived timeout; process group was not killed"
        );
    }
    #[tokio::test]
    async fn find_filtered_prunes_ignored_and_hidden() {
        let (_dir, tools) = tools();
        std::fs::create_dir_all(tools.root().join("target")).unwrap();
        std::fs::create_dir_all(tools.root().join(".hidden")).unwrap();
        std::fs::create_dir_all(tools.root().join("src")).unwrap();
        std::fs::write(tools.root().join("target/built.rs"), "needle\n").unwrap();
        std::fs::write(tools.root().join(".hidden/dot.rs"), "needle\n").unwrap();
        std::fs::write(tools.root().join("src/main.rs"), "needle\n").unwrap();
        std::fs::write(tools.root().join(".gitignore"), "target/\n").unwrap();

        let names = |v: &Value| -> Vec<String> {
            v["matches"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m.as_str().map(str::to_owned))
                .collect()
        };

        // Filtered (the default): target/ is gitignored, .hidden/ is hidden.
        let v = tools.find("**/*.rs", None, true).await.unwrap();
        assert_eq!(names(&v), vec!["src/main.rs".to_string()]);

        // Unfiltered: every file is visited.
        let v = tools.find("**/*.rs", None, false).await.unwrap();
        let got = names(&v);
        assert!(got.contains(&"target/built.rs".to_string()), "got: {got:?}");
        assert!(got.contains(&".hidden/dot.rs".to_string()), "got: {got:?}");
        assert!(got.contains(&"src/main.rs".to_string()), "got: {got:?}");

        // Grep honours `filtered` too (default true via parse_grep_args).
        let v = tools.grep(json!("needle"), None).await.unwrap();
        let files: Vec<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["file"].as_str())
            .collect();
        assert_eq!(files, vec!["src/main.rs"]);

        let v = tools
            .grep(json!({ "regex": "needle", "filtered": false }), None)
            .await
            .unwrap();
        let files: std::collections::HashSet<&str> = v["matches"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["file"].as_str())
            .collect();
        assert!(files.contains("target/built.rs"), "files: {files:?}");
        assert!(files.contains(".hidden/dot.rs"), "files: {files:?}");
        assert!(files.contains("src/main.rs"), "files: {files:?}");
    }
}
