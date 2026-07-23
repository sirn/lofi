//! Skill discovery and reading.
//!
//! Skills are markdown files that provide reusable instructions or domain
//! knowledge the agent can load on demand. Two sources are scanned:
//!
//! - **Global** — `<skills_dir>/*.md` (the `<config_dir>/skills/` directory).
//! - **Per-workspace** — `<root>/.lofi/skills/*.md` (checked into the repo
//!   for project-specific skills).
//!
//! Skill names are the file stem (e.g. `git-workflow.md` → `git-workflow`).
//! When both sources define the same name, the per-workspace version wins
//! (it is more specific). The description is the first non-heading
//! non-empty line of the file.

#[allow(clippy::wildcard_imports)]
use super::*;
use lofi_error::{Error, Result};
use serde_json::{json, Value};

/// Maximum number of skill files scanned across both sources.
const MAX_SKILLS: usize = 500;

impl BuiltinTools {
    /// List available skills from the global and per-workspace directories.
    ///
    /// Each entry is `{ name, description, source }` where `source` is
    /// `"global"` or `"workspace"`. When both sources define the same name,
    /// only the workspace entry is returned. The result is sorted by name.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the number of skill files exceeds
    /// [`MAX_SKILLS`].
    #[allow(clippy::unused_async)]
    pub async fn skills(&self) -> Result<Value> {
        let entries = self.scan_skills()?;
        Ok(json!({ "ok": true, "skills": entries }))
    }

    /// Read a single skill by name. When both sources define the same name,
    /// the per-workspace version is returned.
    ///
    /// # Errors
    /// Returns [`Error::Tool`] if the skill is not found or the file cannot
    /// be read.
    #[allow(clippy::unused_async)]
    pub async fn skill(&self, name: &str) -> Result<Value> {
        // Reject names with path separators so a caller cannot escape the
        // skills directories.
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.contains("..")
        {
            return Err(Error::Tool(format!(
                "skill: invalid name `{name}`"
            )));
        }

        // Check per-workspace first (more specific).
        let ws_skills = self.root.join(".lofi").join("skills");
        let ws_path = ws_skills.join(format!("{name}.md"));
        if ws_path.is_file() {
            return Self::read_skill_file(&ws_path, name, "workspace");
        }

        // Then global.
        if let Some(dir) = &self.skills_dir {
            let g_path = dir.join(format!("{name}.md"));
            if g_path.is_file() {
                return Self::read_skill_file(&g_path, name, "global");
            }
        }

        Err(Error::Tool(format!(
            "skill `{name}` not found"
        )))
    }

    /// Scan both skill directories and collect sorted, de-duplicated entries.
    fn scan_skills(&self) -> Result<Vec<Value>> {
        // name → (description, source)
        let mut map: std::collections::BTreeMap<String, (String, String)> =
            std::collections::BTreeMap::new();

        // Global skills.
        if let Some(dir) = &self.skills_dir {
            Self::scan_skill_dir(dir, "global", &mut map)?;
        }

        // Per-workspace skills (override global on name collision).
        let ws_skills = self.root.join(".lofi").join("skills");
        Self::scan_skill_dir(&ws_skills, "workspace", &mut map)?;

        if map.len() > MAX_SKILLS {
            return Err(Error::Tool(format!(
                "skills: exceeded {MAX_SKILLS}-entry limit"
            )));
        }

        Ok(map
            .into_iter()
            .map(|(name, (desc, source))| {
                json!({
                    "name": name,
                    "description": desc,
                    "source": source,
                })
            })
            .collect())
    }

    /// Scan one directory for `*.md` skill files, inserting into `map`.
    #[allow(clippy::unnecessary_wraps)]
    fn scan_skill_dir(
        dir: &Path,
        source: &str,
        map: &mut std::collections::BTreeMap<String, (String, String)>,
    ) -> Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Ok(());
        };
        for entry in read_dir {
            let path = match entry {
                Ok(e) => e.path(),
                Err(_) => continue,
            };
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let desc = read_description(&path).unwrap_or_default();
            map.insert(stem.to_string(), (desc, source.to_string()));
        }
        Ok(())
    }

    /// Read a skill file and return the structured result.
    fn read_skill_file(
        path: &Path,
        name: &str,
        source: &str,
    ) -> Result<Value> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::Tool(format!("skill `{name}`: {e}"))
        })?;
        Ok(json!({
            "ok": true,
            "name": name,
            "source": source,
            "content": content,
        }))
    }
}

/// Extract the first non-heading, non-empty line from a markdown file as
/// the skill description. Falls back to the file name when the file is
/// empty or all headings.
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
    // Fall back to the file stem.
    path.file_stem()
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

    #[tokio::test]
    async fn skills_list_global() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        std::fs::write(
            skills.path().join("git-workflow.md"),
            "# Git Workflow\n\nStandard branching and commit workflow.\n",
        )
        .unwrap();
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
    async fn skills_list_workspace_overrides_global() {
        let dir = tempdir().unwrap();
        let g_skills = tempdir().unwrap();
        std::fs::write(
            g_skills.path().join("deploy.md"),
            "Global deploy instructions.\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join(".lofi").join("skills"))
            .unwrap();
        std::fs::write(
            dir.path().join(".lofi").join("skills").join("deploy.md"),
            "# Deploy\n\nProject-specific deploy.\n",
        )
        .unwrap();
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
        std::fs::write(
            skills.path().join("testing.md"),
            "# Testing\n\nRun cargo test and clippy.\n",
        )
        .unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skill("testing")
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["name"], json!("testing"));
        assert_eq!(v["source"], json!("global"));
        assert!(v["content"].as_str().unwrap().contains("Run cargo test"));
    }

    #[tokio::test]
    async fn skill_read_workspace_overrides_global() {
        let dir = tempdir().unwrap();
        let g_skills = tempdir().unwrap();
        std::fs::write(g_skills.path().join("ci.md"), "global ci\n").unwrap();
        std::fs::create_dir_all(dir.path().join(".lofi").join("skills"))
            .unwrap();
        std::fs::write(
            dir.path().join(".lofi").join("skills").join("ci.md"),
            "workspace ci\n",
        )
        .unwrap();
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
    async fn skill_description_skips_headings() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        std::fs::write(
            skills.path().join("lint.md"),
            "# Linting\n## Subsection\n\nRun clippy and fmt.\n",
        )
        .unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let desc = v["skills"][0]["description"].as_str().unwrap();
        assert_eq!(desc, "Run clippy and fmt.");
    }

    #[tokio::test]
    async fn skill_description_falls_back_to_stem() {
        let dir = tempdir().unwrap();
        let skills = tempdir().unwrap();
        std::fs::write(skills.path().join("empty.md"), "# Only Heading\n").unwrap();
        let v = tools(dir.path(), Some(skills.path().to_path_buf()))
            .skills()
            .await
            .unwrap();
        let desc = v["skills"][0]["description"].as_str().unwrap();
        assert_eq!(desc, "empty");
    }
}