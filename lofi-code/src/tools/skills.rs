//! Skill discovery and reading.
//!
//! Skills are directories containing a `SKILL.md` file that provide reusable
//! instructions or domain knowledge the agent can load on demand. Two sources
//! are scanned:
//!
//! - **Global** — `<skills_dir>/<name>/SKILL.md` (the `<config_dir>/skills/`
//!   directory).
//! - **Per-workspace** — `<root>/.lofi/skills/<name>/SKILL.md` (checked into
//!   the repo for project-specific skills).
//!
//! Skill names are the directory path relative to the skills root, so
//! `skills/git-workflow/SKILL.md` → `git-workflow` and
//! `skills/git-workflow/rebase/SKILL.md` → `git-workflow/rebase`. When both
//! sources define the same name, the per-workspace version wins (it is more
//! specific). The description is the first non-heading non-empty line of
//! `SKILL.md`.
//!
//! Symlinks are followed — skill directories or `SKILL.md` files may be
//! symlinks (e.g. referencing nix store paths). The walk is bounded by depth
//! and visited-entry limits to prevent infinite loops.

#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

/// Maximum number of skill directories scanned across both sources.
const MAX_SKILLS: usize = 500;
/// Maximum directory walk depth for skill discovery.
const MAX_WALK_DEPTH: usize = 8;
/// Maximum directory entries visited during skill discovery.
const MAX_WALK_VISITED: usize = 10_000;
/// The marker file name identifying a skill directory.
const SKILL_FILE: &str = "SKILL.md";

impl BuiltinTools {
    /// List available skills from the global and per-workspace directories.
    ///
    /// Each entry is `{ name, description, source }` where `source` is
    /// `"global"` or `"workspace"`. When both sources define the same name,
    /// only the workspace entry is returned. The result is sorted by name.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the number of skills exceeds
    /// [`MAX_SKILLS`] or the walk exceeds [`MAX_WALK_VISITED`].
    #[allow(clippy::unused_async)]
    pub async fn skills(&self) -> Result<Value> {
        let entries = self.scan_skills()?;
        Ok(json!({ "ok": true, "skills": entries }))
    }

    /// Read a single skill's `SKILL.md` by name. When both sources define the
    /// same name, the per-workspace version is returned.
    ///
    /// The name may contain `/` as a namespace separator (e.g.
    /// `git-workflow/rebase`). `..` and absolute paths are rejected.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the skill is not found or the file cannot
    /// be read.
    #[allow(clippy::unused_async)]
    pub async fn skill(&self, name: &str) -> Result<Value> {
        let (path, source) = self.resolve_skill_path(name)?;
        Self::read_skill_file(&path, name, SKILL_FILE, source)
    }

    /// Read a companion file within a skill's directory.
    ///
    /// `file` is a path relative to the skill directory (e.g.
    /// `examples/branching.md`). `..`, absolute paths, and backslashes are
    /// rejected. This is the only way to read files under global skills,
    /// which live outside the workspace root and are therefore unreachable
    /// via `lofi.read`.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the skill or file is not found, or the file
    /// path is invalid.
    #[allow(clippy::unused_async)]
    pub async fn skill_read(&self, name: &str, file: &str) -> Result<Value> {
        validate_skill_name(name)?;
        validate_skill_file(file)?;

        // Resolve the skill directory (not the SKILL.md file).
        let (skill_dir, source) = self.resolve_skill_dir(name)?;
        let file_path = skill_dir.join(file);
        if !file_path.is_file() {
            return Err(Error::Tool(format!(
                "skill `{name}`: file `{file}` not found"
            )));
        }
        Self::read_skill_file(&file_path, name, file, source)
    }

