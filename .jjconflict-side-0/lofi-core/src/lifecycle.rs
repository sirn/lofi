use std::sync::{Arc, Mutex};

use lofi_types::{
    CompactionConfig, ContentBlock, Message, PromptKind, Role, SessionEvent, SessionEventKind,
    Usage,
};

use crate::compact::{compact, compacted_history, CompactOptions, Compaction};
use crate::recall::{
    recall, recall_cursor, CompactionTarget, RecallOutcome, RecallRequest, RecallScope,
};
use crate::session::recorder::SessionRecord;
use crate::session::store::{CompactionCounts, EventIndex, IndexKind, SessionCursor};
use crate::session::view::SessionView;
use crate::{CodeCompactionHook, Error, Result};

/// Core-owned mutable state and policy for an agent conversation.
/// Frontends render lifecycle outcomes, but do not plan compactions,
/// reconstruct event histories, execute recall, or decide policy eligibility.
pub struct AgentLifecycle {
    history: Arc<Mutex<Vec<Message>>>,
    compaction: CompactionConfig,
    context_window: u64,
    previous_context_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum HardCompactOutcome {
    Compacted(Compaction),
    Cooldown,
    NotEnoughHistory,
}

/// Estimated retained allocations for the core-owned conversation history.
///
/// This accounts for collection and string capacities owned by messages. It
/// intentionally does not claim allocator precision (for example, JSON object
/// node overhead is implementation-specific).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryStats {
    pub messages: usize,
    pub estimated_retained_bytes: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineageJobReconciliation {
    pub killed: Vec<u64>,
    pub stale: Vec<u64>,
}

impl AgentLifecycle {
    #[must_use]
    pub fn new(compaction: CompactionConfig, context_window: u64) -> Self {
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
            compaction,
            context_window,
            previous_context_tokens: None,
        }
    }

    #[must_use]
    pub fn shared_history(&self) -> Arc<Mutex<Vec<Message>>> {
        Arc::clone(&self.history)
    }

