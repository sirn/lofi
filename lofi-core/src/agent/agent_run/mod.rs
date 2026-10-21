#![allow(clippy::wildcard_imports)]

//! The agent entry points. The turn itself lives in the `turn_session` module;
//! this module only names the two ways a caller starts one.

pub(crate) mod messages;
#[cfg(test)]
mod tests;

#[cfg(test)]
pub(in crate::agent) use super::turn_session::MAX_RECOVERY_ROUNDS;
#[cfg(test)]
pub(in crate::agent) use messages::{assistant_announces_tool_intent, assistant_is_thinking_only};

use super::*;

impl Agent {
    /// Seeds a fresh user history and drives one turn.
    /// The configured system prompt is added to each provider request when the
    /// caller-owned history does not already contain one.
    /// If the receiver is dropped (the channel closes), the run exits gracefully.
    /// # Errors
    /// Propagates an error from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a tool result.
    pub async fn run(&self, user_prompt: String, tx: Sender<AgentEvent>) -> Result<()> {
        let mut messages = Vec::new();
        self.run_turn(
            &mut messages,
            TurnRequest::prompt(user_prompt, lofi_types::PromptKind::User),
            tx,
            None,
            RunControls::default(),
        )
        .await
    }

    /// Run a submitted prompt or continue the current turn without a prompt.
    /// Notices use the same durable prompt path as notices generated during a run.
    /// # Errors
    /// Propagates provider, tool execution, and transcript storage errors.
    pub async fn run_turn(
        &self,
        messages: &mut Vec<Message>,
        request: TurnRequest,
        tx: Sender<AgentEvent>,
        session: Option<&crate::session::store::SessionCursor>,
        controls: RunControls,
    ) -> Result<()> {
        super::turn_session::run(self, messages, request, tx, session, controls)
            .await
            .map(|_| ())
    }
}
