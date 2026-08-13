use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use lofi_types::{
    ContentBlock, Message, Model, NativeToolRecord, Role, RunModel, StreamingEvent, ThinkingLevel,
    Usage,
};
use tokio::sync::mpsc::Sender;

use crate::config_loader::load_config_or_default;
use crate::models::ModelRegistry;
use crate::session::recorder::{SessionRecord, SessionRecorder, TurnOutcome};
use crate::state;
use lofi_code::policy::ResolvedPolicy;
use lofi_code::{exec, BashEnv, ExecCtx, ExecOptions, RecallFn, ResultFn, ToolEvent};
use lofi_error::{Error, Result};
use lofi_providers::MessageAssembler;
use lofi_providers::ToolSchema;
use lofi_providers::{open, Provider};
use lofi_types::BashConfig;
use std::sync::atomic::{AtomicU64, Ordering};
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
mod auto_mode;
mod event;
mod exec;
mod model;

#[cfg(test)]
mod tests;

pub use event::AgentEvent;
#[cfg(test)]
pub(crate) use exec::extract_code_prefix;
pub(crate) use exec::{cap_exec_result, cap_tool_result, CodePrefixDecoder};
pub use exec::{
    exec_input_code_and_label, exec_label, exec_result_display, exec_tool_schema, parse_exec_input,
};
pub use model::{build_agent, rebuild_agent, select_model};

pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");

/// Per-chunk idle budget: if no SSE event arrives for this long the stream
/// is treated as stuck and aborted. This is intentionally an *idle* timeout
/// rather than a wall-clock cap so a long but productive reasoning-model turn
/// (which can stream for several minutes) is not cut off mid-response. The
/// HTTP client has no overall timeout for the same reason.
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Maximum cumulative bytes of streamed text/thinking/tool-input retained
/// for a single round, so a hostile or misbehaving endpoint sending many
/// small valid events cannot exhaust memory within the stream timeout.
const MAX_ROUND_BYTES: usize = 32 * 1024 * 1024;
const MAX_TOOL_RESULT_BYTES: usize = 50 * 1024;
/// Outer cap on the whole exec result sent back to the provider. Native
/// sub-tool results are already individually capped to `MAX_TOOL_RESULT_BYTES`;
/// this guards the aggregate (many sub-tool results + `logs`) so a long
/// `Promise.all` burst or a verbose `print` loop can't balloon the context.
const MAX_EXEC_RESULT_BYTES: usize = 200 * 1024;

const PER_EVENT_OVERHEAD: usize = 64;
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TurnStats {
    turn_start: Instant,
    tool_starts: HashMap<String, Instant>,
    tool_elapsed: HashMap<String, Duration>,
    thinking_elapsed: Vec<Duration>,
    native_tools: Vec<NativeToolRecord>,
    cost: f64,
    usage: Usage,
}

impl TurnStats {
    fn new() -> Self {
        Self {
            turn_start: Instant::now(),
            tool_starts: HashMap::new(),
            tool_elapsed: HashMap::new(),
            thinking_elapsed: Vec::new(),
            native_tools: Vec::new(),
            cost: 0.0,
            usage: Usage::default(),
        }
    }

    fn tool_start(&mut self, id: &str) {
        self.tool_starts.insert(id.to_string(), Instant::now());
    }

    fn tool_end(&mut self, id: &str) -> u64 {
        let ms = self
            .tool_starts
            .get(id)
            .map_or(0, |s| s.elapsed().as_millis() as u64);
        self.tool_elapsed
            .insert(id.to_string(), Duration::from_millis(ms));
        ms
    }

    fn add_usage(&mut self, usage: Usage, model: &Model) {
        self.usage = usage;
        if let (Some(ip), Some(op)) = (model.input_price, model.output_price) {
            // `input_tokens` excludes the cached slice (see provider usage
            // parsers). Cache reads and writes are billed at their own rates
            // when the model reports them; otherwise they fall through to
            // the input rate so the full prompt is still accounted for.
            let cache_read_rate = model.cache_read_price.unwrap_or(ip);
            let cache_write_rate = model.cache_write_price.unwrap_or(ip);
            self.cost += usage.input_tokens as f64 / 1_000_000.0 * ip
                + usage.cache_read_tokens as f64 / 1_000_000.0 * cache_read_rate
                + usage.cache_write_tokens as f64 / 1_000_000.0 * cache_write_rate
                + usage.output_tokens as f64 / 1_000_000.0 * op;
        }
        if let Some(pr) = model.per_request_price {
            self.cost += pr;
        }
    }

    /// Snapshot the turn's accumulators for the session recorder. The order
    /// of `tool_elapsed` matches insertion (i.e. first-seen tool-call order),
    /// and `thinking_elapsed` is in emission order — both preserved by the
    /// recorder so the resumed view's markers match the live one's.
    fn summary(&self, elapsed_ms: u64) -> crate::session::recorder::TurnSummary {
        let mut tool_elapsed: Vec<(String, u64)> = self
            .tool_elapsed
            .iter()
            .map(|(id, d)| (id.clone(), d.as_millis() as u64))
            .collect();
        tool_elapsed.sort_by(|a, b| a.0.cmp(&b.0));
        crate::session::recorder::TurnSummary {
            elapsed_ms,
            cost: self.cost,
            usage: self.usage,
            tool_elapsed,
            thinking_elapsed: self
                .thinking_elapsed
                .iter()
                .map(|d| d.as_millis() as u64)
                .collect(),
            native_tools: self.native_tools.clone(),
        }
    }
}