    /// Seed the live system prompt at the front of an empty history. Matches
    /// the durable transcript's lineage-birth pin: only a history with no
    /// messages yet is a fresh lineage, so this is a no-op after resume or any
    /// prior turn. Returns `true` when the system was inserted.
    ///
    /// # Errors
    /// Returns a state error when the shared history lock is poisoned.
    pub fn seed_system(&self, system_prompt: &str) -> Result<bool> {
        if system_prompt.is_empty() {
            return Ok(false);
        }
        let mut history = self
            .history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?;
        if !history.is_empty() {
            return Ok(false);
        }
        history.push(Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text {
                text: system_prompt.to_string(),
            }],
            kind: PromptKind::default(),
        });
        Ok(true)
    }

    /// Reuses the existing buffer's capacity rather than dropping it and
    /// reallocating. Resume and `/tree` rollback both rebuild the full history
    /// each time; handing a fresh multi-MB `Vec` back to the allocator every
    /// cycle leaves glibc arenas bloated (freed-but-not-returned-to-OS), which
    /// surfaces as a one-time RSS step. Reusing the buffer avoids the churn.
    ///
    /// # Errors
    /// Returns a state error when the shared history lock is poisoned.
    pub fn replace_history(&self, messages: Vec<Message>) -> Result<()> {
        let mut history = self
            .history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?;
        if history.capacity() >= messages.len() {
            history.clear();
            history.extend(messages);
        } else {
            *history = messages;
        }
        Ok(())
    }

    /// Rebuild the active agent context from an indexed durable lineage.
    /// # Errors
    /// Propagates transcript reads and poisoned history state.
    pub fn restore_history(&mut self, cursor: &SessionCursor, index: &[EventIndex]) -> Result<()> {
        let mut start = 0;
        for (position, entry) in index.iter().enumerate().rev() {
            if entry.kind != IndexKind::Compaction {
                continue;
            }
            let event = cursor.event_at(entry.offset)?;
            if let SessionEventKind::Compaction {
                first_kept_entry_id,
                ..
            } = &event.kind
            {
                start = if first_kept_entry_id.is_empty() {
                    position
                } else {
                    index[..position]
                        .iter()
                        .position(|entry| entry.id.matches(first_kept_entry_id))
                        .unwrap_or(position)
                };
                break;
            }
        }
        let offsets: Vec<u64> = index[start..]
            .iter()
            .rev()
            .map(|entry| entry.offset)
            .collect();
        let messages = history_from_cursor(cursor, &offsets)?;
        self.replace_history(messages)?;
        self.reset_compaction_policy();
        Ok(())
    }

    /// Reconcile session-owned jobs after a branch switch has been loaded.
    /// Jobs acquired on another lineage are cancelled and removed. Started
    /// jobs that belong to this lineage but no longer have a live process are
    /// reported as stale for the frontend to present.
    ///
    /// # Errors
    /// Propagates lifecycle-marker reads. No jobs are changed if those reads
    /// fail.
    pub fn reconcile_jobs_after_lineage_switch(
        &self,
        cursor: &SessionCursor,
        index: &[EventIndex],
        jobs: &lofi_code::tools::JobRegistry,
    ) -> Result<LineageJobReconciliation> {
        let lineage = crate::session::replay::job_lifecycle_ids_at(cursor, index)?;
        tracing::info!(
            target: "lofi::reconcile",
            spawn_ids = ?lineage.started,
            live = ?jobs.live_ids(),
            "reconcile"
        );
        // A job that completed on a pruned branch left its JobFinished
        // marker behind.  Rollback to a point between start and finish
        // cuts the marker off the lineage, but the process already
        // exited — reporting it as stale is a false positive.
        let killed = jobs.kill_not_in(&lineage.started);
        let live = jobs.live_ids();
        let stale: Vec<u64> = lineage
            .outstanding
            .into_iter()
            .filter(|id| !live.contains(id))
            .collect();
        let stale = if stale.is_empty() {
            stale
        } else {
            let finished_elsewhere = cursor.finished_job_ids_anywhere(&stale)?;
            stale
                .into_iter()
                .filter(|id| !finished_elsewhere.contains(id))
                .collect()
        };
        Ok(LineageJobReconciliation { killed, stale })
    }

    /// # Errors
    /// Returns a state error when the shared history lock is poisoned.
    pub fn push_message(&self, message: Message) -> Result<()> {
        self.history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?
            .push(message);
        Ok(())
    }

    /// # Errors
    /// Returns a state error when the shared history lock is poisoned.
    pub fn clear_history(&mut self) -> Result<()> {
        self.history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?
            .clear();
        self.reset_compaction_policy();
        Ok(())
    }

    #[must_use]
    pub fn history_stats(&self) -> HistoryStats {
        self.history.lock().map_or_else(
            |_| HistoryStats::default(),
            |messages| HistoryStats {
                messages: messages.len(),
                estimated_retained_bytes: messages.capacity() * std::mem::size_of::<Message>()
                    + messages.iter().map(message_heap_bytes).sum::<usize>(),
            },
        )
    }

    pub fn set_context_window(&mut self, context_window: u64) {
        self.context_window = context_window;
        self.reset_compaction_policy();
    }

    fn reset_compaction_policy(&mut self) {
        self.previous_context_tokens = None;
    }

    #[must_use]
    pub fn compact_budget(&self) -> usize {
        self.compaction
            .soft_threshold(self.context_window)
            .or_else(|| self.compaction.hard_threshold(self.context_window))
            .map_or(0, |threshold| (threshold / 2) as usize)
    }

    /// Compact immediately, updating memory and the durable cursor as one
    /// logical operation. Ok(None) means there is not enough history.
    /// # Errors
    /// Propagates history-lock and transcript persistence failures.
    pub fn compact(
        &mut self,
        cursor: Option<&SessionCursor>,
        system_prompt: &str,
    ) -> Result<Option<Compaction>> {
        let Some(events) = self.compaction_events(cursor)? else {
            return Ok(None);
        };
        let options = CompactOptions {
            max_kept_tokens: self.compact_budget(),
            edit: self.compaction.edit.clone(),
            hooks: vec![Arc::new(CodeCompactionHook)],
        };
        let Some(compaction) = compact(&events, &options) else {
            return Ok(None);
        };
        let mut new_history = compacted_history(&compaction);
        if !system_prompt.is_empty() {
            new_history.insert(
                0,
                Message {
                    role: Role::System,
                    blocks: vec![ContentBlock::Text {
                        text: system_prompt.to_string(),
                    }],
                    kind: PromptKind::default(),
                },
            );
        }
        let mut history = self
            .history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?;
        if let Some(cursor) = cursor {
            // System is re-pinned in the same record so the next restore
            // finds this latest System first.
            cursor.record(SessionRecord::Compaction {
                kept_messages: &compaction.kept_messages,
                summary: &compaction.summary,
                summarized_range: &compaction.summarized_range.clone().unwrap_or_default(),
                counts: CompactionCounts {
                    summarized: compaction.summarized_count,
                    represented: compaction.represented_count,
                    kept: compaction.kept_count,
                },
                system_prompt,
            })?;
        }
        *history = new_history;
        drop(history);
        self.reset_compaction_policy();
        // /compact can finish with no following agent run.
        crate::malloc_trim::release_freed_memory();
        Ok(Some(compaction))
    }

    /// Evaluate the soft threshold at a settled model round and compact only
    /// on an upward crossing.
    /// # Errors
    /// Propagates compaction failures.
    pub fn auto_compact(
        &mut self,
        usage: Usage,
        cursor: Option<&SessionCursor>,
        system_prompt: &str,
    ) -> Result<Option<Compaction>> {
        if !self.compaction.auto.enable {
            return Ok(None);
        }
        let Some(threshold) = self.compaction.soft_threshold(self.context_window) else {
            return Ok(None);
        };
        let current = usage.input_tokens + usage.cache_read_tokens;
        if current <= threshold {
            self.previous_context_tokens = Some(current);
            return Ok(None);
        }
        if self
            .previous_context_tokens
            .is_some_and(|previous| previous > threshold)
        {
            return Ok(None);
        }
        self.compact(cursor, system_prompt)
    }

    /// Apply hard-cap cooldown policy and compact when eligible.
    /// # Errors
    /// Propagates history and transcript failures.
    pub fn hard_compact(
        &mut self,
        cursor: Option<&SessionCursor>,
        system_prompt: &str,
    ) -> Result<HardCompactOutcome> {
        if self.messages_since_last_compact(cursor)?
            < self.compaction.min_messages_between_hard_compacts
        {
            return Ok(HardCompactOutcome::Cooldown);
        }
        Ok(match self.compact(cursor, system_prompt)? {
            Some(compaction) => HardCompactOutcome::Compacted(compaction),
            None => HardCompactOutcome::NotEnoughHistory,
        })
    }

    /// Execute recall against the durable session when present, otherwise the
    /// in-memory conversation. None means no history exists yet.
    /// # Errors
    /// Returns a state error when in-memory history cannot be read.
    pub fn recall_line(
        &self,
        cursor: Option<&SessionCursor>,
        line: &str,
    ) -> Result<Option<RecallOutcome>> {
        let request = parse_recall_line(line);
        if let Some(cursor) = cursor {
            return Ok(Some(recall_cursor(cursor, &request)));
        }
        let Some(events) = self.compaction_events(None)? else {
            return Ok(None);
        };
        Ok(Some(recall(&events, &request)))
    }

    fn messages_since_last_compact(&self, cursor: Option<&SessionCursor>) -> Result<usize> {
        let Some(events) = self.compaction_events(cursor)? else {
            return Ok(usize::MAX);
        };
        let Some(last_compaction) = events
            .iter()
            .rposition(|event| matches!(event.kind, SessionEventKind::Compaction { .. }))
        else {
            return Ok(usize::MAX);
        };
        Ok(events[last_compaction + 1..]
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    SessionEventKind::Message(message) if message.role == Role::Assistant
                )
            })
            .count())
    }

    /// Materialize the session lineage for compact/recall/context policy.
    ///
    /// This is the only place the lifecycle chooses between the transcript and
    /// the in-memory tail, and it does so by building a [`SessionView`]: the view
    /// owns the source choice, so this method is only a constructor plus a
    /// materialize call. Every consumer (compact, auto/hard compact, recall,
    /// messages-since-compact) reads through this one view.
    fn compaction_events(
        &self,
        cursor: Option<&SessionCursor>,
    ) -> Result<Option<Vec<SessionEvent>>> {
        let view = cursor.map_or_else(
            || SessionView::from_memory(&self.history),
            SessionView::from_transcript,
        );
        view.session_events()
    }
}

