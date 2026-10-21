use std::sync::Arc;

use async_trait::async_trait;
use lofi_error::Result;
use lofi_types::{Message, ModelChoice, RunModel, ServiceTier, ThinkingLevel, Usage};
use tokio::sync::mpsc;

use crate::agent::turn_session::{ExchangeDriver, TurnPolicy};
use crate::agent::{AgentEvent, RunControls, ToolExecutor, TurnRequest};
use crate::session::recorder::TurnOutcome;
use crate::session::store::SessionCursor;

/// A conversation engine. Every model runs behind this interface, whether the
/// engine is an HTTP provider driven by Lofi's own tool loop or a CLI child
/// that owns its loop and calls back through the exec bridge.
pub type BackendRef = Arc<dyn Backend>;

/// What the user selected inside one backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSelection {
    /// Model id as the backend names it; "default" means whatever the backend
    /// runs without an explicit choice.
    pub model: String,
    pub thinking: ThinkingLevel,
    pub service_tier: ServiceTier,
}

/// A one-shot text call: no tools, no conversation, no turn slot.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    pub prompt: String,
    pub max_tokens: Option<u64>,
}

/// The answer of a one-shot text call.
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    pub usage: Usage,
    /// Unknown spend is None, never zero.
    pub cost: Option<f64>,
}

/// The abstraction above the transport. Callers never learn whether the model
/// is an HTTP endpoint or a child process.
#[async_trait]
pub trait Backend: Send + Sync {
    /// The provider or backend key this instance was configured under.
    fn name(&self) -> &str;

    /// The current model, effort and service tier inside this backend.
    fn selection(&self) -> ModelSelection;

    /// Capabilities of the current selection, as the model picker shows them.
    fn choice(&self) -> ModelChoice;

    fn system_prompt(&self) -> &str;

    /// Session-scoped execution state: jobs, policy, auto mode, subagents.
    fn tool_executor(&self) -> ToolExecutor;

    /// The identity recorded on turn markers.
    fn run_model(&self) -> RunModel {
        let selection = self.selection();
        RunModel {
            provider: self.name().to_owned(),
            id: selection.model.clone(),
            selected: selection.model.clone(),
            display: None,
            thinking: selection.thinking.clone(),
            service_tier: selection.service_tier.clone(),
        }
    }

    /// The same conversation with another model selected.
    ///
    /// # Errors
    /// Returns a configuration error for a model, thinking level or service
    /// tier this backend does not declare.
    fn with_selection(&self, query: &str) -> Result<BackendRef>;

    #[must_use]
    fn with_tool_executor(&self, executor: ToolExecutor) -> BackendRef;

    /// Spawned subagents of this session. The registry lives in the
    /// executor, so a CLI backend exposes the same runs an HTTP one does.
    /// A backend cannot opt out: an engine without subagents would surface
    /// here as a silent absence instead of a compile error.
    fn subagents(&self) -> crate::SubagentRegistry;

    /// The lofi.subagent hook, or None where nested subagents are disabled.
    fn subagent_hook(&self) -> Option<lofi_code::SubagentFn>;

    /// Route shell approvals to the host UI.
    #[must_use]
    fn with_confirm_tx(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<crate::agent::ConfirmRequest>,
    ) -> BackendRef {
        let executor = self.tool_executor().with_confirm_tx(tx);
        self.with_tool_executor(executor)
    }

    #[must_use]
    fn with_system_prompt(&self, prompt: String) -> BackendRef;

    /// A new conversation with the same configuration and selection. Used for
    /// subagents, so a child never shares a transcript or a turn slot with its
    /// parent.
    #[must_use]
    fn fork(&self, executor: ToolExecutor) -> BackendRef;

    /// Open the transport of one turn.
    ///
    /// The driver holds the per-turn state that must not leak across turns:
    /// the loop detector, the omission notice, the child handle and its
    /// delivery facts. It never settles; the turn does that.
    ///
    /// # Errors
    /// Propagates configuration and state failures.
    fn open_turn(&self) -> Result<Box<dyn ExchangeDriver>>;

    /// The tuneables of a turn on this backend: auto-continue, the retry
    /// ladder, and the room the provider needs for output.
    fn turn_policy(&self) -> TurnPolicy;

    /// The context window this backend runs in, before a turn reports one.
    fn context_window(&self) -> Option<u64> {
        None
    }

    /// Run one turn: record the input, drive the exchanges, settle.
    ///
    /// The body of every implementation is one call to
    /// `crate::agent::turn_session::run`, so retry, recovery, auto-continue,
    /// discard, record, usage and the terminal marker cannot disagree between
    /// transports.
    ///
    /// The outcome is typed, so a caller never classifies a run by reading the
    /// cancel flag or the events afterwards.
    ///
    /// # Errors
    /// Propagates transport, tool and transcript errors.
    async fn run_turn(
        &self,
        messages: &mut Vec<Message>,
        request: TurnRequest,
        events: mpsc::Sender<AgentEvent>,
        cursor: Option<&SessionCursor>,
        controls: RunControls,
    ) -> Result<TurnOutcome>;

    /// Answer one prompt with text only.
    ///
    /// The call shares no conversation and no turn slot with [`Backend::run_turn`], so a
    /// backend may answer while a turn is in flight on the same instance.
    /// This is what lets the shell-approval evaluator run on any backend,
    /// including the one whose turn is waiting for the approval.
    ///
    /// # Errors
    /// Propagates transport, protocol and configuration errors.
    async fn complete(&self, request: CompletionRequest) -> Result<Completion>;
}
