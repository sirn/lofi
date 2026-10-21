use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use lofi_types::{
    ContentBlock, Message, Model, NativeToolRecord, Role, RunModel, ServiceTier, ThinkingLevel,
    Usage,
};
use tokio::sync::mpsc::Sender;

use crate::backend;
use crate::models::ModelRegistry;
use crate::state;
use lofi_code::policy::ResolvedPolicy;
use lofi_code::{BashEnv, ExecCtx, ExecOptions, RecallFn, ResultFn, ToolEvent};
use lofi_error::{Error, Result};
use lofi_providers::ToolSchema;
use lofi_providers::{open, Provider};
use lofi_types::BashConfig;
#[cfg(test)]
use lofi_types::StreamingEvent;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::time::Duration;
use tokio::sync::oneshot;

#[derive(Debug)]
pub struct ConfirmRequest {
    pub id: u64,
    pub command: String,
    pub reason: Arc<Mutex<lofi_code::ConfirmReason>>,
    pub active: Arc<AtomicBool>,
    pub respond: oneshot::Sender<bool>,
}

mod agent_run;
#[cfg(test)]
use agent_run::assistant_announces_tool_intent;
pub(crate) use agent_run::messages::MAX_REQUEST_IMAGE_BYTES;
mod auto_mode;
mod event;
mod exec;
mod mailbox;
mod model;
pub(crate) mod native;
mod subagent;
mod tool_executor;
pub(crate) mod turn;
pub(crate) mod turn_session;
pub use lofi_code::SubagentOp;
pub(crate) use subagent::backend_hook;
pub use subagent::{
    SubagentNotice, SubagentNoticeKind, SubagentRegistry, SubagentRunning, SubagentSupport,
};
pub use tool_executor::{ToolCallContext, ToolExecutor};
pub(crate) use turn_session::ToolRecorder;
pub use turn_session::{RunControls, TurnInput, TurnRequest};

#[cfg(test)]
mod tests;

pub(crate) use auto_mode::collect_text;
pub use event::AgentEvent;
#[cfg(test)]
pub(crate) use exec::extract_code_prefix;
pub(crate) use exec::{cap_exec_result, cap_tool_result, CodePrefixDecoder};
pub use exec::{
    exec_input_code_and_label, exec_label, exec_result_display, exec_tool_schema, parse_exec_input,
};
pub(crate) use model::assemble_system_prompt;
pub(crate) use model::{parse_model_query, NO_MODELS_HINT};
pub use model::{rebuild_agent, select_model, select_model_with_level};

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");

/// Maximum cumulative bytes of streamed text/thinking/tool-input retained
/// for a single round, so a hostile or misbehaving endpoint sending many
/// small valid events cannot exhaust memory within the stream timeout.
const MAX_ROUND_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TOOL_RESULT_BYTES: usize = 50 * 1024;
/// Outer cap on the whole exec result sent back to the provider. Native
/// sub-tool results are already individually capped to `MAX_TOOL_RESULT_BYTES`;
/// this guards the aggregate (many sub-tool results + `logs`) so a long
/// `Promise.all` burst or a verbose `print` loop can't balloon the context.
pub(crate) const MAX_EXEC_RESULT_BYTES: usize = 200 * 1024;

const PER_EVENT_OVERHEAD: usize = 64;
/// User-role notice appended once per turn when the provider reports a
/// token-limit stop, nudging the model to pick up where it was cut off.
pub(crate) const TRUNCATION_CONTINUATION_PROMPT: &str =
    "Your previous response was cut off at the token limit. Continue where you left off.";
pub(crate) const THINKING_ONLY_CONTINUATION_PROMPT: &str = "Your previous response contained reasoning but no final answer. Continue the task and give the final answer.";
pub(crate) const LOST_TOOL_CONTINUATION_PROMPT: &str = "Your tool call was not received. Continue the task by issuing the required tool call again. If no tool call is required, give the final answer.";
pub(crate) const INTENT_CONTINUATION_PROMPT: &str = "Continue the task now. If the task is already complete, give the final answer. Do not only describe the next action.";