fn history_from_cursor(cursor: &SessionCursor, leaf_first_offsets: &[u64]) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    let mut latest_system: Option<Message> = None;
    let mut skipping_failed_turn = false;
    let mut summary = None;
    cursor.visit_events(leaf_first_offsets, |event| {
        match event.kind {
            SessionEventKind::Compaction { summary: text, .. } if !text.is_empty() => {
                summary = Some(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text { text }],
                    kind: PromptKind::default(),
                });
            }
            SessionEventKind::TurnFailed { .. } => skipping_failed_turn = true,
            SessionEventKind::TurnEnd { .. } => skipping_failed_turn = false,
            SessionEventKind::Message(message) if !skipping_failed_turn => {
                // Leaf→root, the first (nearest) System event is the one in
                // force for this context; hoist it to the head instead of
                // leaving it inline. Older System events further up the chain
                // are pre-compaction context — never re-surfaced.
                if message.role == Role::System && latest_system.is_none() {
                    latest_system = Some(message);
                } else {
                    messages.push(message);
                }
            }
            SessionEventKind::UserShell {
                command,
                output,
                exit_code,
                signal,
                duration_ms,
                truncated,
                cancelled,
                exclude_from_context: false,
            } if !skipping_failed_turn => {
                let result = crate::DirectShellResult::from_session(
                    command,
                    output,
                    exit_code,
                    signal,
                    duration_ms,
                    truncated,
                    cancelled,
                );
                messages.push(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text {
                        text: result.context_text(),
                    }],
                    kind: PromptKind::default(),
                });
            }
            _ => {}
        }
        Ok(())
    })?;
    messages.reverse();
    if let Some(summary) = summary {
        messages.insert(0, summary);
    }
    if let Some(system) = latest_system {
        messages.insert(0, system);
    }
    Ok(messages)
}

