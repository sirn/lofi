//! Filesystem path-resolution, security, and walk helpers shared by the
//! builtin tools: workspace-root containment checks, symlink rejection, and
//! capped directory walks for `grep`/`find`.

#[allow(clippy::wildcard_imports)]
use super::*;/// Create a fresh per-session tmp directory under the system temp dir.
/// Called when no explicit tmp dir is provided.
pub(super) fn default_tmp_dir() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("lofi-session-{nanos:016x}"));
    // Best-effort: if creation fails, fall back to the system temp dir itself
    // so bash log writes still succeed somewhere.
    let _ = std::fs::create_dir_all(&dir);
    dir
}



/// Resolve `p` against `root`, rejecting escapes.
///
/// If the target exists, it is canonicalized directly. If not (the common
/// `write` case where the leaf does not yet exist), the parent is
/// canonicalized and the leaf filename is re-joined. The result must start
/// with `root` or the call fails as a path-escape.
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
pub(super) fn reject_non_regular(label: &str, path: &Path) -> Result<()> {
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
pub(super) fn reject_symlink_leaf(root: &Path, p: &str, label: &str) -> Result<()> {
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
pub(super) fn walk_files_capped(
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
pub(super) fn find_walk(
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
pub(super) fn parse_grep_args(pattern: Value) -> Result<(String, bool, usize, usize)> {
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
