use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
pub mod jobs;
pub mod skills;
mod steer;
pub mod timers;
pub mod truncate;
pub mod write;

pub use bash_env::BashEnv;
pub use bash_util::{read_capped, wait_for_cancel, PgrpKillGuard};
use fs::{
    approval_candidate, atomic_write, find_walk, is_outside_error, parse_grep_args,
    reject_non_regular, reject_symlink_leaf, resolve_for_read, walk_files_capped, WalkCeilings,
    WalkLimit, OUTSIDE_ROOT_ERROR,
};
pub use jobs::{
    JobAttributes, JobColor, JobInfo, JobNotice, JobRegistry, JobScreen, JobScreenLine, JobSpan,
    JobStyle,
};
pub use timers::TimerRegistry;
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
const MAX_PENDING_IMAGES: usize = 32;

#[derive(Debug)]
pub(super) struct PendingImage {
    pub file: std::fs::File,
    pub bytes: usize,
}

#[derive(Clone)]
pub struct BuiltinTools {
    root: PathBuf,
    tmp_dir: PathBuf,
    tool_cb: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    tool_counter: Arc<AtomicU64>,
    image_counter: Arc<AtomicU64>,
    pending_images: Arc<Mutex<std::collections::HashMap<u64, PendingImage>>>,
    bash_env: BashEnv,
    shell_policy: crate::policy::ResolvedPolicy,
    confirm: Option<crate::ConfirmFn>,
    auto_mode: Option<crate::AutoModeFn>,
    /// Session-scoped `/policy` override shared with the owning agent so a
    /// dialog pick applies to every later exec in the session.
    policy_override: crate::policy::PolicyOverride,
    skills_dir: Option<PathBuf>,
    read_roots: Vec<PathBuf>,
    /// Approves one directory for use outside the workspace root.
    approve_dir: Option<crate::ApproveDirFn>,
    cancel: Option<Arc<AtomicBool>>,
    /// Background jobs spawned by this session. Shared with the owning
    /// agent so jobs survive across the per-exec tool bundles. Explicit host
    /// shutdown kills survivors; dropping the last clone is the fallback.
    jobs: JobRegistry,
    /// Wake-up timers scheduled by this session (`lofi.notifyAt` and
    /// `lofi.notifyEvery`). Shared with the owning agent so the host can
    /// hand each fired notice back to the agent.
    timers: TimerRegistry,
    /// Owning subagent run id when this tool bundle serves one. Spawned
    /// jobs and timers are tagged with it for kill-by-owner and the
    /// per-owner cap.
    job_owner: Option<String>,
    on_job_acquired: Option<crate::JobAcquireFn>,
    /// Visible-output caps for tool results (file reads and bash output).
    truncate: truncate::TruncatedCap,
}

