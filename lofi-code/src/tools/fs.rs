//! Filesystem path-resolution, security, and walk helpers shared by the
//! builtin tools: workspace-root containment checks, symlink rejection, and
//! capped directory walks for `grep`/`find`.

#[allow(clippy::wildcard_imports)]
use super::*;

/// Atomically replace a regular file and durably commit both its contents and
/// directory entry. The temporary file lives beside the destination so rename
/// cannot cross filesystems.
pub(super) fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write as _;

    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("file path has no parent"))?;
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let mut attempt = 0_u64;
    let (tmp, mut file) = loop {
        let tmp = parent.join(format!(".{stem}.lofi-{}-{attempt}.tmp", std::process::id()));
        match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = attempt.saturating_add(1);
            }
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub(super) fn resolve_under(root: &Path, p: &str) -> Result<PathBuf> {
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

/// Relative/empty paths resolve under `primary_root` (via [`resolve_under`]).
/// Absolute paths are canonicalized (existing prefix) and checked against
/// `primary_root` and every `extra_root`. Symlinks are followed by
/// canonicalization; the final `starts_with` check against the allowed roots
/// is the security boundary.
pub(super) fn resolve_for_read(
    primary_root: &Path,
    extra_roots: &[PathBuf],
    p: &str,
) -> Result<PathBuf> {
    if p.is_empty() || !Path::new(p).is_absolute() {
        return resolve_under(primary_root, p);
    }

    let path = Path::new(p);
    let resolved = if path.exists() {
        path.canonicalize()
            .map_err(|_| Error::Tool(format!("cannot resolve: {p}")))?
    } else {
        let mut existing = path.to_path_buf();
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
            .map_err(|_| Error::Tool(format!("cannot resolve: {p}")))?;
        let mut r = canon;
        for name in tail.into_iter().rev() {
            r.push(name);
        }
        r
    };

    if resolved.starts_with(primary_root) {
        return Ok(resolved);
    }
    for root in extra_roots {
        if resolved.starts_with(root) {
            return Ok(resolved);
        }
    }
    Err(Error::Tool(format!("path outside allowed roots: {p}")))
}

pub(super) fn reject_non_regular(label: &str, path: &Path) -> Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.is_file() {
            return Err(Error::Tool(format!("{label}: not a regular file")));
        }
    }
    Ok(())
}

/// Reject a leaf symlink at the un-canonicalized `root/p` before
/// [`resolve_under`] follows it. `resolve_under` canonicalizes existing paths,
/// so a symlink to a regular file inside the workspace would otherwise be
/// observed only as its target and silently followed.
pub(super) fn reject_symlink_leaf(root: &Path, p: &str, label: &str) -> Result<()> {
    let leaf = root.join(p);
    if std::fs::symlink_metadata(&leaf).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(Error::Tool(format!("{label}: symlink not allowed")));
    }
    Ok(())
}

/// Containment state shared by the `grep`/`find` walkers. The workspace's
/// canonical root is the security boundary: every directory is canonicalized
/// once as it is visited, and any directory (or symlinked leaf) resolving
/// outside it is skipped. Canonicalized directories are deduplicated in
/// `seen_dirs` so a symlinked directory is not descended into twice.
struct WalkGuard {
    canonical_root: PathBuf,
    seen_dirs: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
}