/// Spend of one complete usage report, or `None` when the model carries no
/// price. The same arithmetic bills a finished round and a one-shot call.
#[must_use]
pub fn usage_cost(model: &Model, usage: Usage) -> Option<f64> {
    let (Some(input), Some(output)) = (model.input_price, model.output_price) else {
        return model.per_request_price;
    };
    let cache_read = model.cache_read_price.unwrap_or(input);
    let cache_write = model.cache_write_price.unwrap_or(input);
    let mut cost = usage.input_tokens as f64 / 1_000_000.0 * input
        + usage.cache_read_tokens as f64 / 1_000_000.0 * cache_read
        + usage.cache_write_tokens as f64 / 1_000_000.0 * cache_write
        + usage.output_tokens as f64 / 1_000_000.0 * output;
    if let Some(per_request) = model.per_request_price {
        cost += per_request;
    }
    Some(cost)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Cheaply cloneable because the provider is held in an `Arc`.
#[derive(Clone)]
pub struct Agent {
    pub(crate) provider: Arc<dyn Provider>,
    pub(crate) model: Model,
    root: PathBuf,
    tmp_dir: PathBuf,
    tmp_lease: Option<Arc<state::SessionTempDir>>,
    retry: crate::retry::RetryPolicy,
    pub(crate) system_prompt: String,
    max_output_tokens: Option<u64>,
    compaction: lofi_types::CompactionConfig,
    bash_env: BashEnv,
    shell_policy: ResolvedPolicy,
    truncate: lofi_code::TruncatedCap,
    image: lofi_types::ImageConfig,
    auto_continue: lofi_types::AutoContinuePolicy,
    confirm_tx: Option<tokio::sync::mpsc::UnboundedSender<ConfirmRequest>>,
    confirm_counter: Arc<AtomicU64>,
    auto_mode: Option<lofi_code::AutoModeFn>,
    /// Config the evaluator was opened from, with its provider resolved.
    auto_mode_config: Option<Arc<lofi_types::Config>>,
    /// Turn context for the auto-mode evaluator: the request a command
    /// answers. The turn loop writes it at the start of each turn.
    auto_mode_hints: lofi_code::AutoModeHintHandle,
    /// Session-scoped `/policy` approval override. Shared with the TUI so
    /// the dialog pick reaches tools built by later execs, and cloned into
    /// every rebuilt agent so a model switch keeps the choice.
    policy_override: lofi_code::policy::PolicyOverride,
    skills_dir: Option<PathBuf>,
    /// Session-scoped background jobs. Shared with every exec so a job
    /// spawned in one round is visible to the next. The host shuts down
    /// survivors when the session ends.
    jobs: lofi_code::tools::JobRegistry,
    /// Owning subagent run id; set on child agents so their spawned jobs
    /// are tagged for kill-by-owner and the per-owner cap.
    job_owner: Option<String>,
    /// Mailbox capability of a child agent: its own inbox plus the right to
    /// write to the parent. `None` on a session agent, which addresses runs
    /// through the registry instead.
    mailbox: Option<mailbox::MailboxScope>,
    /// Session-shared sandbox thread, born with the first exec. Guest work
    /// on the single-threaded host runtime would freeze the UI; the worker
    /// keeps the sandbox off it for the agent's lifetime.
    exec_worker: std::sync::Arc<std::sync::OnceLock<lofi_code::ExecWorker>>,
    /// Registry and config the subagent hook needs to resolve model
    /// overrides. `None` disables `lofi.subagent` (nested subagent agents).
    subagent_support: Option<SubagentSupport>,
    /// Spawned subagents of this session, resumable by id.
    subagents: SubagentRegistry,
    /// The config and registry this backend was opened from. Model selection
    /// rebuilds the transport from here, so no caller keeps a second copy.
    pub(crate) source: Option<Arc<backend::NativeSource>>,
}

impl Agent {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Box<dyn Provider>,
        model: Model,
        root: PathBuf,
        tmp_dir: PathBuf,
        system_prompt: String,
        max_output_tokens: Option<u64>,
        compaction: lofi_types::CompactionConfig,
        bash: &BashConfig,
        env: &[(String, String)],
        truncate: lofi_types::TruncateConfig,
        image: lofi_types::ImageConfig,
        shell_policy_config: &lofi_types::ShellPolicyConfig,
    ) -> Self {
        let bash_env = crate::bash_env::resolve_bash_env(bash, env);
        let truncate = lofi_code::TruncatedCap {
            max_lines: truncate.max_lines,
            max_bytes: truncate.max_bytes,
        };
        let shell_policy = lofi_code::policy::defaults::resolve(shell_policy_config);
        Self {
            provider: Arc::from(provider),
            model,
            root,
            tmp_dir,
            tmp_lease: None,
            retry: crate::retry::RetryPolicy::default(),
            system_prompt,
            max_output_tokens,
            compaction,
            bash_env,
            shell_policy,
            truncate,
            image,
            auto_continue: lofi_types::AutoContinuePolicy::default(),
            confirm_tx: None,
            confirm_counter: Arc::new(AtomicU64::new(0)),
            auto_mode: None,
            auto_mode_config: None,
            auto_mode_hints: auto_mode::hint_handle(),
            policy_override: lofi_code::policy::PolicyOverride::default(),
            skills_dir: None,
            jobs: lofi_code::tools::JobRegistry::new(),
            job_owner: None,
            mailbox: None,
            exec_worker: std::sync::Arc::new(std::sync::OnceLock::new()),
            subagent_support: None,
            subagents: SubagentRegistry::default(),
            source: None,
        }
    }

    fn with_tmp_lease(mut self, lease: state::SessionTempDir) -> Self {
        let lease = Arc::new(lease);
        self.tmp_dir = lease.path().to_path_buf();
        self.tmp_lease = Some(lease);
        self
    }

    #[must_use]
    pub fn with_model(&self, provider: Box<dyn Provider>, model: Model) -> Self {
        Self {
            provider: Arc::from(provider),
            model,
            root: self.root.clone(),
            tmp_dir: self.tmp_dir.clone(),
            tmp_lease: self.tmp_lease.clone(),
            retry: self.retry,
            system_prompt: self.system_prompt.clone(),
            max_output_tokens: self.max_output_tokens,
            compaction: self.compaction.clone(),
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            truncate: self.truncate,
            image: self.image,
            auto_continue: self.auto_continue,
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            auto_mode_config: self.auto_mode_config.clone(),
            auto_mode_hints: self.auto_mode_hints.clone(),
            policy_override: self.policy_override.clone(),
            skills_dir: self.skills_dir.clone(),
            jobs: self.jobs.clone(),
            job_owner: self.job_owner.clone(),
            mailbox: self.mailbox.clone(),
            exec_worker: self.exec_worker.clone(),
            subagent_support: self.subagent_support.clone(),
            subagents: self.subagents.clone(),
            source: self.source.clone(),
        }
    }

    #[must_use]
    pub fn with_system_prompt(&self, system_prompt: String) -> Self {
        Self {
            provider: self.provider.clone(),
            model: self.model.clone(),
            root: self.root.clone(),
            tmp_dir: self.tmp_dir.clone(),
            tmp_lease: self.tmp_lease.clone(),
            retry: self.retry,
            system_prompt,
            max_output_tokens: self.max_output_tokens,
            compaction: self.compaction.clone(),
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            auto_mode_config: self.auto_mode_config.clone(),
            auto_mode_hints: self.auto_mode_hints.clone(),
            policy_override: self.policy_override.clone(),
            skills_dir: self.skills_dir.clone(),
            jobs: self.jobs.clone(),
            job_owner: self.job_owner.clone(),
            mailbox: self.mailbox.clone(),
            exec_worker: self.exec_worker.clone(),
            subagent_support: self.subagent_support.clone(),
            subagents: self.subagents.clone(),
            truncate: self.truncate,
            image: self.image,
            auto_continue: self.auto_continue,
            source: self.source.clone(),
        }
    }

    #[must_use]
    pub fn with_skills_dir(&self, skills_dir: Option<PathBuf>) -> Self {
        Self {
            provider: self.provider.clone(),
            model: self.model.clone(),
            root: self.root.clone(),
            tmp_dir: self.tmp_dir.clone(),
            tmp_lease: self.tmp_lease.clone(),
            retry: self.retry,
            system_prompt: self.system_prompt.clone(),
            max_output_tokens: self.max_output_tokens,
            compaction: self.compaction.clone(),
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            auto_mode_config: self.auto_mode_config.clone(),
            auto_mode_hints: self.auto_mode_hints.clone(),
            policy_override: self.policy_override.clone(),
            skills_dir,
            jobs: self.jobs.clone(),
            job_owner: self.job_owner.clone(),
            mailbox: self.mailbox.clone(),
            exec_worker: self.exec_worker.clone(),
            subagent_support: self.subagent_support.clone(),
            subagents: self.subagents.clone(),
            truncate: self.truncate,
            image: self.image,
            auto_continue: self.auto_continue,
            source: self.source.clone(),
        }
    }

    /// Replace every session-scoped field from an executor. Both backends own
    /// the same kind of executor, so a switch or a fork moves between them
    /// without copying twenty fields by hand.
    #[must_use]
    pub fn with_executor(&self, executor: &ToolExecutor) -> Self {
        Self {
            root: executor.root.clone(),
            tmp_dir: executor.tmp_dir.clone(),
            tmp_lease: executor.tmp_lease.clone(),
            bash_env: executor.bash_env.clone(),
            shell_policy: executor.shell_policy.clone(),
            truncate: executor.truncate,
            image: executor.image,
            confirm_tx: executor.confirm_tx.clone(),
            confirm_counter: executor.confirm_counter.clone(),
            auto_mode: executor.auto_mode.clone(),
            auto_mode_config: executor.auto_mode_config.clone(),
            auto_mode_hints: executor.auto_mode_hints.clone(),
            policy_override: executor.policy_override.clone(),
            skills_dir: executor.skills_dir.clone(),
            jobs: executor.jobs.clone(),
            job_owner: executor.job_owner.clone(),
            exec_worker: executor.exec_worker.clone(),
            mailbox: executor.mailbox.clone(),
            // Session state lives in the executor, so a backend opened from
            // the same executor joins the runs the session already has.
            subagents: executor.subagents.clone(),
            subagent_support: executor.subagent_support.clone(),
            ..self.clone()
        }
    }

    /// A new conversation that shares the backend configuration and nothing
    /// else: its own transcript slot, its own nested-subagent scope.
    #[must_use]
    pub fn forked(&self, executor: &ToolExecutor) -> Self {
        let mut child = self.with_executor(executor);
        child.mailbox = None;
        child.subagent_support = None;
        child.subagents = SubagentRegistry::default();
        child
    }

    #[must_use]
    pub fn with_source(&self, source: Arc<backend::NativeSource>) -> Self {
        Self {
            source: Some(source),
            ..self.clone()
        }
    }

    #[must_use]
    pub fn source(&self) -> Option<&Arc<backend::NativeSource>> {
        self.source.as_ref()
    }

    #[must_use]
    pub fn model(&self) -> &Model {
        &self.model
    }

    #[must_use]
    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    #[must_use]
    pub fn with_confirm_tx(
        &self,
        confirm_tx: tokio::sync::mpsc::UnboundedSender<ConfirmRequest>,
    ) -> Self {
        Self {
            confirm_tx: Some(confirm_tx),
            ..self.clone()
        }
    }

    /// Share the session's jobs, policy and sandbox with a backend.
    #[must_use]
    pub fn tool_executor(&self) -> ToolExecutor {
        ToolExecutor {
            root: self.root.clone(),
            tmp_dir: self.tmp_dir.clone(),
            tmp_lease: self.tmp_lease.clone(),
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            truncate: self.truncate,
            image: self.image,
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            auto_mode_config: self.auto_mode_config.clone(),
            auto_mode_hints: self.auto_mode_hints.clone(),
            policy_override: self.policy_override.clone(),
            skills_dir: self.skills_dir.clone(),
            jobs: self.jobs.clone(),
            job_owner: self.job_owner.clone(),
            exec_worker: self.exec_worker.clone(),
            subagent: self.subagent_hook(),
            mailbox: self.mailbox.clone(),
            subagents: self.subagents.clone(),
            subagent_support: self.subagent_support.clone(),
            models: self
                .subagent_support
                .as_ref()
                .map(SubagentSupport::models_fn),
        }
    }

    /// Keep execution state when switching from another backend.
    #[must_use]
    pub fn with_tool_executor(mut self, executor: ToolExecutor) -> Self {
        self.root = executor.root;
        self.tmp_dir = executor.tmp_dir;
        self.tmp_lease = executor.tmp_lease;
        self.bash_env = executor.bash_env;
        self.shell_policy = executor.shell_policy;
        self.truncate = executor.truncate;
        self.image = executor.image;
        self.confirm_tx = executor.confirm_tx;
        self.confirm_counter = executor.confirm_counter;
        self.auto_mode = executor.auto_mode;
        self.auto_mode_config = executor.auto_mode_config;
        self.auto_mode_hints = executor.auto_mode_hints;
        self.policy_override = executor.policy_override;
        self.skills_dir = executor.skills_dir;
        self.jobs = executor.jobs;
        self.exec_worker = executor.exec_worker;
        self.job_owner = executor.job_owner;
        // A fork built from this executor carries the mailbox the spawner
        // chose for the child; the session agent has none either way.
        self.mailbox = executor.mailbox;
        // Session state follows the executor, so switching transports keeps
        // the same runs and the same override resolution.
        self.subagents = executor.subagents;
        if self.subagent_support.is_none() {
            self.subagent_support = executor.subagent_support;
        }
        self
    }

    #[must_use]
    pub fn with_auto_mode(&self, auto_mode: lofi_code::AutoModeFn) -> Self {
        Self {
            auto_mode: Some(auto_mode),
            ..self.clone()
        }
    }

    /// The turn-context handle the evaluator reads. The turn loop replaces
    /// its contents at the start of every turn.
    #[must_use]
    pub fn auto_mode_hints(&self) -> &lofi_code::AutoModeHintHandle {
        &self.auto_mode_hints
    }

    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    #[must_use]
    pub fn tmp_dir(&self) -> &PathBuf {
        &self.tmp_dir
    }

    /// The session-scoped `/policy` approval override handle; the TUI
    /// writes to it, every exec reads from it.
    #[must_use]
    pub fn policy_override(&self) -> &lofi_code::policy::PolicyOverride {
        &self.policy_override
    }

    /// Whether auto-mode approval evaluation is configured for this agent.
    #[must_use]
    pub fn has_auto_mode(&self) -> bool {
        self.auto_mode.is_some()
    }

    /// The session's background-job registry. Cheap to clone (shares the
    /// same map); the UI clones it once to render the job list and badge
    /// and to kill jobs from the `/job` modal.
    #[must_use]
    pub fn jobs(&self) -> lofi_code::tools::JobRegistry {
        self.jobs.clone()
    }

    /// The session's subagent registry (shared by every clone). The UI keeps
    /// a handle so a new session can drop the entries of the old one.
    #[must_use]
    pub fn subagents(&self) -> SubagentRegistry {
        self.subagents.clone()
    }

    /// The raw model identity stamped onto [`AgentEvent::TurnEnd`] (and the
    /// transcript's `TurnEnd` marker, via the recorder). Built from the
    /// resolved [`Model`] so it is authoritative across a model switch on
    /// resume — the persisted turn keeps its original model, and the UI
    /// renders it to `provider/id:level` at display time rather than storing
    /// a formatted string.
    #[must_use]
    pub fn run_model(&self) -> RunModel {
        RunModel {
            provider: self.model.provider.clone(),
            id: self.model.id.clone(),
            selected: self.model.id.clone(),
            display: None,
            thinking: self.model.thinking.clone(),
            service_tier: self.model.service_tier.clone(),
        }
    }

    #[must_use]
    pub fn with_retry(mut self, retry: crate::retry::RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    #[must_use]
    pub fn with_auto_continue(mut self, policy: lofi_types::AutoContinuePolicy) -> Self {
        self.auto_continue = policy;
        self
    }

    #[must_use]
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

    /// Effective `max_tokens` for the next request, clipped so the request
    /// cannot demand more output than the remaining context window. Some
    /// providers reject `max_tokens >= context_window - input_tokens`; the
    /// prior round's [`Usage::context_tokens`] approximates the next
    /// request's input. Returns `None` (omit the field) when the
    /// window or model cap is unknown.
    #[must_use]
    fn clipped_max_tokens(&self, prev_input_tokens: Option<u64>) -> Option<u64> {
        let cap = self.max_output_tokens.or(self.model.max_tokens)?;
        let window = self.model.context_window?;
        let input = prev_input_tokens.unwrap_or(0);
        let headroom = window.saturating_sub(input);
        (headroom > 0).then(|| cap.min(headroom))
    }
}
