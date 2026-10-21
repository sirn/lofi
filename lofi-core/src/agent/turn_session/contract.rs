//! The seam: what a transport must produce, and what a turn may ask of it.

use std::sync::Arc;

use async_trait::async_trait;
use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, RunModel, StopReason};

use super::io::TurnIo;
use crate::retry::RetryPolicy;

/// How a transport records a tool call it runs outside the exchange loop.
///
/// A child process runs its own tools and calls back through the MCP bridge;
/// those blocks must reach the transcript in order, so the driver that owns
/// the child's message copy also owns this hook.
pub type ToolRecorder = Arc<dyn Fn(ContentBlock) -> Result<()> + Send + Sync>;

/// One exchange: the next assistant message, its tools run, and whether the
/// turn is done.
pub struct Exchange {
    /// The model asked for no more tools.
    pub finished: bool,
    pub stop_reason: Option<StopReason>,
    /// Something the turn must repair, in the turn's own terms.
    pub anomaly: Option<Anomaly>,
}

impl Exchange {
    #[must_use]
    pub fn done(stop_reason: Option<StopReason>) -> Self {
        Self {
            finished: true,
            stop_reason,
            anomaly: None,
        }
    }

    #[must_use]
    pub fn continued(stop_reason: Option<StopReason>) -> Self {
        Self {
            finished: false,
            stop_reason,
            anomaly: None,
        }
    }
}

/// A repair the turn owns the policy for. The transport only reports the fact.
pub enum Anomaly {
    /// A repeated reasoning pattern or tool cycle. The optional index is the
    /// assistant message the transport interrupted, which the repair removes.
    LoopDetected {
        detail: String,
        removal: Option<usize>,
    },
}

/// An exchange that did not complete.
pub struct ExchangeFailure {
    pub error: Error,
    /// The exchange reached the model and produced output, so the retry
    /// budget restarts.
    pub progress: bool,
    pub rollback: Rollback,
}

/// What the turn may undo, and therefore whether it may ask again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Rollback {
    /// Nothing ran that may not run again: drop what this exchange appended
    /// after its round start, record the discard, then use the ladder.
    Retry,
    /// Stop the turn. Whatever this exchange completed stays in the history.
    Stop,
}

impl From<Error> for ExchangeFailure {
    fn from(error: Error) -> Self {
        Self::failed(error)
    }
}

impl ExchangeFailure {
    #[must_use]
    pub fn failed(error: Error) -> Self {
        Self {
            error,
            progress: false,
            rollback: Rollback::Retry,
        }
    }

    #[must_use]
    pub fn stopped(error: Error) -> Self {
        Self {
            error,
            progress: false,
            rollback: Rollback::Stop,
        }
    }

    #[must_use]
    pub fn with_progress(mut self) -> Self {
        self.progress = true;
        self
    }
}

/// Facts the turn hands a driver once the transcript has landed.
pub struct Settlement {
    /// The turn's transcript is on disk.
    pub recorded: bool,
    /// The leaf the session committed, when it recorded one.
    pub leaf: Option<String>,
}

/// The turn's tuneables, resolved by the backend that knows the model.
pub struct TurnPolicy {
    pub auto_continue: lofi_types::AutoContinuePolicy,
    pub retry: RetryPolicy,
    /// The context fill above which the turn stops so the UI can compact.
    /// The backend resolves it from the window its model runs in minus the
    /// room reserved for output; None disables the cap.
    pub hard_cap: Option<u64>,
}

/// A turn-scoped transport.
///
/// The driver holds the per-turn state that must not leak across turns: the
/// loop detector, the omission notice, the child handle and its delivery
/// facts. It never settles: the turn does that, and then tells the driver what
/// landed.
#[async_trait]
pub trait ExchangeDriver: Send {
    /// Produce the next assistant message for this history and run the tools
    /// it asks for. Append exactly what the model produced, in order.
    async fn run(
        &mut self,
        io: &TurnIo,
        history: &mut Vec<Message>,
    ) -> std::result::Result<Exchange, ExchangeFailure>;

    /// The turn settled. Advance only what depends on the transcript landing.
    async fn settle(&mut self, io: &TurnIo, settled: Settlement);

    /// How a tool this driver runs outside the loop reaches the transcript.
    fn tool_recorder(&self) -> Option<ToolRecorder> {
        None
    }

    /// The model identity the terminal marker carries.
    fn run_model(&self) -> RunModel;
}
