//! Symlinks are followed — skill directories or `SKILL.md` files may be
//! symlinks (e.g. referencing nix store paths). The walk is bounded by depth
//! and visited-entry limits to prevent infinite loops.

#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

const MAX_SKILLS: usize = 500;
const MAX_WALK_DEPTH: usize = 8;
const MAX_WALK_VISITED: usize = 10_000;
const SKILL_FILE: &str = "SKILL.md";
/// Maximum size of a skill file; reads are bounded so a large or linked
/// `SKILL.md` cannot exhaust host memory outside `QuickJS`'s limit.
const MAX_SKILL_FILE_BYTES: usize = 1024 * 1024;

impl BuiltinTools {
    /// # Errors
    /// Returns an error when a configured skill root cannot be scanned safely.
    #[allow(clippy::unused_async)]
    pub async fn skills(&self, search: Option<&str>) -> Result<Value> {
        let entries = self.scan_skills(search)?;
        Ok(json!({ "ok": true, "skills": entries }))
    }

    /// The name may contain `/` as a namespace separator (e.g.
    /// `git-workflow/rebase`). `..` and absolute paths are rejected.
    /// # Errors
    /// Returns [`Error::Tool`] if the skill is not found or the file cannot
    /// be read.
    #[allow(clippy::unused_async)]
    pub async fn skill(&self, name: &str) -> Result<Value> {
        let (path, source) = self.resolve_skill_path(name)?;
        Self::read_skill_file(&path, name, SKILL_FILE, source)
    }

    fn resolve_skill_path(&self, name: &str) -> Result<(PathBuf, &'static str)> {
        validate_skill_name(name)?;

        let ws_skills = self.root.join(".lofi").join("skills");
        let ws_path = ws_skills.join(name).join(SKILL_FILE);
        if ws_path.is_file() {
            return Ok((ws_path, "workspace"));
        }

        if let Some(dir) = &self.skills_dir {
            let g_path = dir.join(name).join(SKILL_FILE);
            if g_path.is_file() {
                return Ok((g_path, "global"));
            }
        }

        Err(Error::Tool(format!("skill `{name}` not found")))
    }

    fn scan_skills(&self, search: Option<&str>) -> Result<Vec<Value>> {
        let mut map: std::collections::BTreeMap<String, (String, String, String)> =
            std::collections::BTreeMap::new();

        if let Some(dir) = &self.skills_dir {
            Self::walk_skills(dir, "global", &mut map)?;
        }

        let ws_skills = self.root.join(".lofi").join("skills");
        Self::walk_skills(&ws_skills, "workspace", &mut map)?;

        if map.len() > MAX_SKILLS {
            return Err(Error::Tool(format!(
                "skills: exceeded {MAX_SKILLS}-entry limit"
            )));
        }

        let needle = search.map(str::to_lowercase);
        Ok(map
            .into_iter()
            .filter(|(name, (desc, _, _))| {
                needle.as_ref().is_none_or(|n| {
                    name.to_lowercase().contains(n) || desc.to_lowercase().contains(n)
                })
            })
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

    fn walk_skills(
        dir: &Path,
        source: &str,
        map: &mut std::collections::BTreeMap<String, (String, String, String)>,
    ) -> Result<()> {
        let mut visited = 0usize;
        Self::walk_skills_inner(dir, dir, source, map, 0, &mut visited)
    }

    fn walk_skills_inner(
        dir: &Path,
        root: &Path,
        source: &str,
        map: &mut std::collections::BTreeMap<String, (String, String, String)>,
        depth: usize,
        visited: &mut usize,
    ) -> Result<()> {
        if depth > MAX_WALK_DEPTH {
            return Err(Error::Tool(format!(
                "skills: exceeded {MAX_WALK_DEPTH}-depth limit"
            )));
        }
        if *visited > MAX_WALK_VISITED {
            return Err(Error::Tool(format!(
                "skills: exceeded {MAX_WALK_VISITED}-entry walk limit"
            )));
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
                return Err(Error::Tool(format!(
                    "skills: exceeded {MAX_WALK_VISITED}-entry walk limit"
                )));
            }
            let path = match entry {
                Ok(e) => e.path(),
                Err(_) => continue,
            };
            if !path.is_dir() {
                continue;
            }
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
            Self::walk_skills_inner(&path, root, source, map, depth + 1, visited)?;
        }
        Ok(())
    }

    fn read_skill_file(path: &Path, name: &str, file: &str, source: &str) -> Result<Value> {
        let content = read_bounded(path, MAX_SKILL_FILE_BYTES)
            .map_err(|e| Error::Tool(format!("skill `{name}`: {e}")))?;
        let skill_dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
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

/// One discovered skill: its `/`-separated name, one-line description, and
/// origin (`workspace` or `global`).
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub source: String,
    pub location: String,
}

/// Scan the workspace and global skill roots for available skills, returning
/// name/description/source for each. Mirrors [`BuiltinTools::skills`] but
/// returns plain structs for prompt assembly (no JSON, no `BuiltinTools`).
///
/// # Errors
/// Returns an error when a configured skill root cannot be scanned safely.
pub fn scan_skill_summaries(root: &Path, skills_dir: Option<&Path>) -> Result<Vec<SkillSummary>> {
    let mut map: std::collections::BTreeMap<String, (String, String, String)> =
        std::collections::BTreeMap::new();
    if let Some(dir) = skills_dir {
        BuiltinTools::walk_skills(dir, "global", &mut map)?;
    }
    let ws_skills = root.join(".lofi").join("skills");
    BuiltinTools::walk_skills(&ws_skills, "workspace", &mut map)?;
    if map.len() > MAX_SKILLS {
        return Err(Error::Tool(format!(
            "skills: exceeded {MAX_SKILLS}-entry limit"
        )));
    }
    Ok(map
        .into_iter()
        .map(|(name, (desc, source, path))| SkillSummary {
            name,
            description: desc,
            source,
            location: path,
        })
        .collect())
}

fn validate_skill_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::Tool("skill: name must not be empty".into()));
    }
    if name.starts_with('/') || name.contains('\\') {
        return Err(Error::Tool(format!("skill: invalid name `{name}`")));
    }
    for component in name.split('/') {
        if component == ".." {
            return Err(Error::Tool(format!("skill: invalid name `{name}`")));
        }
    }
    Ok(())
}