    /// Search across all skill `SKILL.md` files for a case-insensitive
    /// substring match. Returns matching skills with their description and
    /// up to 5 matching lines (with line numbers and context).
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the query is empty or the walk fails.
    #[allow(clippy::unused_async)]
    pub async fn skill_search(&self, query: &str) -> Result<Value> {
        if query.trim().is_empty() {
            return Err(Error::Tool("skill_search: query must not be empty".into()));
        }

        let query_lower = query.to_lowercase();
        let entries = self.scan_skills()?;

        let mut results: Vec<Value> = Vec::new();
        for entry in entries {
            let name = entry["name"].as_str().unwrap_or_default();
            let source = entry["source"].as_str().unwrap_or_default();
            let desc = entry["description"].as_str().unwrap_or_default();
            let skill_path = entry["path"].as_str().unwrap_or_default();

            let (path, _) = self.resolve_skill_path(name)?;
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let mut matches: Vec<Value> = Vec::new();
            for (i, line) in content.lines().enumerate() {
                if line.to_lowercase().contains(&query_lower) {
                    matches.push(json!({
                        "line": i + 1,
                        "text": line.trim(),
                    }));
                    if matches.len() >= 5 {
                        break;
                    }
                }
            }

            if !matches.is_empty() {
                results.push(json!({
                    "name": name,
                    "description": desc,
                    "source": source,
                    "path": skill_path,
                    "matches": matches,
                }));
            }
        }

        Ok(json!({ "ok": true, "results": results }))
    }

    /// Resolve a skill name to its `SKILL.md` path and source.
    /// Workspace wins over global on name collision.
    fn resolve_skill_path(&self, name: &str) -> Result<(PathBuf, &'static str)> {
        validate_skill_name(name)?;

        // Check per-workspace first (more specific).
        let ws_skills = self.root.join(".lofi").join("skills");
        let ws_path = ws_skills.join(name).join(SKILL_FILE);
        if ws_path.is_file() {
            return Ok((ws_path, "workspace"));
        }

        // Then global.
        if let Some(dir) = &self.skills_dir {
            let g_path = dir.join(name).join(SKILL_FILE);
            if g_path.is_file() {
                return Ok((g_path, "global"));
            }
        }

        Err(Error::Tool(format!(
            "skill `{name}` not found"
        )))
    }

    /// Resolve a skill name to its directory path and source.
    fn resolve_skill_dir(&self, name: &str) -> Result<(PathBuf, &'static str)> {
        validate_skill_name(name)?;

        let ws_skills = self.root.join(".lofi").join("skills");
        let ws_dir = ws_skills.join(name);
        if ws_dir.is_dir() && ws_dir.join(SKILL_FILE).is_file() {
            return Ok((ws_dir, "workspace"));
        }

        if let Some(dir) = &self.skills_dir {
            let g_dir = dir.join(name);
            if g_dir.is_dir() && g_dir.join(SKILL_FILE).is_file() {
                return Ok((g_dir, "global"));
            }
        }

        Err(Error::Tool(format!(
            "skill `{name}` not found"
        )))
    }

    /// Scan both skill directories and collect sorted, de-duplicated entries.
    fn scan_skills(&self) -> Result<Vec<Value>> {
        // name → (description, source)
        let mut map: std::collections::BTreeMap<String, (String, String, String)> =
            std::collections::BTreeMap::new();

        // Global skills.
        if let Some(dir) = &self.skills_dir {
            Self::walk_skills(dir, "global", &mut map)?;
        }

        // Per-workspace skills (override global on name collision).
        let ws_skills = self.root.join(".lofi").join("skills");
        Self::walk_skills(&ws_skills, "workspace", &mut map)?;

        if map.len() > MAX_SKILLS {
            return Err(Error::Tool(format!(
                "skills: exceeded {MAX_SKILLS}-entry limit"
            )));
        }

        Ok(map
            .into_iter()
            .map(|(name, (desc, source, path))| {
                json!({
                    "name": name,
                    "description": desc,
                    "source": source,
                    "path": path,
                })
            })
            .collect())
    }

    /// Recursively walk `dir` for `<sub>/SKILL.md` files, inserting into `map`.
    /// The skill name is the directory path relative to `dir`. Symlinks are
    /// followed. The walk is bounded by [`MAX_WALK_DEPTH`] and
    /// [`MAX_WALK_VISITED`].
    fn walk_skills(
        dir: &Path,
        source: &str,
        map: &mut std::collections::BTreeMap<String, (String, String, String)>,
    ) -> Result<()> {
        let mut visited = 0usize;
        Self::walk_skills_inner(dir, dir, source, map, 0, &mut visited)
    }