/// Cheaply cloneable because the provider is held in an `Arc`.
#[derive(Clone)]
pub struct Agent {
    provider: Arc<dyn Provider>,
    model: Model,
    root: PathBuf,
    tmp_dir: PathBuf,
    tmp_lease: Option<Arc<state::SessionTempDir>>,
    retry: crate::retry::RetryPolicy,
    system_prompt: String,
    max_output_tokens: Option<u64>,
    reserved_context_tokens: u64,
    bash_env: BashEnv,
    shell_policy: ResolvedPolicy,
    truncate: lofi_code::TruncatedCap,
    confirm_tx: Option<tokio::sync::mpsc::UnboundedSender<ConfirmRequest>>,
    confirm_counter: Arc<AtomicU64>,
    auto_mode: Option<lofi_code::AutoModeFn>,
    skills_dir: Option<PathBuf>,
    /// Session-scoped background jobs. Shared with every exec so a job
    /// spawned in one round is visible to the next. The host shuts down
    /// survivors when the session ends.
    jobs: lofi_code::tools::JobRegistry,
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
        reserved_context_tokens: u64,
        bash: &BashConfig,
        truncate: lofi_types::TruncateConfig,
        shell_policy_config: &lofi_types::ShellPolicyConfig,
    ) -> Self {
        let bash_env = crate::bash_env::resolve_bash_env(bash);
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
            reserved_context_tokens,
            bash_env,
            shell_policy,
            truncate,
            confirm_tx: None,
            confirm_counter: Arc::new(AtomicU64::new(0)),
            auto_mode: None,
            skills_dir: None,
            jobs: lofi_code::tools::JobRegistry::new(),
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
            reserved_context_tokens: self.reserved_context_tokens,
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            truncate: self.truncate,
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            skills_dir: self.skills_dir.clone(),
            jobs: self.jobs.clone(),
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
            reserved_context_tokens: self.reserved_context_tokens,
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            skills_dir: self.skills_dir.clone(),
            jobs: self.jobs.clone(),
            truncate: self.truncate,
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
            reserved_context_tokens: self.reserved_context_tokens,
            bash_env: self.bash_env.clone(),
            shell_policy: self.shell_policy.clone(),
            confirm_tx: self.confirm_tx.clone(),
            confirm_counter: self.confirm_counter.clone(),
            auto_mode: self.auto_mode.clone(),
            skills_dir,
            jobs: self.jobs.clone(),
            truncate: self.truncate,
        }
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

    #[must_use]
    pub fn with_auto_mode(&self, auto_mode: lofi_code::AutoModeFn) -> Self {
        Self {
            auto_mode: Some(auto_mode),
            ..self.clone()
        }
    }

    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    #[must_use]
    pub fn hard_compact_threshold(&self) -> Option<u64> {
        if self.reserved_context_tokens == 0 {
            return None;
        }
        self.model
            .context_window
            .filter(|&w| w > self.reserved_context_tokens)
            .map(|w| w - self.reserved_context_tokens)
    }

    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    #[must_use]
    pub fn tmp_dir(&self) -> &PathBuf {
        &self.tmp_dir
    }

    /// The session's background-job registry. Cheap to clone (shares the
    /// same map); the UI clones it once to render the job list and badge
    /// and to kill jobs from the `/job` modal.
    #[must_use]
    pub fn jobs(&self) -> lofi_code::tools::JobRegistry {
        self.jobs.clone()
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
            thinking: self.model.thinking.clone(),
        }
    }

    #[must_use]
    pub fn with_retry(mut self, retry: crate::retry::RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    #[must_use]
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

    /// Effective `max_tokens` for the next request, clipped so the request
    /// cannot demand more output than the remaining context window. Some
    /// providers reject `max_tokens >= context_window - input_tokens`; the
    /// prior round's usage (`input_tokens + cache_read_tokens`) approximates
    /// the next request's input. Returns `None` (omit the field) when the
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

/// Lossless event emit. An awaited [`Sender::send`] applies backpressure
/// when the 64-slot channel fills instead of dropping the event; the outer
/// `run` loop detects a closed channel and exits gracefully.
async fn emit(tx: Option<&Sender<AgentEvent>>, ev: AgentEvent) -> bool {
    if let Some(t) = tx {
        // Awaited (lossless) send: a tool/error/terminal event must not be
        // silently dropped when the 64-slot channel fills. A closed channel
        // (receiver gone) is reported to the caller so the round can stop
        // instead of continuing to consume/produce for a dead consumer.
        return t.send(ev).await.is_ok();
    }
    true
}

async fn wait_for_cancel(cancel: &Arc<AtomicBool>) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