fn read_description(path: &Path) -> Option<String> {
    let content = read_bounded(path, MAX_SKILL_FILE_BYTES).ok()?;
    crate::skill_metadata::description(&content).or_else(|| {
        path.parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(std::string::ToString::to_string)
    })
}

/// Read a file up to `max` bytes as a UTF-8 string, returning an error when
/// the file exceeds the cap or cannot be read.
fn read_bounded(path: &Path, max: usize) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    // Cap the read at max+1 so we can detect files that exceed the limit
    // even when the reported metadata size is zero or stale.
    let mut reader = std::io::Read::take(&mut file, (max as u64) + 1);
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut buf)?;
    if buf.len() > max {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("file exceeds {max}-byte limit"),
        ));
    }
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
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
            crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
            None,
            None,
            skills_dir,
        )
    }

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
            .skills(None)
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
            .skills(None)
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
            .skills(None)
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 1);
        assert_eq!(skills_arr[0]["source"], json!("workspace"));
        assert_eq!(
            skills_arr[0]["description"],
            json!("Project-specific deploy.")
        );
    }

    #[tokio::test]
    async fn skills_list_empty_when_no_dirs() {
        let dir = tempdir().unwrap();
        let v = tools(dir.path(), None).skills(None).await.unwrap();
        assert_eq!(v["skills"].as_array().unwrap().len(), 0);
    }
    #[tokio::test]
    async fn skills_search_filters_by_name_and_description() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        make_skill(skills.path(), "git-workflow", "# Git\n\nBranching help.\n");
        make_skill(skills.path(), "deploy", "# Deploy\n\nShip to prod.\n");
        make_skill(skills.path(), "review", "# Review\n\nRead a diff.\n");
        let t = tools(dir.path(), Some(skills.path().to_path_buf()));

        // Name match.
        let v = t.skills(Some("git")).await.unwrap();
        let arr = v["skills"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], json!("git-workflow"));

        // Description match (case-insensitive).
        let v = t.skills(Some("PROD")).await.unwrap();
        let arr = v["skills"].as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], json!("deploy"));

        // No match.
        let v = t.skills(Some("nonexistent")).await.unwrap();
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
    async fn skills_follows_symlinked_dir() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        let real = tempdir().unwrap();
        std::fs::write(real.path().join(SKILL_FILE), "Symlinked skill.\n").unwrap();
        std::os::unix::fs::symlink(real.path(), skills.path().join("linked")).unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills(None)
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
        std::os::unix::fs::symlink(
            real_file.path(),
            skills.path().join("linked-file").join(SKILL_FILE),
        )
        .unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill("linked-file")
            .await
            .unwrap();
        assert!(v["content"]
            .as_str()
            .unwrap()
            .contains("Symlinked SKILL.md"));
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
            .skills(None)
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
            .skills(None)
            .await
            .unwrap();
        let desc = v["skills"][0]["description"].as_str().unwrap();
        assert_eq!(desc, "my-skill");
    }

    #[tokio::test]
    async fn skills_ignores_non_skill_md() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        std::fs::create_dir_all(skills.path().join("foo")).unwrap();
        std::fs::write(skills.path().join("foo").join("README.md"), "not a skill\n").unwrap();
        make_skill(skills.path(), "bar", "a real skill\n");
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills(None)
            .await
            .unwrap();
        let skills_arr = v["skills"].as_array().unwrap();
        assert_eq!(skills_arr.len(), 1);
        assert_eq!(skills_arr[0]["name"], json!("bar"));
    }
}
