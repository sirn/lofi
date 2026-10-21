//! The boundary events of one turn: the opening that names the turn, the
//! prompts it carries, and the terminal that closes it.
//!
//! Every transport ends a turn the same way: the transcript range is
//! committed first (the UI materializes it later from disk), then the banner
//! that carries the model, elapsed time and spend. Every transport opens one
//! the same way too. Keeping both ends in one place is what makes replay and
//! the live UI agree whichever transport ran the turn.

use lofi_types::{PromptKind, RunModel, StopReason, Usage};
use tokio::sync::mpsc::Sender;

use super::AgentEvent;
use crate::session::recorder::TurnOutcome;

/// What ended the turn, for the banner a caller renders.
pub(crate) struct TurnTerminal {
    pub model: RunModel,
    pub outcome: TurnOutcome,
    pub elapsed_ms: u64,
    pub cost: f64,
    pub usage: Usage,
    pub stop_reason: Option<StopReason>,
    /// Byte range the turn committed to the transcript, when it wrote one.
    pub committed: Option<(u64, u64)>,
}

/// The opening of one turn.
///
/// A turn announces itself exactly once: `RunStart` when it carries a prompt,
/// `TurnContinue` when it only continues an existing exchange. Each notice and
/// the input itself then publish one `Prompt` boundary. Nothing between those
/// events is this type's business: a provider round and a CLI child differ
/// there, and that is the transport's own loop.
pub(crate) struct TurnDriver {
    tx: Sender<AgentEvent>,
    opened: bool,
}

impl TurnDriver {
    pub(crate) fn new(tx: Sender<AgentEvent>) -> Self {
        Self { tx, opened: false }
    }

    /// Announce the turn, once. Returns false when the consumer is gone.
    pub(crate) async fn open(&mut self) -> bool {
        if self.opened {
            return true;
        }
        self.opened = true;
        !self.tx.is_closed() && self.tx.send(AgentEvent::RunStart).await.is_ok()
    }

    /// Publish one prompt boundary, announcing the turn on the first one.
    pub(crate) async fn prompt(&mut self, prompt: &str, kind: PromptKind) -> bool {
        self.open().await
            && self
                .tx
                .send(AgentEvent::Prompt {
                    prompt: prompt.to_owned(),
                    kind,
                })
                .await
                .is_ok()
    }

    /// Announce a turn that carries no new user message. The transcript
    /// records the boundary only, so live execution and replay agree.
    pub(crate) async fn continuation(&mut self) -> bool {
        if self.opened {
            return true;
        }
        self.opened = true;
        !self.tx.is_closed() && self.tx.send(AgentEvent::TurnContinue).await.is_ok()
    }
}

/// Emit the commit marker, then the terminal banner.
///
/// A detached turn stamps nothing: its consumer is gone and no durable
/// boundary may claim it finished.
pub(crate) async fn emit_terminal(tx: &Sender<AgentEvent>, terminal: TurnTerminal) {
    let TurnTerminal {
        model,
        outcome,
        elapsed_ms,
        cost,
        usage,
        stop_reason,
        committed,
    } = terminal;
    if tx.is_closed() || matches!(outcome, TurnOutcome::Detached) {
        return;
    }
    if let Some((byte_start, byte_end)) = committed {
        let _ = tx
            .send(AgentEvent::TurnCommitted {
                byte_start,
                byte_end,
            })
            .await;
    }
    let _ = match outcome {
        TurnOutcome::Finished => {
            tx.send(AgentEvent::TurnEnd {
                model,
                elapsed_ms,
                cost,
                usage,
                stop_reason,
            })
            .await
        }
        TurnOutcome::Failed(error) => {
            tx.send(AgentEvent::TurnFailed {
                model,
                elapsed_ms,
                error,
                cost,
                usage,
            })
            .await
        }
        TurnOutcome::ContextPressure => {
            tx.send(AgentEvent::ContextPressure {
                elapsed_ms,
                cost,
                usage,
            })
            .await
        }
        TurnOutcome::Cancelled => {
            tx.send(AgentEvent::TurnCancelled {
                model,
                elapsed_ms,
                cost,
                usage,
            })
            .await
        }
        TurnOutcome::Detached => Ok(()),
    };
}