    /// Recursive inner walk. `root` is the skills root for computing relative
    /// names; `dir` is the current directory being scanned.
    fn walk_skills_inner(
        dir: &Path,
        root: &Path,
        source: &str,
        map: &mut std::collections::BTreeMap<String, (String, String, String)>,
        depth: usize,
        visited: &mut usize,
    ) -> Result<()> {
        if depth > MAX_WALK_DEPTH || *visited > MAX_WALK_VISITED {
            return Ok(());
        }
        if !dir.is_dir() {
            return Ok(());
        }
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in read_dir {
            *visited += 1;
            if *visited > MAX_WALK_VISITED {
                return Ok(());
            }
            let path = match entry {
                Ok(e) => e.path(),
                Err(_) => continue,
            };
            if !path.is_dir() {
                continue;
            }
            // Check if this directory is a skill (contains SKILL.md).
            // Symlinks to SKILL.md or to the directory itself are followed.
            let skill_file = path.join(SKILL_FILE);
            if skill_file.is_file() {
                let name = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|p| p.to_str())
                    .unwrap_or_default()
                    .to_string();
                if !name.is_empty() {
                    let desc = read_description(&skill_file).unwrap_or_default();
                    let dir_path = path.to_string_lossy().to_string();
                    map.insert(name, (desc, source.to_string(), dir_path));
                }
            }
            // Recurse into subdirectories for nested skills.
            Self::walk_skills_inner(&path, root, source, map, depth + 1, visited)?;
        }
        Ok(())
    }

    /// Read a file within a skill directory and return the structured result.
    fn read_skill_file(
        path: &Path,
        name: &str,
        file: &str,
        source: &str,
    ) -> Result<Value> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::Tool(format!("skill `{name}`: {e}"))
        })?;
        let skill_dir = path.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        Ok(json!({
            "ok": true,
            "name": name,
            "source": source,
            "file": file,
            "path": skill_dir,
            "content": content,
        }))
    }
}

/// Validate a skill name: non-empty, no `..`, no leading `/`, no backslash.
/// `/` is allowed as a namespace separator.
fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Tool("skill: name must not be empty".into()));
    }
    if name.starts_with('/') || name.contains('\\') {
        return Err(Error::Tool(format!(
            "skill: invalid name `{name}`"
        )));
    }
    // Reject any `..` component.
    for component in name.split('/') {
        if component == ".." {
            return Err(Error::Tool(format!(
                "skill: invalid name `{name}`"
            )));
        }
    }
    Ok(())
}

/// Validate a file path within a skill directory: non-empty, no `..`, no
/// leading `/`, no backslash. `/` is allowed for subdirectories.
fn validate_skill_file(file: &str) -> Result<()> {
    if file.is_empty() {
        return Err(Error::Tool("skill: file path must not be empty".into()));
    }
    if file.starts_with('/') || file.contains('\\') {
        return Err(Error::Tool(format!(
            "skill: invalid file path `{file}`"
        )));
    }
    for component in file.split('/') {
        if component == ".." {
            return Err(Error::Tool(format!(
                "skill: invalid file path `{file}`"
            )));
        }
    }
    Ok(())
}

