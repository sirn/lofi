//! Nested agent loop for `agent()` calls from inside the sandbox.
//!
//! `run` builds a fresh message history (system prompt + user prompt) and
//! drives a nested agent loop using the parent's tool registry and workspace
//! root. The single agent tool is the code-mode `exec` (see [`lofi_code`]).
//!
//! ## No iteration or wall-clock cap (v1)
//!
//! There is deliberately no fixed iteration or wall-clock cap: either would
//! truncate useful long-running work. Provider streams are bounded by an idle
//! timeout, native tools carry their own timeouts, and synchronous guest code
//! is bounded by the sandbox CPU interrupt budget (see [`lofi_code`]).
//!
//! ## Wiring
//!
//! To avoid a circular dependency, the nested loop is expressed in terms of
//! the [`RoundTrip`] trait: `agent::run_once` implements it for the agent
//! runtime context, and `subagent::run` accepts any `R: RoundTrip`.

use std::path::PathBuf;
use std::time::Instant;

use futures::future::LocalBoxFuture;

use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, Role};

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
    fn round_trip<'a>(
        &'a self,
        messages: &'a mut Vec<Message>,
    ) -> LocalBoxFuture<'a, Result<SubagentRound>>;
}

/// Accounting returned by one nested provider round.
#[derive(Debug, Clone, Copy)]
pub struct SubagentRound {
    /// Whether the assistant finished without requesting another tool call.
    pub finished: bool,
    /// This round's token usage.
    pub usage: lofi_types::Usage,
    /// This round's model cost.
    pub cost: f64,
}

/// Options for a subagent run.
#[derive(Debug, Clone, Default)]
pub struct SubagentOptions {
    /// System prompt prefix.
    pub system: String,
}

/// Summary of a completed synchronous subagent run.
#[derive(Debug, Clone)]
pub struct SubagentResult {
    /// Final assistant text.
    pub text: String,
    /// Number of provider rounds (including the final text-only round).
    pub rounds: u64,
    /// Aggregated token usage across every round.
    pub usage: lofi_types::Usage,
    /// Aggregated model cost across every round.
    pub cost: f64,
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
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
/// final turn (no tool call). The code-mode `exec` tool runs against the
/// parent's workspace root. Provider idle and native-tool limits still apply.
///
/// # Errors
/// Returns [`Error::Provider`] if every round-trip attempt fails, or
/// [`Error::Sandbox`] if a tool execution fails fatally.
pub async fn run(
    _parent: &SubagentCtx,
    round_trip: &dyn RoundTrip,
    prompt: &str,
    opts: &SubagentOptions,
) -> Result<SubagentResult> {
    let started = Instant::now();
    let mut messages = initial_history(&opts.system, prompt);
    let mut rounds = 0u64;
    let mut usage = lofi_types::Usage::default();
    let mut cost = 0.0;

    loop {
        // Do not impose a wall-clock deadline here. Provider streams already
        // carry an idle timeout, and native tools carry their own limits;
        // productive reasoning and long-running tools must not be cut off by
        // the old blanket 120-second round timeout.
        let round = round_trip.round_trip(&mut messages).await?;
        rounds = rounds.saturating_add(1);
        usage.input_tokens = usage.input_tokens.saturating_add(round.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(round.usage.output_tokens);
        usage.cache_read_tokens = usage
            .cache_read_tokens
            .saturating_add(round.usage.cache_read_tokens);
        usage.cache_write_tokens = usage
            .cache_write_tokens
            .saturating_add(round.usage.cache_write_tokens);
        cost += round.cost;
        if round.finished {
            let text = final_text(&messages).ok_or_else(|| {
                Error::Provider("subagent finished with no assistant text".into())
            })?;
            return Ok(SubagentResult {
                text,
                rounds,
                usage,
                cost,
                duration_ms: started.elapsed().as_millis() as u64,
            });
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

    struct TwoRoundTrip {
        calls: std::sync::atomic::AtomicU64,
    }

    impl RoundTrip for TwoRoundTrip {
        fn round_trip<'a>(
            &'a self,
            messages: &'a mut Vec<Message>,
        ) -> LocalBoxFuture<'a, Result<SubagentRound>> {
            Box::pin(async move {
                let call = self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if call == 1 {
                    messages.push(Message {
                        role: Role::Assistant,
                        blocks: vec![ContentBlock::Text {
                            text: "done".into(),
                        }],
                    });
                }
                Ok(SubagentRound {
                    finished: call == 1,
                    usage: lofi_types::Usage {
                        input_tokens: 10,
                        output_tokens: 2,
                        cache_read_tokens: 3,
                        cache_write_tokens: 1,
                    },
                    cost: 0.25,
                })
            })
        }
    }

    #[tokio::test]
    async fn run_returns_structured_accounting() {
        let runner = TwoRoundTrip {
            calls: std::sync::atomic::AtomicU64::new(0),
        };
        let parent = SubagentCtx {
            root: PathBuf::new(),
            strings: std::collections::HashMap::new(),
        };
        let result = run(&parent, &runner, "go", &SubagentOptions::default())
            .await
            .unwrap();
        assert_eq!(result.text, "done");
        assert_eq!(result.rounds, 2);
        assert_eq!(result.usage.input_tokens, 20);
        assert_eq!(result.usage.output_tokens, 4);
        assert_eq!(result.usage.cache_read_tokens, 6);
        assert_eq!(result.usage.cache_write_tokens, 2);
        assert!((result.cost - 0.5).abs() < f64::EPSILON);
    }

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