impl WalkGuard {
    fn new(allowed_root: &Path) -> Result<Self> {
        Ok(Self {
            canonical_root: allowed_root.canonicalize()?,
            seen_dirs: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Whether `entry` may be descended into (directories) or opened (files).
    /// Directories are canonicalized once and deduplicated; symlinked leaves
    /// are resolved so a link pointing outside the root is not followed.
    /// Regular files inside an already-contained directory are admitted
    /// without a syscall.
    fn admit(&self, entry: &ignore::DirEntry) -> bool {
        let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
        if is_dir || entry.path_is_symlink() {
            let Ok(canon) = entry.path().canonicalize() else {
                return false;
            };
            if !canon.starts_with(&self.canonical_root) {
                return false;
            }
            if is_dir {
                let mut seen = match self.seen_dirs.lock() {
                    Ok(seen) => seen,
                    Err(poisoned) => poisoned.into_inner(),
                };
                return seen.insert(canon);
            }
        }
        true
    }
}

/// Build a recursive walker over `base` with `guard` enforcing root
/// containment. Symlinks are followed (loop-safe via `walkdir`). When
/// `filtered` is set, `.gitignore`/`.ignore`/global-ignore rules and
/// hidden-file pruning apply (rg/fd-style); otherwise every entry under the
/// root is visited.
fn build_walk(base: &Path, guard: &Arc<WalkGuard>, filtered: bool) -> ignore::WalkBuilder {
    let mut builder = ignore::WalkBuilder::new(base);
    builder
        .standard_filters(filtered)
        .hidden(filtered)
        // Honor `.gitignore`/`.ignore` even when the walk root is not inside
        // a git work tree: a lofi workspace is not necessarily a repo.
        .require_git(false)
        .follow_links(true)
        .sort_by_file_path(std::cmp::Ord::cmp);
    let guard = Arc::clone(guard);
    builder.filter_entry(move |entry| guard.admit(entry));
    builder
}

/// Walk ceilings shared by `grep`/`find`, enforced across the parallel
/// walker. `visited` counts every walked entry (files and directories);
/// `capped` records which ceiling first tripped so the walk can stop
/// scheduling new work. Because traversal is parallel, the ceilings are a
/// stop point rather than an exact cut: in-flight entries may push the count
/// past the limit before the walk halts.
struct WalkCaps {
    visited: std::sync::atomic::AtomicUsize,
    max_visited: usize,
    max_hits: usize,
    limit: std::sync::Mutex<WalkLimit>,
}

impl WalkCaps {
    fn new(max_hits: usize, max_visited: usize) -> Self {
        Self {
            visited: std::sync::atomic::AtomicUsize::new(0),
            max_visited,
            max_hits,
            limit: std::sync::Mutex::new(WalkLimit::Complete),
        }
    }

    /// Account for one walked entry and the current hit count, returning
    /// whether the walk should stop. The first ceiling to trip wins.
    fn tally(&self, hits: usize) -> ignore::WalkState {
        let visited = self
            .visited
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let tripped = if hits >= self.max_hits {
            WalkLimit::TooManyHits
        } else if visited >= self.max_visited {
            WalkLimit::TooManyVisited
        } else {
            return ignore::WalkState::Continue;
        };
        let mut limit = match self.limit.lock() {
            Ok(limit) => limit,
            Err(poisoned) => poisoned.into_inner(),
        };
        if *limit == WalkLimit::Complete {
            *limit = tripped;
        }
        ignore::WalkState::Quit
    }

    fn limit(&self) -> WalkLimit {
        match self.limit.lock() {
            Ok(limit) => *limit,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

/// Recursively collect regular-file paths under `dir` into `out`, bounded by
/// `max_visited` walked entries. Traversal is parallel; `out` is unsorted on
/// return (callers sort). Returns `true` when the traversal ceiling was hit.
pub(super) fn walk_files_capped(
    dir: &Path,
    allowed_root: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut usize,
    max_visited: usize,
    filtered: bool,
) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let guard = Arc::new(WalkGuard::new(allowed_root)?);
    let caps = Arc::new(WalkCaps::new(usize::MAX, max_visited));
    let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
    build_walk(dir, &guard, filtered).build_parallel().run(|| {
        let caps = Arc::clone(&caps);
        let collected = Arc::clone(&collected);
        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let is_file = entry.file_type().is_some_and(|ft| ft.is_file());
            if is_file {
                let mut collected = match collected.lock() {
                    Ok(c) => c,
                    Err(poisoned) => poisoned.into_inner(),
                };
                collected.push(entry.path().to_path_buf());
            }
            caps.tally(0)
        })
    });
    *visited = caps.visited.load(std::sync::atomic::Ordering::Relaxed);
    let collected = match collected.lock() {
        Ok(c) => c,
        Err(poisoned) => poisoned.into_inner(),
    };
    out.extend(collected.iter().cloned());
    Ok(caps.limit() != WalkLimit::Complete)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WalkLimit {
    Complete,
    TooManyHits,
    TooManyVisited,
}

/// The two safety ceilings a recursive walk enforces: how many results to
/// collect and how many entries to visit before halting.
#[derive(Debug, Clone, Copy)]
pub(super) struct WalkCeilings {
    pub max_hits: usize,
    pub max_visited: usize,
}

/// Traverse `dir` in parallel applying `matcher` to each regular file's path
/// relative to `root`, collecting matching relative paths into `hits`
/// (unsorted; callers sort). Reports which safety ceiling was hit.
pub(super) fn find_walk(
    dir: &Path,
    root: &Path,
    matcher: &globset::GlobMatcher,
    hits: &mut Vec<String>,
    ceilings: WalkCeilings,
    visited: &mut usize,
    filtered: bool,
) -> Result<WalkLimit> {
    if !dir.is_dir() {
        return Ok(WalkLimit::Complete);
    }
    let guard = Arc::new(WalkGuard::new(root)?);
    let caps = Arc::new(WalkCaps::new(ceilings.max_hits, ceilings.max_visited));
    let collected = Arc::new(std::sync::Mutex::new(Vec::new()));
    build_walk(dir, &guard, filtered).build_parallel().run(|| {
        let caps = Arc::clone(&caps);
        let collected = Arc::clone(&collected);
        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return caps.tally(0);
            }
            let path = entry.path();
            let mut n_hits = 0usize;
            if let Ok(rel) = path.strip_prefix(root) {
                let rel = rel.to_string_lossy().into_owned();
                if matcher.is_match(&rel) {
                    let mut collected = match collected.lock() {
                        Ok(c) => c,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    collected.push(rel);
                    n_hits = collected.len();
                }
            }
            caps.tally(n_hits)
        })
    });
    *visited = caps.visited.load(std::sync::atomic::Ordering::Relaxed);
    let collected = match collected.lock() {
        Ok(c) => c,
        Err(poisoned) => poisoned.into_inner(),
    };
    hits.extend(collected.iter().cloned());
    Ok(caps.limit())
}

/// Returns `(regex, case_insensitive, context_lines, filtered)`. `filtered`
/// defaults to `true` so ignored and hidden files are pruned unless the
/// caller opts into an exhaustive walk.
pub(super) fn parse_grep_args(pattern: Value) -> Result<(String, bool, usize, bool)> {
    match pattern {
        Value::String(s) => Ok((s, false, 0, true)),
        Value::Object(_) => {
            let re_src = pattern
                .get("regex")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool("grep: missing 'regex'".into()))?
                .to_owned();
            let ic = pattern.get("ic").and_then(Value::as_bool).unwrap_or(false);
            let ctx = usize::try_from(pattern.get("ctx").and_then(Value::as_u64).unwrap_or(0))
                .unwrap_or(0);
            let filtered = pattern
                .get("filtered")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Ok((re_src, ic, ctx, filtered))
        }
        _ => Err(Error::Tool("grep: pattern must be string or object".into())),
    }
}