/// Extract the first non-heading, non-empty line from a markdown file as
/// the skill description. Falls back to the parent directory name when the
/// file is empty or all headings.
fn read_description(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Skip markdown headings and front-matter delimiters.
        if trimmed.starts_with('#') || trimmed == "---" {
            continue;
        }
        return Some(trimmed.to_string());
    }
    // Fall back to the parent directory name.
    path.parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(std::string::ToString::to_string)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tempfile::tempdir;

    fn tools(root: &Path, skills_dir: Option<PathBuf>) -> BuiltinTools {
        let tmp_dir = std::env::temp_dir().join("lofi-test-skills");
        let _ = std::fs::create_dir_all(&tmp_dir);
        BuiltinTools::with_skills_dir(
            root.to_path_buf(),
            None,
            tmp_dir,
            BashEnv::default(),
            skills_dir,
        )
    }

    /// Create a skill directory with `SKILL.md`.
    fn make_skill(root: &Path, name: &str, content: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SKILL_FILE), content).unwrap();
    }

    #[tokio::test]
    async fn skills_list_global() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(
            skills.path(),
            "git-workflow",
            "# Git Workflow\n\nStandard branching and commit workflow.\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 1);
        assert_eq!(skills_arr[0]["name"], json!("git-workflow"));
        assert_eq!(
            skills_arr[0]["description"],
            json!("Standard branching and commit workflow.")
        );
        assert_eq!(skills_arr[0]["source"], json!("global"));
    }

    #[tokio::test]
    async fn skills_list_namespaced() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git-workflow", "Top-level git skill.\n");
        make_skill(
            skills.path(),
            "git-workflow/rebase",
            "# Rebase\n\nNested rebase skill.\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 2);
        assert_eq!(skills_arr[0]["name"], json!("git-workflow"));
        assert_eq!(skills_arr[1]["name"], json!("git-workflow/rebase"));
        assert_eq!(skills_arr[1]["description"], json!("Nested rebase skill."));
    }

    #[tokio::test]
    async fn skills_list_workspace_overrides_global() {
        let dir = tempdir().unwrap();
        let g_skills = tempdir().unwrap();
        make_skill(g_skills.path(), "deploy", "Global deploy instructions.\n");
        make_skill(
            dir.path().join(".lofi").join("skills").as_path(),
            "deploy",
            "# Deploy\n\nProject-specific deploy.\n",
        );
        let v = tools(dir.path(), Some(g_skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 1);
        assert_eq!(skills_arr[0]["source"], json!("workspace"));
        assert_eq!(skills_arr[0]["description"], json!("Project-specific deploy."));
    }

    #[tokio::test]
    async fn skills_list_empty_when_no_dirs() {
        let dir = tempdir().unwrap();
        let v = tools(dir.path(), None).skills().await.unwrap();
        assert_eq!(v["skills"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn skill_read_global() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(
            skills.path(),
            "testing",
            "# Testing\n\nRun cargo test and clippy.\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill("testing")
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["name"], json!("testing"));
        assert_eq!(v["source"], json!("global"));
        assert_eq!(v["file"], json!("SKILL.md"));
        assert!(v["content"].as_str().unwrap().contains("Run cargo test"));
    }

    #[tokio::test]
    async fn skill_read_namespaced() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(
            skills.path(),
            "git-workflow/rebase",
            "# Rebase\n\nRebase workflow.\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill("git-workflow/rebase")
            .await
            .unwrap();
        assert_eq!(v["name"], json!("git-workflow/rebase"));
        assert!(v["content"].as_str().unwrap().contains("Rebase workflow"));
    }

    #[tokio::test]
    async fn skill_read_workspace_overrides_global() {
        let dir = tempdir().unwrap();
        let g_skills = tempdir().unwrap();
        make_skill(g_skills.path(), "ci", "global ci\n");
        make_skill(
            dir.path().join(".lofi").join("skills").as_path(),
            "ci",
            "workspace ci\n",
        );
        let v = tools(dir.path(), Some(g_skills.path().to_path_buf()))
            .skill("ci")
            .await
            .unwrap();
        assert_eq!(v["source"], json!("workspace"));
        assert_eq!(v["content"], json!("workspace ci\n"));
    }

    #[tokio::test]
    async fn skill_not_found() {
        let dir = tempdir().unwrap();
        let err = tools(dir.path(), None)
            .skill("nonexistent")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn skill_rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let err = tools(dir.path(), None)
            .skill("../etc/passwd")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid name"));
    }

    #[tokio::test]
    async fn skill_rejects_leading_slash() {
        let dir = tempdir().unwrap();
        let err = tools(dir.path(), None)
            .skill("/etc/passwd")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid name"));
    }

    #[tokio::test]
    async fn skill_rejects_dotdot_component() {
        let dir = tempdir().unwrap();
        let err = tools(dir.path(), None)
            .skill("foo/../bar")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid name"));
    }

    #[tokio::test]
    async fn skill_read_reads_companion() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git-workflow", "# Git\n\nWorkflow.\n");
        std::fs::create_dir_all(skills.path().join("git-workflow").join("examples"))
            .unwrap();
        std::fs::write(
            skills.path().join("git-workflow").join("examples").join("branching.md"),
            "Example branching strategy.\n",
        )
        .unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_read("git-workflow", "examples/branching.md")
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["name"], json!("git-workflow"));
        assert_eq!(v["file"], json!("examples/branching.md"));
        assert_eq!(v["source"], json!("global"));
        assert!(v["content"].as_str().unwrap().contains("Example branching"));
    }

    #[tokio::test]
    async fn skill_read_not_found() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git-workflow", "# Git\n");
        let err = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_read("git-workflow", "missing.txt")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn skill_read_rejects_dotdot() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git-workflow", "# Git\n");
        let err = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_read("git-workflow", "../../../etc/passwd")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid file path"));
    }

    #[tokio::test]
    async fn skill_read_rejects_leading_slash() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git-workflow", "# Git\n");
        let err = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_read("git-workflow", "/etc/passwd")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid file path"));
    }

    #[tokio::test]
    async fn skills_follows_symlinked_dir() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        let real = tempdir().unwrap();
        // Create a real skill directory, then symlink it into skills/.
        std::fs::write(real.path().join(SKILL_FILE), "Symlinked skill.\n").unwrap();
        std::os::unix::fs::symlink(real.path(), skills.path().join("linked"))
            .unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 1);
        assert_eq!(skills_arr[0]["name"], json!("linked"));
        assert_eq!(skills_arr[0]["description"], json!("Symlinked skill."));
    }

    #[tokio::test]
    async fn skills_follows_symlinked_skill_file() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        let real_file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(&real_file, "Symlinked SKILL.md.\n").unwrap();
        std::fs::create_dir_all(skills.path().join("linked-file")).unwrap();
        std::os::unix::fs::symlink(real_file.path(), skills.path().join("linked-file").join(SKILL_FILE))
            .unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill("linked-file")
            .await
            .unwrap();
        assert!(v["content"].as_str().unwrap().contains("Symlinked SKILL.md"));
    }

    #[tokio::test]
    async fn skill_description_skips_headings() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(
            skills.path(),
            "lint",
            "# Linting\n## Subsection\n\nRun clippy and fmt.\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let desc = v["skills"][0]["description"].as_str().unwrap();
        assert_eq!(desc, "Run clippy and fmt.");
    }

    #[tokio::test]
    async fn skill_description_falls_back_to_dir_name() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "my-skill", "# Only Heading\n");
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let desc = v["skills"][0]["description"].as_str().unwrap();
        assert_eq!(desc, "my-skill");
    }

    #[tokio::test]
    async fn skills_ignores_non_skill_md() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        // A directory with a random .md file (not SKILL.md) should be ignored.
        std::fs::create_dir_all(skills.path().join("foo")).unwrap();
        std::fs::write(skills.path().join("foo").join("README.md"), "not a skill\n").unwrap();
        // But a directory with SKILL.md is found.
        make_skill(skills.path(), "bar", "a real skill\n");
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 1);
        assert_eq!(skills_arr[0]["name"], json!("bar"));
    }

    #[tokio::test]
    async fn skill_search_finds_matches() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(
            skills.path(),
            "git-workflow",
            "# Git Workflow\n\nAlways rebase before merging.\nUse conventional commits.\n",
        );
        make_skill(
            skills.path(),
            "deploy",
            "# Deploy\n\nRun terraform apply.\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_search("rebase")
            .await
            .unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], json!("git-workflow"));
        assert!(results[0]["description"].as_str().unwrap().contains("rebase"));
        let matches = results[0]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line"], json!(3));
        assert!(matches[0]["text"].as_str().unwrap().contains("rebase"));
    }

    #[tokio::test]
    async fn skill_search_case_insensitive() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "lint", "# Lint\n\nRun CLIPPY and fmt.\n");
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_search("clippy")
            .await
            .unwrap();
        let results = v["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], json!("lint"));
    }

    #[tokio::test]
    async fn skill_search_no_matches() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git", "# Git\n\nSome workflow.\n");
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_search("nonexistent-term")
            .await
            .unwrap();
        assert_eq!(v["results"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn skill_search_rejects_empty_query() {
        let dir = tempdir().unwrap();
        let err = tools(dir.path(), None)
            .skill_search("")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn skill_search_caps_matches_per_skill() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        // 6 lines all containing "match" — should cap at 5.
        make_skill(
            skills.path(),
            "many",
            "# Many\n\nmatch one\nmatch two\nmatch three\nmatch four\nmatch five\nmatch six\n",
        );
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill_search("match")
            .await
            .unwrap();
        let matches = v["results"][0]["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 5);
    }
}