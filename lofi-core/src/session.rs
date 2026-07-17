//! In-memory conversation log.
//!
//! A thin `Vec<Message>` wrapper with an associated system prompt. There is
//! no persistence in v1 — the log lives only for the process lifetime. The
//! interactive TUI (a later step) holds one of these to render history and
//! feed the agent loop; `Agent::run` maintains its own working history while
//! a run is in flight, so this type is deliberately minimal.

use lofi_types::Message;

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
