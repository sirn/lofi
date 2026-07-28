//! Filesystem path-resolution, security, and walk helpers shared by the
//! builtin tools: workspace-root containment checks, symlink rejection, and
//! capped directory walks for `grep`/`find`.

#[allow(clippy::wildcard_imports)]
use super::*;

pub(super) fn default_tmp_dir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("lofi-session-{nanos:016x}"));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

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

/// Symlinks are followed (via `metadata`) so symlinked files and directories
/// appear in results; broken symlinks are skipped. The canonicalization + root
/// check in `resolve_for_read` is the security boundary for path escapes.
/// Walk `dir` recursively, collecting regular-file paths into `out`.
/// Every `read_dir` entry (files, directories, skipped specials) increments
/// `visited`, and the traversal stops at `max_visited` entries so a tree of
/// many empty directories can't make the walk effectively unbounded.
/// Non-regular entries (FIFOs, sockets, devices) are skipped so a special
/// file can't block in a later read. Returns `true` when truncated.
pub(super) fn walk_files_capped(
    dir: &Path,
    allowed_root: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut usize,
    max_visited: usize,
) -> Result<bool> {
    if !dir.is_dir() {
        return Ok(false);
    }
    let canonical_root = allowed_root.canonicalize()?;
    walk_files_capped_inner(
        dir,
        &canonical_root,
        out,
        visited,
        max_visited,
        &mut Vec::new(),
    )
}

fn walk_files_capped_inner(
    dir: &Path,
    canonical_root: &Path,
    out: &mut Vec<PathBuf>,
    visited: &mut usize,
    max_visited: usize,
    ancestors: &mut Vec<PathBuf>,
) -> Result<bool> {
    let canonical_dir = dir.canonicalize()?;
    if !canonical_dir.starts_with(canonical_root) || ancestors.contains(&canonical_dir) {
        return Ok(false);
    }
    ancestors.push(canonical_dir);
    for entry in std::fs::read_dir(dir)? {
        if *visited >= max_visited {
            return Ok(true);
        }
        *visited += 1;
        let entry = entry?;
        let ft = match std::fs::metadata(entry.path()) {
            Ok(m) => m.file_type(),
            Err(_) => continue,
        };
        if ft.is_dir() {
            if walk_files_capped_inner(
                &entry.path(),
                canonical_root,
                out,
                visited,
                max_visited,
                ancestors,
            )? {
                ancestors.pop();
                return Ok(true);
            }
        } else if ft.is_file() {
            let Ok(canonical_file) = entry.path().canonicalize() else {
                continue;
            };
            if canonical_file.starts_with(canonical_root) {
                out.push(entry.path());
            }
        }
    }
    ancestors.pop();
    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WalkLimit {
    Complete,
    TooManyHits,
    TooManyVisited,
}

/// Traverse `dir` applying `matcher` to each regular file's path relative to
/// `root`, collecting matching relative paths into `hits`. Every `read_dir`
/// entry increments `visited`; the traversal reports which safety ceiling was
/// reached.
pub(super) fn find_walk(
    dir: &Path,
    root: &Path,
    matcher: &globset::GlobMatcher,
    hits: &mut Vec<String>,
    max_hits: usize,
    visited: &mut usize,
    max_visited: usize,
) -> Result<WalkLimit> {
    if !dir.is_dir() {
        return Ok(WalkLimit::Complete);
    }
    let canonical_root = root.canonicalize()?;
    find_walk_inner(
        dir,
        root,
        &canonical_root,
        matcher,
        hits,
        max_hits,
        visited,
        max_visited,
        &mut Vec::new(),
    )
}

#[allow(clippy::too_many_arguments)]
fn find_walk_inner(
    dir: &Path,
    root: &Path,
    canonical_root: &Path,
    matcher: &globset::GlobMatcher,
    hits: &mut Vec<String>,
    max_hits: usize,
    visited: &mut usize,
    max_visited: usize,
    ancestors: &mut Vec<PathBuf>,
) -> Result<WalkLimit> {
    let canonical_dir = dir.canonicalize()?;
    if !canonical_dir.starts_with(canonical_root) || ancestors.contains(&canonical_dir) {
        return Ok(WalkLimit::Complete);
    }
    ancestors.push(canonical_dir);
    for entry in std::fs::read_dir(dir)? {
        if hits.len() >= max_hits {
            ancestors.pop();
            return Ok(WalkLimit::TooManyHits);
        }
        if *visited >= max_visited {
            ancestors.pop();
            return Ok(WalkLimit::TooManyVisited);
        }
        *visited += 1;
        let entry = entry?;
        let ft = match std::fs::metadata(entry.path()) {
            Ok(m) => m.file_type(),
            Err(_) => continue,
        };
        let path = entry.path();
        if ft.is_dir() {
            match find_walk_inner(
                &path,
                root,
                canonical_root,
                matcher,
                hits,
                max_hits,
                visited,
                max_visited,
                ancestors,
            )? {
                WalkLimit::Complete => {}
                other => {
                    ancestors.pop();
                    return Ok(other);
                }
            }
        } else if ft.is_file() {
            let Ok(canonical_file) = path.canonicalize() else {
                continue;
            };
            if !canonical_file.starts_with(canonical_root) {
                continue;
            }
            if let Ok(rel) = path.strip_prefix(root) {
                let rel = rel.to_string_lossy().into_owned();
                if matcher.is_match(&rel) {
                    hits.push(rel);
                }
            }
        }
    }
    ancestors.pop();
    Ok(WalkLimit::Complete)
}

pub(super) fn parse_grep_args(pattern: Value) -> Result<(String, bool, usize)> {
    match pattern {
        Value::String(s) => Ok((s, false, 0)),
        Value::Object(_) => {
            let re_src = pattern
                .get("regex")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Tool("grep: missing 'regex'".into()))?
                .to_owned();
            let ic = pattern.get("ic").and_then(Value::as_bool).unwrap_or(false);
            let ctx = usize::try_from(pattern.get("ctx").and_then(Value::as_u64).unwrap_or(0))
                .unwrap_or(0);
            Ok((re_src, ic, ctx))
        }
        _ => Err(Error::Tool("grep: pattern must be string or object".into())),
    }
}
