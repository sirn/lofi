//! The single session-state representation. Every reader of conversation
//! history materializes lineage through [`SessionView`], which owns the
//! transcript-vs-memory decision. Callers never branch on whether the state
//! lives in a durable transcript or in unsettled agent memory: the view reads
//! from the transcript when one exists and derives the same event lineage from
//! the pending message tail otherwise. This is the one place that chooses a
//! backing source, so compact, recall, and context policy all consume an
//! identical lineage regardless of origin.

use std::sync::{Arc, Mutex};

use lofi_error::{Error, Result};
use lofi_types::{Message, SessionEvent, SessionEventKind};

use super::store::SessionCursor;

/// A source of settled conversation state.
enum SessionSource {
    /// The durable append-only transcript, read through its cursor.
    Transcript(SessionCursor),
    /// The unsettled in-memory message tail (no transcript persisted yet).
    Memory(Arc<Mutex<Vec<Message>>>),
}

/// Single representation of session state, derived from the transcript when
/// present and from memory otherwise. Construct one at the point of use.
pub struct SessionView {
    source: SessionSource,
}

impl SessionView {
    /// View backed by a durable transcript cursor.
    #[must_use]
    pub fn from_transcript(cursor: &SessionCursor) -> Self {
        Self {
            source: SessionSource::Transcript(cursor.clone()),
        }
    }

    /// View backed by the pending in-memory message tail.
    #[must_use]
    pub fn from_memory(history: &Arc<Mutex<Vec<Message>>>) -> Self {
        Self {
            source: SessionSource::Memory(Arc::clone(history)),
        }
    }

    /// Materialize the selected-lineage events as [`SessionEvent`]s, from the
    /// transcript when present and from the synthesized memory tail otherwise.
    /// Returns `Ok(None)` when there is no history at all, so callers treat an
    /// empty session uniformly without inspecting which source backs the view.
    ///
    /// The transcript source returns the compaction scope (the live range since
    /// the last compaction marker): the range compact and recall operate on. The
    /// memory source has no compaction marker yet, so its whole tail is live.
    /// Both yield a single logical lineage traversed identically downstream.
    ///
    /// # Errors
    /// Propagates transcript read failures, or a poisoned memory lock.
    pub fn session_events(&self) -> Result<Option<Vec<SessionEvent>>> {
        match &self.source {
            SessionSource::Transcript(cursor) => Some(cursor.load_compaction_events()).transpose(),
            SessionSource::Memory(history) => {
                let messages = history
                    .lock()
                    .map_err(|_| Error::State("agent history lock poisoned".to_string()))?;
                if messages.is_empty() {
                    return Ok(None);
                }
                Ok(Some(synthesize_lineage(&messages)))
            }
        }
    }
}

/// Shape the pending message tail into the same linear `SessionEvent` lineage
/// the transcript view produces: a parent-to-child chain of `Message` events.
/// Ids are positional; they need only be unique within this one lineage for the
/// lineage walk and compaction boundary logic, and are never persisted.
fn synthesize_lineage(messages: &[Message]) -> Vec<SessionEvent> {
    messages
        .iter()
        .enumerate()
        .map(|(index, message)| SessionEvent {
            id: index.to_string(),
            parent_id: index.checked_sub(1).map(|parent| parent.to_string()),
            kind: SessionEventKind::Message(message.clone()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use lofi_types::{ContentBlock, Role};

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: text.into() }],
            kind: Default::default(),
        }
    }

    #[test]
    fn memory_view_synthesizes_a_linear_lineage() {
        let history = Arc::new(Mutex::new(vec![user("one"), user("two")]));
        let view = SessionView::from_memory(&history);
        let events = view.session_events().unwrap().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, "0");
        assert_eq!(events[0].parent_id, None);
        assert_eq!(events[1].parent_id.as_deref(), Some("0"));
    }

    #[test]
    fn empty_memory_view_is_none() {
        let history = Arc::new(Mutex::new(Vec::new()));
        let view = SessionView::from_memory(&history);
        assert!(view.session_events().unwrap().is_none());
    }
}
