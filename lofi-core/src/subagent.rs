//! Nested agent loop for `agent()` calls from inside the sandbox.
//!
//! `run` builds a fresh message history (system prompt + user prompt) and
//! drives a nested agent loop using the parent's tool registry and workspace
//! root. The single agent tool is the code-mode `exec` (see [`crate::code`]).
//!
//! ## No iteration cap (v1)
//!
//! There is deliberately **no iteration cap** in v1: the subagent relies on the
//! model's own termination (a final assistant turn with no tool call) and a
//! per-call timeout enforced by the provider/agent layer. A fixed cap would
//! truncate genuine work; a timeout bounds runaway loops without imposing an
//! arbitrary ceiling on useful runs.
//!
//! ## Wiring
//!
//! `agent::run_once` (the single-round-trip primitive) lands in a later step.
//! To avoid a circular dependency today, the nested loop is expressed in terms
//! of the [`RoundTrip`] trait: the agent module will implement it for its
//! runtime context, and `subagent::run` accepts any `R: RoundTrip`. This keeps
//! the module compiling and unit-testable without a real provider.

use std::path::PathBuf;
use std::time::Duration;

use futures::future::LocalBoxFuture;

use crate::error::{Error, Result};
use lofi_types::{ContentBlock, Message, Role};

/// Default per-round-trip timeout for a subagent call (120s).
pub const DEFAULT_ROUND_TRIP_TIMEOUT: Duration = Duration::from_mins(2);

/// One agent round-trip: feed the current message history to the model and
/// append the assistant's response. Returns `Ok(true)` when the assistant
/// finished the turn without requesting a tool call (loop should stop), or
/// `Ok(false)` when a tool was invoked and the loop should continue.
///
/// The returned future is a [`LocalBoxFuture`] (not `Send`): the round-trip
/// drives the code-mode sandbox, whose `rquickjs` runtime is `!Send` without
/// the `parallel` feature.
///
/// This trait decouples `subagent` from `agent`, which implements it later.
pub trait RoundTrip: Send + Sync {
    /// Perform a single model round-trip, appending to `messages`.
    fn round_trip<'a>(&'a self, messages: &'a mut Vec<Message>)
        -> LocalBoxFuture<'a, Result<bool>>;
}

/// Options for a subagent run.
#[derive(Debug, Clone)]
pub struct SubagentOptions {
    /// System prompt prefix.
    pub system: String,
    /// Per-round-trip timeout.
    pub timeout: Duration,
    /// Optional wall-clock deadline for the whole nested loop. Bounds
    /// runaway agents that keep completing within each per-round-trip
    /// timeout. `None` disables the total cap.
    pub total_timeout: Option<Duration>,
}

impl Default for SubagentOptions {
    fn default() -> Self {
        Self {
            system: String::new(),
            timeout: DEFAULT_ROUND_TRIP_TIMEOUT,
            total_timeout: None,
        }
    }
}

/// Shared, read-only context inherited from the parent agent.
#[derive(Debug, Clone)]
pub struct SubagentCtx {
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Named strings forwarded to the sandbox.
    pub strings: std::collections::HashMap<String, String>,
}

/// Build the initial message history (system + user prompt).
///
/// Exposed so the construction can be unit-tested without a provider.
#[must_use]
pub fn initial_history(system: &str, prompt: &str) -> Vec<Message> {
    let mut messages = Vec::new();
    if !system.is_empty() {
        messages.push(Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text {
                text: system.to_string(),
            }],
        });
    }
    messages.push(Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
    });
    messages
}

/// Run a nested agent loop and return the assistant's final text.
///
/// Builds a fresh history from `system` + `prompt`, then calls
/// `round_trip.round_trip(&mut messages)` repeatedly until the model emits a
/// final turn (no tool call) or a round-trip times out. The code-mode `exec`
/// tool runs against the parent's workspace root.
///
/// # Errors
/// Returns [`Error::Provider`] if every round-trip attempt fails, or
/// [`Error::Sandbox`] if a tool execution fails fatally.
pub async fn run(
    _parent: &SubagentCtx,
    round_trip: &dyn RoundTrip,
    prompt: &str,
    opts: &SubagentOptions,
) -> Result<String> {
    let mut messages = initial_history(&opts.system, prompt);
    let deadline = opts.total_timeout.map(|d| tokio::time::Instant::now() + d);

    loop {
        let now = tokio::time::Instant::now();
        if let Some(dl) = deadline {
            if now >= dl {
                return Err(Error::Provider("subagent total timeout exceeded".into()));
            }
        }
        // Cap each round by the remaining total budget so a round started
        // near the deadline can't run a full `opts.timeout` past it.
        let budget = match deadline {
            Some(dl) => opts.timeout.min(dl - now),
            None => opts.timeout,
        };
        // Apply the per-round-trip timeout so a stuck provider call can't
        // hang the nested loop indefinitely.
        let finished = tokio::time::timeout(budget, round_trip.round_trip(&mut messages))
            .await
            .map_err(|_| {
                Error::Provider(format!("subagent round-trip timed out after {budget:?}"))
            })??;
        if finished {
            return final_text(&messages)
                .ok_or_else(|| Error::Provider("subagent finished with no assistant text".into()));
        }
    }
}

/// Extract the last assistant message's concatenated text.
fn final_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .map(|m| {
            m.blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn initial_history_with_system() {
        let h = initial_history("you are helpful", "do X");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, Role::System);
        assert_eq!(h[1].role, Role::User);
    }

    #[test]
    fn initial_history_without_system() {
        let h = initial_history("", "do X");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].role, Role::User);
    }

    #[test]
    fn final_text_extracts_last_assistant_text() {
        let msgs = vec![
            Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text { text: "hi".into() }],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "hello ".into(),
                }],
            },
            Message {
                role: Role::Assistant,
                blocks: vec![ContentBlock::Text {
                    text: "world".into(),
                }],
            },
        ];
        assert_eq!(final_text(&msgs).as_deref(), Some("world"));
    }

    #[test]
    fn final_text_none_when_no_assistant_text() {
        let msgs = vec![Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        assert_eq!(final_text(&msgs), None);
    }
}
