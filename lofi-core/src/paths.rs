use std::path::{Path, PathBuf};

use lofi_types::Access;

/// Shared view of a session's allowed paths: the roots the sandbox always
/// honors, the config-sourced grants, and the per-workspace remembered
/// grants. The `/paths` dialog reads and edits through this handle, so a
/// change applies to the next resolution without rebuilding the agent.
#[derive(Clone)]
pub struct PathsHandle {
    root: PathBuf,
    tmp_dir: Option<PathBuf>,
    skills_dir: Option<PathBuf>,
    memory: Option<PathBuf>,
    config: Vec<lofi_types::GrantedDir>,
}

/// One `/paths` dialog row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathRow {
    pub path: PathBuf,
    pub access: Access,
    pub source: PathSource,
}

/// Where a `/paths` row's access comes from. Only state rows are editable
/// from the dialog; the others say where to change them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSource {
    Workspace,
    Builtin,
    Config,
    State,
}

impl PathSource {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Builtin => "builtin",
            Self::Config => "config",
            Self::State => "state",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsSnapshot {
    pub rows: Vec<PathRow>,
}

impl PathsHandle {
    #[must_use]
    pub fn new(
        root: PathBuf,
        tmp_dir: Option<PathBuf>,
        skills_dir: Option<PathBuf>,
        memory: Option<PathBuf>,
        config: Vec<lofi_types::GrantedDir>,
    ) -> Self {
        Self {
            root,
            tmp_dir,
            skills_dir,
            memory,
            config,
        }
    }

    /// The rows to show: the workspace root, the builtin read roots, the
    /// config grants, then the remembered state grants.
    #[must_use]
    pub fn snapshot(&self) -> PathsSnapshot {
        let mut rows = vec![PathRow {
            path: self.root.clone(),
            access: Access::Write,
            source: PathSource::Workspace,
        }];
        for dir in [&self.tmp_dir, &self.skills_dir].into_iter().flatten() {
            rows.push(PathRow {
                path: dir.clone(),
                access: Access::Read,
                source: PathSource::Builtin,
            });
        }
        if let Some(memory) = &self.memory {
            rows.push(PathRow {
                path: memory.clone(),
                access: Access::Write,
                source: PathSource::Builtin,
            });
        }
        for grant in &self.config {
            rows.push(PathRow {
                path: grant.dir.clone(),
                access: grant.access,
                source: PathSource::Config,
            });
        }
        if let Ok(state) = crate::state::WorkspaceState::open(&self.root) {
            for grant in state.grants() {
                rows.push(PathRow {
                    path: grant.dir,
                    access: grant.access,
                    source: PathSource::State,
                });
            }
        }
        PathsSnapshot { rows }
    }

    /// Remember or update a state grant. Wider than a config entry for the
    /// same tree, the two combine additively.
    ///
    /// # Errors
    /// Returns the state-file write failure.
    pub fn set_grant(&self, dir: &Path, access: Access) -> lofi_error::Result<()> {
        let state = crate::state::WorkspaceState::open(&self.root)?;
        state.remember_grant(dir, access)
    }

    /// Forget one remembered state grant.
    ///
    /// # Errors
    /// Returns the state-file write failure.
    pub fn forget_grant(&self, dir: &Path) -> lofi_error::Result<()> {
        let state = crate::state::WorkspaceState::open(&self.root)?;
        state.forget_grant(dir)
    }
}
