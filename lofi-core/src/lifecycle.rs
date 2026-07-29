use std::sync::{Arc, Mutex};

use lofi_types::{
    CompactionConfig, ContentBlock, Message, Role, SessionEvent, SessionEventKind, Usage,
};

use crate::compact::{compact, compacted_history, CompactOptions, Compaction};
use crate::recall::{
    recall, recall_cursor, CompactionTarget, RecallOutcome, RecallRequest, RecallScope,
};
use crate::session::store::{CompactionCounts, EventIndex, IndexKind, SessionCursor};
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

/// Retained-memory measurement for the core-owned conversation history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HistoryStats {
    pub messages: usize,
    pub retained_bytes: usize,
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

    /// # Errors
    /// Returns a state error when the shared history lock is poisoned.
    pub fn replace_history(&self, messages: Vec<Message>) -> Result<()> {
        *self
            .history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))? = messages;
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
                retained_bytes: messages.capacity() * std::mem::size_of::<Message>()
                    + messages.iter().map(message_heap_bytes).sum::<usize>(),
            },
        )
    }

    pub fn set_context_window(&mut self, context_window: u64) {
        self.context_window = context_window;
        self.reset_compaction_policy();
    }

    pub fn reset_compaction_policy(&mut self) {
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
    pub fn compact(&mut self, cursor: Option<&SessionCursor>) -> Result<Option<Compaction>> {
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
        let new_history = compacted_history(&compaction);
        let mut history = self
            .history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?;
        if let Some(cursor) = cursor {
            cursor.append_compaction(
                &compaction.kept_messages,
                &compaction.summary,
                &compaction.summarized_range.clone().unwrap_or_default(),
                CompactionCounts {
                    summarized: compaction.summarized_count,
                    represented: compaction.represented_count,
                    kept: compaction.kept_count,
                },
            )?;
        }
        *history = new_history;
        drop(history);
        self.reset_compaction_policy();
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
        self.compact(cursor)
    }

    /// Apply hard-cap cooldown policy and compact when eligible.
    /// # Errors
    /// Propagates history and transcript failures.
    pub fn hard_compact(&mut self, cursor: Option<&SessionCursor>) -> Result<HardCompactOutcome> {
        if self.messages_since_last_compact(cursor)?
            < self.compaction.min_messages_between_hard_compacts
        {
            return Ok(HardCompactOutcome::Cooldown);
        }
        Ok(match self.compact(cursor)? {
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

    fn compaction_events(
        &self,
        cursor: Option<&SessionCursor>,
    ) -> Result<Option<Vec<SessionEvent>>> {
        if let Some(cursor) = cursor {
            return cursor.load_compaction_events().map(Some);
        }
        let messages = self
            .history
            .lock()
            .map_err(|_| Error::State("agent history lock poisoned".to_string()))?;
        if messages.is_empty() {
            return Ok(None);
        }
        Ok(Some(
            messages
                .iter()
                .enumerate()
                .map(|(index, message)| SessionEvent {
                    id: index.to_string(),
                    parent_id: index.checked_sub(1).map(|parent| parent.to_string()),
                    kind: SessionEventKind::Message(message.clone()),
                })
                .collect(),
        ))
    }
}

fn history_from_cursor(cursor: &SessionCursor, leaf_first_offsets: &[u64]) -> Result<Vec<Message>> {
    let mut messages = Vec::new();
    let mut skipping_failed_turn = false;
    let mut summary = None;
    cursor.visit_events(leaf_first_offsets, |event| {
        match event.kind {
            SessionEventKind::Compaction { summary: text, .. } if !text.is_empty() => {
                summary = Some(Message {
                    role: Role::User,
                    blocks: vec![ContentBlock::Text { text }],
                });
            }
            SessionEventKind::TurnFailed { .. } => skipping_failed_turn = true,
            SessionEventKind::TurnEnd { .. } => skipping_failed_turn = false,
            SessionEventKind::Message(message) if !skipping_failed_turn => messages.push(message),
            SessionEventKind::UserBash {
                command,
                output,
                exit_code,
                signal,
                duration_ms,
                truncated,
                cancelled,
                exclude_from_context: false,
            } if !skipping_failed_turn => {
                let result = crate::UserBashResult::from_session(
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
    Ok(messages)
}

fn message_heap_bytes(message: &Message) -> usize {
    message.blocks.capacity() * std::mem::size_of::<ContentBlock>()
        + message.blocks.iter().map(content_heap_bytes).sum::<usize>()
}

fn content_heap_bytes(block: &ContentBlock) -> usize {
    match block {
        ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.capacity(),
        ContentBlock::ToolUse { id, name, input } => {
            id.capacity() + name.capacity() + json_heap_bytes(input)
        }
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } => tool_use_id.capacity() + content.capacity(),
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
}