fn message_heap_bytes(message: &Message) -> usize {
    message.blocks.capacity() * std::mem::size_of::<ContentBlock>()
        + message.blocks.iter().map(content_heap_bytes).sum::<usize>()
}

fn content_heap_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } => text.capacity(),
        ContentBlock::Thinking { text, signature } => {
            text.capacity() + signature.as_ref().map_or(0, String::capacity)
        }
        ContentBlock::ToolUse { id, name, input } => {
            id.capacity() + name.capacity() + json_heap_bytes(input)
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => tool_use_id.capacity() + content.capacity(),
        ContentBlock::PartSignature {
            provider,
            model,
            format,
            signature,
        } => {
            provider.capacity()
                + model.capacity()
                + match format {
                    lofi_types::PartSignatureFormat::OpenAiExtraContent { namespace } => {
                        namespace.capacity()
                    }
                    lofi_types::PartSignatureFormat::Google
                    | lofi_types::PartSignatureFormat::OpenAiReasoningDetail => 0,
                }
                + signature.capacity()
        }
        ContentBlock::Image { bytes, media_type } => bytes.capacity() + media_type.capacity(),
    }
}

fn json_heap_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(value) => value.capacity(),
        serde_json::Value::Array(values) => {
            values.capacity() * std::mem::size_of::<serde_json::Value>()
                + values.iter().map(json_heap_bytes).sum::<usize>()
        }
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.capacity() + json_heap_bytes(value))
            .sum(),
        _ => 0,
    }
}

