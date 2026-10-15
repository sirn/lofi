//! Core-owned session write endpoint.
//!
//! Callers never assemble session events. The sink owns which session is
//! active. Durable appends go through [`crate::session::store::SessionCursor::record`].

use std::path::{Path, PathBuf};

use lofi_error::Result;
use lofi_types::RunModel;

use super::store::{self, SessionCursor, SessionStore};
use crate::shell::UserShellResult;

#[derive(Debug, Clone)]
pub struct SessionSink {
    store: SessionStore,
    cwd: PathBuf,
    cursor: Option<SessionCursor>,
}

impl SessionSink {
    /// Open a sink targeting the shared session store for this workspace.
    ///
    /// # Errors
    /// Propagates session-store open failures.
    pub fn open(cwd: &Path) -> Result<Self> {
        Ok(Self {
            store: SessionStore::open()?,
            cwd: cwd.to_path_buf(),
            cursor: None,
        })
    }

    /// Wrap an existing (resumed or re-selected) cursor.
    ///
    /// # Errors
    /// Propagates session-store open failures.
    pub fn resumed(cwd: &Path, cursor: SessionCursor) -> Result<Self> {
        Ok(Self {
            store: SessionStore::open()?,
            cwd: cwd.to_path_buf(),
            cursor: Some(cursor),
        })
    }

    /// Build a sink from an explicit store root instead of the shared
    /// on-disk one, so tests can point at a temporary root.
    #[must_use]
    pub fn with_store(store: SessionStore, cwd: &Path, cursor: Option<SessionCursor>) -> Self {
        Self {
            store,
            cwd: cwd.to_path_buf(),
            cursor,
        }
    }

    /// Resolve a resume target by session id prefix, returning the sink bound
    /// to it together withits cursor and snapshot, so the caller never opens a
    /// store itself.
    ///
    /// # Errors
    /// Propagates store open/lookup failures, and `Error::State` when no
    /// session matches `id`.
    pub fn resume_by_id(
        cwd: &Path,
        id: &str,
    ) -> Result<(Self, SessionCursor, store::SessionSnapshot)> {
        let store = SessionStore::open()?;
        // Subagent transcripts are not user sessions: they never match a
        // resume id, so they cannot make a prefix ambiguous or stop the
        // lookup with a rejection.
        let matches: Vec<_> = store
            .list_files_for_cwd(cwd)?
            .into_iter()
            .filter(|file| !file.meta().subagent && file.id().starts_with(id))
            .collect();
        if matches.len() > 1 {
            return Err(lofi_error::Error::State(format!(
                "ambiguous session id '{id}'"
            )));
        }
        let Some(file) = matches.into_iter().next() else {
            return Err(lofi_error::Error::State(format!(
                "no session matching id '{id}' for this workspace"
            )));
        };
        let (cursor, snapshot) = file.open_snapshot()?;
        Ok((Self::resumed(cwd, cursor.clone())?, cursor, snapshot))
    }

    /// Resolve the most recent session for `cwd`, if any. Returns `None`
    /// when the workspace has no sessions.
    ///
    /// # Errors
    /// Propagates store open/lookup failures.
    pub fn resume_last(
        cwd: &Path,
    ) -> Result<Option<(Self, SessionCursor, store::SessionSnapshot)>> {
        let store = SessionStore::open()?;
        // Files come newest-first. Open directly rather than going through
        // `SessionEntry`: entries carry picker display metadata (message
        // counts, previews) whose computation indexes each transcript in
        // full — a whole extra scan of every session in this workspace on
        // every `--continue`. A file that fails to open behaves as absent,
        // the way unloadable files were ineligible before.
        for file in store.list_files_for_cwd(cwd)? {
            // Filter on the listed header first: open_snapshot indexes the
            // whole transcript, and a workspace with many subagent
            // transcripts would pay that cost for each of them.
            if file.meta().subagent {
                continue;
            }
            if let Ok((cursor, snapshot)) = file.open_snapshot() {
                return Ok(Some((
                    Self::resumed(cwd, cursor.clone())?,
                    cursor,
                    snapshot,
                )));
            }
        }
        Ok(None)
    }

    /// Borrow the active cursor, if one exists yet.
    #[must_use]
    pub fn cursor(&self) -> Option<&SessionCursor> {
        self.cursor.as_ref()
    }

    /// Replace the active cursor (session or tree switch).
    pub fn set_cursor(&mut self, cursor: SessionCursor) {
        self.cursor = Some(cursor);
    }

    /// Detach the active cursor so the next persisted action creates a new session.
    pub fn clear_cursor(&mut self) {
        self.cursor = None;
    }

    /// Return the active cursor, creating the session file on first use. This
    /// is the single place new session files come into existence, and a fresh
    /// lineage is pinned with its system prompt as the first event — exactly
    /// the way a compaction re-emits its boundary. Reusing an existing cursor
    /// never re-pins; restore re-reads the system event from the log.
    ///
    /// # Errors
    /// Propagates session-file creation and transcript append failures.
    pub fn cursor_or_create(
        &mut self,
        model: &RunModel,
        system_prompt: &str,
    ) -> Result<SessionCursor> {
        if self.cursor.is_none() {
            let cursor = self.store.create_cursor(&self.cwd, model)?;
            if !system_prompt.is_empty() {
                cursor.record(super::recorder::SessionRecord::System {
                    prompt: system_prompt,
                })?;
            }
            self.cursor = Some(cursor);
        }
        self.cursor.clone().ok_or_else(|| {
            lofi_error::Error::State("session cursor missing after create".to_string())
        })
    }