impl BuiltinTools {
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
            image_counter: Arc::new(AtomicU64::new(0)),
            pending_images: Arc::new(Mutex::new(std::collections::HashMap::new())),
            bash_env,
            shell_policy,
            confirm,
            auto_mode,
            policy_override: crate::policy::PolicyOverride::default(),
            skills_dir,
            read_roots,
            approve_dir: None,
            cancel: None,
            jobs: JobRegistry::new(),
            timers: TimerRegistry::new(),
            job_owner: None,
            on_job_acquired: None,
            truncate: truncate::TruncatedCap::default(),
        }
    }

    /// Attaches the run's cooperative cancellation flag. Long-running tools
    /// (`bash`) race their work against it so user cancellation takes effect
    /// even while the guest is suspended awaiting the tool — the `QuickJS`
    /// interrupt handler cannot fire there, so this is the only path.
    #[must_use]
    pub fn with_cancel(mut self, cancel: Option<Arc<AtomicBool>>) -> Self {
        self.cancel = cancel;
        self
    }

    /// The run's cooperative cancellation flag, for bound tools that await
    /// host work outside this module (`lofi.subagent`).
    pub(crate) fn cancel_flag(&self) -> Option<Arc<AtomicBool>> {
        self.cancel.clone()
    }

    /// Shares the owning session's job registry. Without this each exec
    /// would get a fresh registry and `jobStatus` on the next call could
    /// not see a job spawned by the previous one.
    #[must_use]
    pub fn with_jobs(mut self, jobs: JobRegistry) -> Self {
        self.jobs = jobs;
        self
    }

    /// Shares the owning session's wake-up timer registry. Without this
    /// each exec would get a fresh registry and a notice scheduled by one
    /// call could not fire on the next.
    #[must_use]
    pub fn with_timers(mut self, timers: TimerRegistry) -> Self {
        self.timers = timers;
        self
    }

    /// Tags spawned jobs and timers with the owning subagent run id so a
    /// subagent cancellation kills exactly its own process tree and
    /// `notifyCancel` drops exactly its own timers.
    #[must_use]
    pub fn with_job_owner(mut self, owner: Option<String>) -> Self {
        self.job_owner = owner;
        self
    }

    #[must_use]
    pub fn with_on_job_acquired(mut self, hook: Option<crate::JobAcquireFn>) -> Self {
        self.on_job_acquired = hook;
        self
    }

    /// Shares the session-scoped `/policy` approval override.
    #[must_use]
    pub fn with_policy_override(mut self, policy_override: crate::policy::PolicyOverride) -> Self {
        self.policy_override = policy_override;
        self
    }

    /// Installs the outside-workspace directory approval hook.
    #[must_use]
    pub fn with_approve_dir(mut self, approve_dir: Option<crate::ApproveDirFn>) -> Self {
        self.approve_dir = approve_dir;
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

    pub(super) fn register_image(&self, file: std::fs::File, bytes: usize) -> Result<u64> {
        let id = self.image_counter.fetch_add(1, Ordering::Relaxed);
        let mut images = self
            .pending_images
            .lock()
            .map_err(|_| Error::Tool("image registry lock poisoned".to_string()))?;
        if images.len() >= MAX_PENDING_IMAGES {
            return Err(Error::Tool(format!(
                "exec exceeded {MAX_PENDING_IMAGES}-image limit"
            )));
        }
        images.insert(id, PendingImage { file, bytes });
        Ok(id)
    }

    pub(super) fn take_image(&self, id: u64) -> Option<PendingImage> {
        self.pending_images
            .lock()
            .ok()
            .and_then(|mut images| images.remove(&id))
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

    /// Resolves a read-only tool path against the workspace root and the
    /// registered read roots. Returns the resolved path and the root that
    /// contains it, which `ls`/`find`/`grep` strip from results.
    pub(super) async fn resolve_read(&self, p: &str) -> Result<(PathBuf, PathBuf)> {
        let mut roots = Vec::with_capacity(1 + self.read_roots.len());
        roots.push(self.root.clone());
        roots.extend(self.read_roots.iter().cloned());
        self.resolve_with_approval(p, roots).await
    }

    /// Resolves a mutation path. Relative paths land under the workspace
    /// root; a registered read root stays read-only without a prompt. An
    /// absolute path outside every root offers a one-directory approval
    /// first.
    pub(super) async fn resolve_mutate(&self, p: &str) -> Result<PathBuf> {
        if let Ok((resolved, root)) = resolve_for_read(&self.root, &self.read_roots, p) {
            if root != self.root {
                return Err(Error::Tool(format!("path is read-only: {p}")));
            }
            return Ok(resolved);
        }
        self.resolve_with_approval(p, vec![self.root.clone()])
            .await
            .map(|(resolved, _)| resolved)
    }

    /// [`resolve_for_read`] with a one-directory approval for an absolute
    /// path outside every root in `roots`. The approval is offered only for
    /// the outside-roots rejection — it cannot fix a "cannot resolve" or
    /// "invalid path" failure — and never for a filesystem-root candidate.
    /// On approval the granted root is appended and the lookup runs again,
    /// so the granted root also names the root that contains the result. A
    /// refused or unavailable approval keeps the path rejected; relative
    /// paths never reach the hook.
    async fn resolve_with_approval(
        &self,
        p: &str,
        mut roots: Vec<PathBuf>,
    ) -> Result<(PathBuf, PathBuf)> {
        match resolve_for_read(&roots[0], &roots[1..], p) {
            Err(err)
                if Path::new(p).is_absolute()
                    && matches!(&err, Error::Tool(msg) if is_outside_error(msg)) =>
            {
                let Some(approve) = &self.approve_dir else {
                    return Err(err);
                };
                let Some(candidate) = approval_candidate(Path::new(p)) else {
                    return Err(err);
                };
                let Some(granted) = approve(candidate.clone()).await else {
                    return Err(Error::Tool(format!(
                        "{OUTSIDE_ROOT_ERROR}: {p} (user refused {})",
                        candidate.display()
                    )));
                };
                roots.push(granted);
                resolve_for_read(&roots[0], &roots[1..], p)
            }
            result => result,
        }
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
        let tmp = dir.path().join("lofi-tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        let tools = tools_in(dir.path().to_path_buf(), tmp);
        (dir, tools)
    }

    /// A bundle whose registered read root (`tmp_dir`) sits outside the
    /// workspace root, like the real session temporary directory.
    fn tools_in(root: PathBuf, tmp_dir: PathBuf) -> BuiltinTools {
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
        BuiltinTools::with_tool_cb(root, None, tmp_dir, BashEnv::default(), policy, None)
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
    async fn read_next_offset_pages_to_eof() {
        let (_dir, tools) = tools();
        let content = (0..10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(tools.root().join("big.txt"), content).unwrap();

        let v = tools.read("big.txt", Some(1), Some(3)).await.unwrap();
        assert_eq!(v["next_offset"], json!(4));
        assert_eq!(v["truncated"], json!(true));

        let v = tools.read("big.txt", Some(4), Some(3)).await.unwrap();
        assert_eq!(v["content"], json!("line3\nline4\nline5"));
        assert_eq!(v["next_offset"], json!(7));

        let v = tools.read("big.txt", Some(10), None).await.unwrap();
        assert_eq!(v["truncated"], json!(false));
        assert_eq!(v["next_offset"], json!(null));
    }

    #[tokio::test]
    async fn read_next_offset_skips_overlong_line() {
        // A single line larger than max_bytes returns no content; the cursor
        // must still advance so paging loops do not stall on it.
        let (_dir, tools) = tools();
        let long = "x".repeat(truncate::DEFAULT_MAX_BYTES + 1);
        std::fs::write(tools.root().join("long.txt"), format!("{long}\nshort")).unwrap();
        let v = tools.read("long.txt", Some(1), None).await.unwrap();
        assert_eq!(v["content"], json!(""));
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["next_offset"], json!(2));
        let v = tools.read("long.txt", Some(2), None).await.unwrap();
        assert_eq!(v["content"], json!("short"));
        assert_eq!(v["next_offset"], json!(null));
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
    async fn bash_sleep_command_is_refused_by_steering() {
        let (_dir, tools) = tools();
        let v = tools.bash(json!({ "cmd": "sleep 0.1" })).await.unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["status"], json!("denied"));
        assert_eq!(v["duration_ms"], json!(0));
        let output = v["output"].as_str().unwrap();
        assert!(output.contains("End the turn"), "output: {output}");
        assert!(
            output.contains("Set steer: false to run this command."),
            "output: {output}"
        );
    }

    #[tokio::test]
    async fn bash_steer_false_runs_the_command() {
        let (_dir, tools) = tools();
        let v = tools
            .bash(json!({ "cmd": "sleep 0.1", "steer": false }))
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert!(!v["output"].as_str().unwrap().contains("Steering"));
    }

    #[tokio::test]
    async fn bash_external_agent_cli_is_refused_by_steering() {
        let (_dir, tools) = tools();
        // The refusal follows the parsed command shape: the CLI need not
        // exist, and the command does not run.
        let v = tools
            .bash(json!({ "cmd": "codex --help", "timeoutMs": 5000 }))
            .await
            .unwrap();
        assert_eq!(v["status"], json!("denied"));
        assert!(v["output"].as_str().unwrap().contains("lofi.subagent"));
    }

    #[tokio::test]
    async fn bash_backgrounding_refusal_names_jobspawn() {
        let (_dir, tools) = tools();
        let v = tools.bash(json!({ "cmd": "sleep 30 &" })).await.unwrap();
        assert_eq!(v["status"], json!("denied"));
        let output = v["output"].as_str().unwrap();
        assert!(output.contains("bare & is not allowed"), "output: {output}");
        assert!(output.contains("jobSpawn"), "output: {output}");
        // The refusal has no opt-out; no line may promise one.
        assert!(!output.contains("steer: false"), "output: {output}");
    }

    #[tokio::test]
    async fn bash_backgrounding_refusal_drops_the_suggestion_with_steer_false() {
        let (_dir, tools) = tools();
        // Every detach idiom keeps its refusal with steer: false; only the
        // jobSpawn suggestion is dropped.
        for (cmd, base) in [
            ("sleep 0.1 &", "bare & is not allowed"),
            ("nohup sleep 5", "nohup is not allowed"),
        ] {
            let v = tools
                .bash(json!({ "cmd": cmd, "steer": false }))
                .await
                .unwrap();
            assert_eq!(v["status"], json!("denied"), "cmd: {cmd}");
            let output = v["output"].as_str().unwrap();
            assert!(output.contains(base), "cmd: {cmd}, output: {output}");
            assert!(!output.contains("jobSpawn"), "cmd: {cmd}, output: {output}");
            assert!(
                !output.contains("steer: false"),
                "cmd: {cmd}, output: {output}"
            );
        }
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
        // Not a plain `sleep`: steering refuses that command, and this
        // test pins the bare timeout output.
        let v = tools
            .bash(json!({ "cmd": "tail -f /dev/null", "timeoutMs": 50 }))
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(false));
        assert_eq!(v["output"], json!("<timeout>"));
        assert_eq!(v["code"], Value::Null);
        assert_eq!(v["status"], json!("timeout"));
        assert_eq!(v["duration_ms"], json!(50));
        assert_eq!(v["signal"], Value::Null);
        assert_eq!(v["command"], json!("tail -f /dev/null"));
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

    /// An approval hook that records every requested candidate and answers
    /// with `answer`.
    fn recording_hook(
        answer: Option<PathBuf>,
        asked: Arc<Mutex<Vec<PathBuf>>>,
    ) -> crate::ApproveDirFn {
        Arc::new(move |dir: PathBuf| {
            let answer = answer.clone();
            let asked = asked.clone();
            Box::pin(async move {
                asked.lock().unwrap().push(dir);
                answer
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Option<PathBuf>> + Send + Sync>,
                >
        })
    }

    #[tokio::test]
    async fn outside_read_asks_and_grants_the_holding_directory() {
        let (_dir, tools) = tools();
        let outside = tempdir().unwrap();
        std::fs::create_dir(outside.path().join("sub")).unwrap();
        std::fs::write(outside.path().join("sub/b.txt"), "hello").unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(
            Some(outside_root.clone()),
            asked.clone(),
        )));

        let path = outside
            .path()
            .join("sub/b.txt")
            .to_string_lossy()
            .into_owned();
        let v = tools.read(&path, None, None).await.unwrap();
        assert_eq!(v["content"], json!("hello"));

        // The candidate is the directory that holds the file, not the whole
        // outside tree.
        assert_eq!(*asked.lock().unwrap(), vec![outside_root.join("sub")]);

        // Results are shown against the granted root, the same way workspace
        // results are shown against the workspace root.
        let dir = outside.path().join("sub").to_string_lossy().into_owned();
        let v = tools.ls(&dir).await.unwrap();
        assert_eq!(v["entries"], json!(["sub/b.txt"]));
    }

    #[tokio::test]
    async fn outside_write_grants_the_holding_directory() {
        let (_dir, tools) = tools();
        let outside = tempdir().unwrap();
        let outside_root = outside.path().canonicalize().unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(
            Some(outside_root.clone()),
            asked.clone(),
        )));

        let path = outside
            .path()
            .join("new.txt")
            .to_string_lossy()
            .into_owned();
        let v = tools
            .write(json!({ "path": path, "text": "x" }))
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(
            std::fs::read_to_string(outside.path().join("new.txt")).unwrap(),
            "x"
        );
        assert_eq!(*asked.lock().unwrap(), vec![outside_root]);
    }

    #[tokio::test]
    async fn outside_new_directory_is_granted_at_the_request() {
        // A candidate that does not exist yet is granted as named, so a new
        // top-level directory never turns into a grant for its parent.
        let (_dir, tools) = tools();
        let outside = tempdir().unwrap();
        let candidate = outside.path().canonicalize().unwrap().join("new/deep");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools =
            tools.with_approve_dir(Some(recording_hook(Some(candidate.clone()), asked.clone())));

        let path = outside
            .path()
            .join("new/deep/file.txt")
            .to_string_lossy()
            .into_owned();
        tools
            .write(json!({ "path": path, "text": "x" }))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(outside.path().join("new/deep/file.txt")).unwrap(),
            "x"
        );
        assert_eq!(*asked.lock().unwrap(), vec![candidate]);
    }

    #[tokio::test]
    async fn outside_refusal_keeps_the_path_rejected() {
        let (_dir, tools) = tools();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(None, asked.clone())));

        let err = tools.read("/etc/passwd", None, None).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("path outside allowed roots"), "got: {msg}");
        assert!(msg.contains("user refused"), "got: {msg}");
        assert_eq!(*asked.lock().unwrap(), vec![PathBuf::from("/etc")]);
    }

    #[tokio::test]
    async fn filesystem_root_is_never_an_approval_candidate() {
        let (_dir, tools) = tools();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(
            Some(PathBuf::from("/")),
            asked.clone(),
        )));

        let err = tools.ls("/").await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("path outside allowed roots"), "got: {msg}");
        assert!(!msg.contains("user refused"), "got: {msg}");
        assert!(asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn write_under_a_read_root_is_read_only_without_a_prompt() {
        // The read root must sit outside the workspace root, like the real
        // session temporary directory, for the read-only contract to apply.
        let ws = tempdir().unwrap();
        let tmp = tempdir().unwrap();
        let tools = tools_in(ws.path().to_path_buf(), tmp.path().to_path_buf());
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(
            Some(tmp.path().to_path_buf()),
            asked.clone(),
        )));

        let path = tools
            .tmp_dir()
            .join("out.txt")
            .to_string_lossy()
            .into_owned();
        let err = tools
            .write(json!({ "path": path, "text": "x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("path is read-only"), "got: {err}");
        assert!(!tools.tmp_dir().join("out.txt").exists());
        assert!(asked.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_request_approves_the_resolved_directory() {
        use std::os::unix::fs::symlink;
        let (_dir, tools) = tools();
        let target = tempdir().unwrap();
        std::fs::write(target.path().join("x.txt"), "payload").unwrap();
        let holder = tempdir().unwrap();
        symlink(target.path().join("x.txt"), holder.path().join("link")).unwrap();
        let target_root = target.path().canonicalize().unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(
            Some(target_root.clone()),
            asked.clone(),
        )));

        let path = holder.path().join("link").to_string_lossy().into_owned();
        let v = tools.read(&path, None, None).await.unwrap();
        assert_eq!(v["content"], json!("payload"));
        // The candidate is the directory the symlink resolves into, so the
        // retry after approval succeeds instead of failing again.
        assert_eq!(*asked.lock().unwrap(), vec![target_root]);
    }

    #[tokio::test]
    async fn relative_escape_never_asks() {
        let (_dir, tools) = tools();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let tools = tools.with_approve_dir(Some(recording_hook(
            Some(PathBuf::from("/")),
            asked.clone(),
        )));

        let err = tools.read("../escape", None, None).await.unwrap_err();
        assert!(
            err.to_string().contains("escapes workspace root"),
            "got: {err}"
        );
        assert!(asked.lock().unwrap().is_empty());
    }
}