fn parse_recall_line(line: &str) -> RecallRequest {
    let raw = line.trim().strip_prefix("/recall").unwrap_or("").trim();
    let mut scope = RecallScope::default();
    let mut query = Vec::new();
    let mut page = 1usize;
    for token in raw.split_whitespace() {
        if let Some(value) = token.strip_prefix("scope:") {
            scope = match value {
                "all" => RecallScope::All,
                "lineage" => RecallScope::Lineage,
                "latest" => RecallScope::Compaction(CompactionTarget::Latest),
                value if value.starts_with("compaction:") => value
                    .strip_prefix("compaction:")
                    .and_then(|index| index.parse().ok())
                    .map_or(RecallScope::Lineage, |index| {
                        RecallScope::Compaction(CompactionTarget::Index(index))
                    }),
                _ => RecallScope::Lineage,
            };
        } else if let Some(value) = token.strip_prefix("page:") {
            page = value.parse::<usize>().unwrap_or(1).max(1);
        } else {
            query.push(token);
        }
    }
    let query = query.join(" ");
    RecallRequest {
        query: (!query.is_empty()).then_some(query),
        scope,
        page,
        expand: Vec::new(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn recall_command_parsing_stays_in_core() {
        let request = parse_recall_line("/recall scope:compaction:2 page:3 needle");
        assert_eq!(request.page, 3);
        assert_eq!(request.query.as_deref(), Some("needle"));
        assert!(matches!(
            request.scope,
            RecallScope::Compaction(CompactionTarget::Index(2))
        ));
    }

    #[test]
    fn compact_budget_uses_soft_threshold() {
        let mut config = CompactionConfig::default();
        config.auto.context_ratio = Some(0.5);
        let lifecycle = AgentLifecycle::new(config, 100_000);
        assert_eq!(lifecycle.compact_budget(), 25_000);
    }

    #[test]
    fn seed_system_populates_empty_history_once() {
        let lifecycle = AgentLifecycle::new(CompactionConfig::default(), 100_000);
        assert!(lifecycle.seed_system("sys").unwrap());
        assert!(
            !lifecycle.seed_system("sys-again").unwrap(),
            "second seed must be a no-op"
        );
        let history = lifecycle.shared_history().lock().unwrap().clone();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].role, Role::System);
        assert_eq!(history.iter().filter(|m| m.role == Role::System).count(), 1);
    }

    #[test]
    fn seed_system_skips_after_any_message_and_ignores_empty() {
        let lifecycle = AgentLifecycle::new(CompactionConfig::default(), 100_000);
        assert!(!lifecycle.seed_system("").unwrap());
        lifecycle
            .push_message(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "hi".into() }],
                kind: PromptKind::default(),
            })
            .unwrap();
        assert!(
            !lifecycle.seed_system("sys").unwrap(),
            "a history carrying a resume/turn is not a birth point"
        );
        assert_eq!(lifecycle.history_stats().messages, 1);
    }

    #[test]
    fn job_release_does_not_attach_after_owner_branch_is_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
        let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
        let root = cursor.leaf_id();
        let owner = cursor.record_job_started(42).unwrap();

        cursor.restore_branch(root).unwrap();
        cursor.record_job_finished(&owner, 42).unwrap();

        let snapshot = cursor.snapshot().unwrap();
        let lineage =
            crate::session::replay::job_lifecycle_ids_at(&cursor, &snapshot.index).unwrap();
        assert!(lineage.started.is_empty());
        assert!(lineage.outstanding.is_empty());
    }

    #[test]
    fn lineage_job_reconciliation_reports_only_outstanding_missing_jobs() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
        let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
        let mut started = [SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::JobStarted { job_id: 42 },
        }];
        cursor.append_events(&mut started).unwrap();
        let lifecycle = AgentLifecycle::new(CompactionConfig::default(), 100_000);
        let jobs = lofi_code::tools::JobRegistry::new();

        let snapshot = cursor.snapshot().unwrap();
        let reconciliation = lifecycle
            .reconcile_jobs_after_lineage_switch(&cursor, &snapshot.index, &jobs)
            .unwrap();
        assert!(reconciliation.killed.is_empty());
        assert_eq!(reconciliation.stale, vec![42]);

        cursor.record_job_finished(&started[0].id, 42).unwrap();
        let snapshot = cursor.snapshot().unwrap();
        let reconciliation = lifecycle
            .reconcile_jobs_after_lineage_switch(&cursor, &snapshot.index, &jobs)
            .unwrap();
        assert!(reconciliation.killed.is_empty());
        assert!(reconciliation.stale.is_empty());
    }

    #[test]
    fn reconciliation_ignores_jobs_finished_on_a_pruned_branch() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
        let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
        let owner = cursor.record_job_started(42).unwrap();
        cursor.record_job_finished(&owner, 42).unwrap();
        // Roll back to the JobStarted event so the JobFinished marker lands on
        // the pruned branch while JobStarted stays on the selected lineage.
        cursor.restore_branch(Some(owner)).unwrap();

        let lifecycle = AgentLifecycle::new(CompactionConfig::default(), 100_000);
        // An empty registry models the state after a prior branch switch
        // already dropped the job, or a resumed process that never re-registered
        // it. Without the finished-anywhere check this reports a false stale.
        let jobs = lofi_code::tools::JobRegistry::new();
        let snapshot = cursor.snapshot().unwrap();
        let reconciliation = lifecycle
            .reconcile_jobs_after_lineage_switch(&cursor, &snapshot.index, &jobs)
            .unwrap();
        assert!(reconciliation.killed.is_empty());
        assert!(
            reconciliation.stale.is_empty(),
            "a job with a JobFinished marker elsewhere must not be stale: {:?}",
            reconciliation.stale
        );
    }

    #[test]
    fn restore_history_keeps_cancelled_turn_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
        let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
        let mut events = vec![
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text { text: "go".into() }],
                    kind: PromptKind::default(),
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::Message(Message {
                    role: Role::Assistant,
                    blocks: vec![ContentBlock::Text {
                        text: "partial".into(),
                    }],
                    kind: PromptKind::default(),
                }),
            },
            SessionEvent {
                id: String::new(),
                parent_id: None,
                kind: SessionEventKind::TurnCancelled {
                    model: "p/m".into(),
                    elapsed_ms: 5,
                    cost: 0.0,
                    usage: Usage::default(),
                },
            },
        ];
        cursor.append_events(&mut events).unwrap();
        let snapshot = cursor.snapshot().unwrap();
        let mut lifecycle = AgentLifecycle::new(CompactionConfig::default(), 100_000);

        lifecycle.restore_history(&cursor, &snapshot.index).unwrap();

        let history = lifecycle.shared_history();
        let messages = history.lock().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, Role::User);
        assert!(matches!(
            &messages[1].blocks[..],
            [ContentBlock::Text { text }] if text == "partial"
        ));
    }
    #[test]
    fn compact_emits_system_before_history_and_restore_recovers_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::session::store::SessionStore::new(dir.path().join("sessions"));
        let cursor = store.create_cursor(dir.path(), &"p/m".into()).unwrap();
        cursor.append_system("instructions v1").unwrap();
        let make_msg = |role: Role, text: &str| SessionEvent {
            id: String::new(),
            parent_id: None,
            kind: SessionEventKind::Message(Message {
                role,
                blocks: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
                kind: PromptKind::default(),
            }),
        };
        let mut events = vec![make_msg(Role::User, "u0"), make_msg(Role::Assistant, "a0")];
        for turn in 1..=4 {
            events.push(make_msg(Role::User, &format!("u{turn}")));
            events.push(make_msg(Role::Assistant, &format!("a{turn}")));
        }
        cursor.append_events(&mut events).unwrap();
        let mut lifecycle = AgentLifecycle::new(CompactionConfig::default(), 1_000);
        let snapshot = cursor.snapshot().unwrap();
        lifecycle.restore_history(&cursor, &snapshot.index).unwrap();
        let live_before = lifecycle.shared_history().lock().unwrap().clone();
        assert_eq!(live_before.len(), 11);
        assert_eq!(live_before[0].role, Role::System);

        let compaction = lifecycle
            .compact(Some(&cursor), "instructions v1")
            .unwrap()
            .expect("compact produced a result");
        assert!(!compaction.summary.is_empty());

        let live = lifecycle.shared_history().lock().unwrap().clone();
        assert!(!live.is_empty());
        assert_eq!(live[0].role, Role::System);
        assert_eq!(live[1].role, Role::User);

        let logged = cursor.load_events().unwrap();
        let last_pos = logged
            .iter()
            .rposition(|event| {
                matches!(
                    &event.kind,
                    SessionEventKind::Message(m) if m.role == Role::System
                )
            })
            .expect("a System event exists on the log");
        assert!(matches!(
            &logged[last_pos].kind,
            SessionEventKind::Message(m) if matches!(&m.blocks[..], [ContentBlock::Text { text }] if text == "instructions v1")
        ));

        let snapshot = cursor.snapshot().unwrap();
        lifecycle.restore_history(&cursor, &snapshot.index).unwrap();
        let restored = lifecycle.shared_history().lock().unwrap().clone();
        assert_eq!(restored[0].role, Role::System);
        assert_eq!(
            restored.iter().filter(|m| m.role == Role::System).count(),
            1,
            "exactly one System after restore: {restored:?}"
        );
    }

    #[test]
    fn history_stats_measure_core_owned_capacities() {
        let mut lifecycle = AgentLifecycle::new(CompactionConfig::default(), 100_000);
        let text = String::with_capacity(4_096);
        lifecycle
            .push_message(Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text { text }],
                kind: PromptKind::default(),
            })
            .unwrap();

        let stats = lifecycle.history_stats();
        assert_eq!(stats.messages, 1);
        assert!(
            stats.estimated_retained_bytes >= 4_096,
            "text capacity must be represented: {stats:?}"
        );

        lifecycle.clear_history().unwrap();
        let cleared = lifecycle.history_stats();
        assert_eq!(cleared.messages, 0);
        assert!(
            cleared.estimated_retained_bytes < stats.estimated_retained_bytes,
            "clearing drops nested allocations while retaining at most the outer vector: {cleared:?}"
        );
    }
}