    /// Record a completed user-shell command and return its byte range.
    ///
    /// # Errors
    /// Propagates session-file creation and transcript I/O failures.
    pub fn record_user_shell(
        &mut self,
        result: &UserShellResult,
        exclude_from_context: bool,
        model: &RunModel,
        system_prompt: &str,
    ) -> Result<(u64, u64)> {
        let cursor = self.cursor_or_create(model, system_prompt)?;
        cursor.record(super::recorder::SessionRecord::UserShell {
            result,
            exclude_from_context,
        })
    }

    /// Record subagent spend that no turn claimed, flushed at session switch
    /// or quit. No-op when no session is open.
    ///
    /// # Errors
    /// Propagates transcript I/O failures.
    pub fn record_subagent_usage(&self, usage: lofi_types::Usage, cost: f64) -> Result<()> {
        let Some(cursor) = &self.cursor else {
            return Ok(());
        };
        cursor.record(super::recorder::SessionRecord::SubagentUsage { usage, cost })?;
        Ok(())
    }

    /// Move the active session head to the given entry, returning the snapshot
    /// of the newly selected lineage.
    ///
    /// # Errors
    /// Returns an error when no session is active or the switch fails.
    pub fn switch_branch(&self, entry_id: String) -> Result<store::SessionSnapshot> {
        let cursor = self
            .cursor
            .as_ref()
            .ok_or_else(|| lofi_error::Error::State("no active session to branch".to_string()))?;
        cursor.switch_branch(entry_id)
    }

    /// Restore the active session head after a failed `switch_branch`, returning
    /// the restored snapshot. Pass None to reattach the root, or the previous
    /// leaf id to restore a real head.
    ///
    /// # Errors
    /// Returns an error when no session is active or the restore fails.
    pub fn restore_branch(&self, leaf: Option<String>) -> Result<store::SessionSnapshot> {
        let cursor = self
            .cursor
            .as_ref()
            .ok_or_else(|| lofi_error::Error::State("no active session to branch".to_string()))?;
        cursor.restore_branch(leaf)
    }

    /// List the session files for this workspace. Read-only; never mutates.
    ///
    /// # Errors
    /// Propagates directory read failures.
    pub fn workspace_sessions(&self) -> Result<Vec<store::SessionFile>> {
        // Subagent transcripts are not user sessions: they stay out of the
        // picker, and interactive resume rejects them.
        Ok(self
            .store
            .list_files_for_cwd(&self.cwd)?
            .into_iter()
            .filter(|file| !file.meta().subagent)
            .collect())
    }

    /// List the session entries (files with message counts and previews) for
    /// this workspace. Read-only; never mutates.
    ///
    /// # Errors
    /// Propagates directory read failures.
    pub fn workspace_entries(&self) -> Result<Vec<store::SessionEntry>> {
        self.store.list_for_cwd(&self.cwd)
    }

    /// Subagent transcripts spawned by the session `parent`, newest first.
    /// Read-only; never mutates.
    ///
    /// # Errors
    /// Propagates directory read failures.
    pub fn subagent_sessions(&self, parent: &str) -> Result<Vec<store::SessionFile>> {
        self.store.list_subagent_files_for_cwd(&self.cwd, parent)
    }

    /// Run a read-heavy transcript walk on the store's shared IO worker.
    /// Picker scans and tree hydration funnel here so their transient
    /// allocations reuse one arena instead of spawning a fresh thread per
    /// action.
    pub fn submit_io(&self, job: Box<dyn FnOnce(&mut super::io::WorkerState) + Send + 'static>) {
        super::io::submit(self.store.root().to_path_buf(), job);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::RunModel;
    use tempfile::tempdir;

    // Point LOFI_STATE_HOME at a temp dir; restores the previous value on drop.
    struct StateEnv {
        prev: Option<std::ffi::OsString>,
        _dir: tempfile::TempDir,
    }

    impl StateEnv {
        fn new() -> Self {
            let dir = tempdir().unwrap();
            let prev = std::env::var_os("LOFI_STATE_HOME");
            std::env::set_var("LOFI_STATE_HOME", dir.path());
            Self { prev, _dir: dir }
        }
    }

    impl Drop for StateEnv {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("LOFI_STATE_HOME", v),
                None => std::env::remove_var("LOFI_STATE_HOME"),
            }
        }
    }

    #[test]
    fn picker_and_continue_exclude_subagent_transcripts() {
        let _env = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let _state = StateEnv::new();
        let cwd = std::path::Path::new("/tmp/project");
        let model: RunModel = "p/m".into();

        let store = store::SessionStore::open().unwrap();
        let normal = store.create_cursor(cwd, &model).unwrap();
        let sub = store
            .create_subagent_cursor(cwd, &model, Some("parent-x"))
            .unwrap();
        assert_ne!(normal.id(), sub.id());

        let sink = SessionSink::with_store(store::SessionStore::open().unwrap(), cwd, None);
        let files = sink.workspace_sessions().unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0].id(), normal.id());
        let entries = sink.workspace_entries().unwrap();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(!entries[0].meta.subagent);

        let (_, _, snapshot) = SessionSink::resume_last(cwd).unwrap().unwrap();
        assert!(!snapshot.meta.subagent, "--continue landed on a subagent");

        // Subagent transcripts never match a resume id: their flag keeps
        // them out of the picker, so a resumed child would become invisible.
        let err = SessionSink::resume_by_id(cwd, &sub.id()).unwrap_err();
        assert!(err.to_string().contains("no session matching id"), "{err}");
    }
}
