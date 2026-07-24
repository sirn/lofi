//! Conversation persistence and the in-memory log.
//!
//! Two concerns live here:
//! - [`Session`] — a thin in-memory `Vec<Message>` wrapper with an associated
//!   system prompt, used by callers that want a log without persistence.
//! - [`store`] — JSONL transcript files under the agent state dir, used by the
//!   interactive TUI to auto-save and restore conversations.
//!
//! The transcript format is intentionally simple: the first line is a JSON
//! header ([`store::Header`]) carrying version/cwd/model/name metadata, and
//! every subsequent line is a serialized [`SessionEvent`] — a conversation
//! message, a tool-call timing, or a turn-end marker with cost/usage. The
//! engine appends these as a turn commits; the UI replays them, so live and
//! resumed state share one code path. Append-only writes make auto-save a
//! single `write_all` after each turn.

use lofi_types::Message;

pub mod recorder;
pub mod store;

/// An in-memory conversation log.
#[derive(Debug, Clone, Default)]
pub struct Session {
    messages: Vec<Message>,
    system_prompt: String,
}

impl Session {
    /// Construct a new session with the given system prompt and no messages.
    #[must_use]
    pub fn new(system_prompt: String) -> Self {
        Self {
            messages: Vec::new(),
            system_prompt,
        }
    }

    /// Append a message to the log.
    pub fn push(&mut self, message: Message) {
        self.messages.push(message);
    }

    /// Read-only view of the message log.
    #[must_use]
    pub fn as_slice(&self) -> &[Message] {
        &self.messages
    }

    /// The system prompt associated with this session.
    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// Number of messages in the log.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{ContentBlock, Role};

    #[test]
    fn push_and_as_slice() {
        let mut s = Session::new("sys".to_string());
        assert!(s.is_empty());
        s.push(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "hi".to_string(),
            }],
        });
        assert_eq!(s.len(), 1);
        let slice = s.as_slice();
        assert_eq!(slice[0].role, Role::User);
        assert_eq!(s.system_prompt(), "sys");
    }

    #[test]
    fn default_is_empty() {
        let s = Session::default();
        assert!(s.is_empty());
        assert!(s.system_prompt().is_empty());
    }
}
