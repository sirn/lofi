//! Core-owned session write endpoint.
//!
//! Callers never assemble session events. The sink owns which session is
//! active. Durable appends go through [`crate::session::store::SessionCursor::record`].

use std::path::{Path, PathBuf};

use lofi_error::Result;
use lofi_types::RunModel;

use super::store::{self, SessionCursor, SessionStore};
use crate::shell::DirectShellResult;

#[derive(Debug)]
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
        let entry = store.find(cwd, id)?.ok_or_else(|| {
            lofi_error::Error::State(format!("no session matching id '{id}' for this workspace"))
        })?;
        let (cursor, snapshot) = entry.open_snapshot()?;
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
        let Some(entry) = store.most_recent(cwd)? else {
            return Ok(None);
        };
        let (cursor, snapshot) = entry.open_snapshot()?;
        Ok(Some((
            Self::resumed(cwd, cursor.clone())?,
            cursor,
            snapshot,
        )))
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
    pub fn record_user_bash(
        &mut self,
        result: &DirectShellResult,
        exclude_from_context: bool,
        model: &RunModel,
        system_prompt: &str,
    ) -> Result<(u64, u64)> {
        let cursor = self.cursor_or_create(model, system_prompt)?;
        cursor.record(super::recorder::SessionRecord::UserBash {
            result,
            exclude_from_context,
        })
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
        self.store.list_files_for_cwd(&self.cwd)
    }

    /// List the session entries (files with message counts and previews) for
    /// this workspace. Read-only; never mutates.
    ///
    /// # Errors
    /// Propagates directory read failures.
    pub fn workspace_entries(&self) -> Result<Vec<store::SessionEntry>> {
        self.store.list_for_cwd(&self.cwd)
    }

    /// Run a read-heavy transcript walk on the store's shared IO worker.
    /// Picker scans and tree hydration funnel here so their transient
    /// allocations reuse one arena instead of spawning a fresh thread per
    /// action.
    pub fn submit_io(&self, job: Box<dyn FnOnce(&mut super::io::WorkerState) + Send + 'static>) {
        super::io::submit(self.store.root().to_path_buf(), job);
    }
}
