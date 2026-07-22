//! The agent loop.
//!
//! Drives a multi-turn conversation with a model, streaming [`AgentEvent`]s to
//! callers and dispatching the single LLM-facing tool — `exec` — into the
//! code-mode sandbox ([`lofi_code`]). The agent owns the resolved provider,
//! the selected model, the workspace root, and the system prompt; subagents
//! reuse the same shape via the [`crate::subagent::RoundTrip`] impl.
//!
//! ## No iteration cap (v1)
//!
//! Following the plan, neither [`Agent::run`] nor subagent runs impose an
//! iteration cap. Runaway loops are bounded by **per-call timeouts**: each
//! provider stream is wrapped in [`tokio::time::timeout`] with
//! [`DEFAULT_STREAM_TIMEOUT`], and each `exec` call inherits
//! [`lofi_code::DEFAULT_GUEST_TIMEOUT`] via [`lofi_code::ExecOptions`].
//! These bound provider waits and awaited native-tool calls; a *synchronous*
//! guest loop (see [`lofi_code`]) is not interruptible and is not covered.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use futures::StreamExt;
use lofi_types::{
    ContentBlock, Message, Model, NativeToolRecord, Role, RunModel, StreamingEvent,
    ThinkingLevel, Usage,
};
use tokio::sync::mpsc::Sender;

use crate::config_loader::load_config_or_default;
use crate::models::ModelRegistry;
use crate::session::recorder::{SessionRecorder, TurnOutcome};
use crate::state;
use crate::subagent::{self, RoundTrip, SubagentCtx, SubagentOptions};
use lofi_code::{exec, AgentFn, BashEnv, ExecCtx, ExecOptions, RecallFn, ResultFn, ToolEvent};
use lofi_error::{Error, Result};
use lofi_types::BashConfig;
use lofi_providers::ir::chat::ToolSchema;
use lofi_providers::ir::codec::assemble_message;
use lofi_providers::{open, Provider};

/// The system prompt shipped with lofi, `include_str!`'d from
/// `prompts/system.md`.
mod agent_run;
mod event;
mod exec;
mod model;

#[cfg(test)]
mod tests;

pub use event::AgentEvent;
pub use exec::{exec_input_code_and_label, exec_label, exec_result_display, exec_tool_schema, parse_exec_input};
pub use model::{build_agent, rebuild_agent, select_model};
pub(crate) use exec::{cap_exec_result, cap_tool_result, extract_code_prefix};
pub(crate) use model::initial_history;

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
/// Cap on each native tool (`lofi.read`/`lofi.bash`/…) result, applied
/// before it's folded into the exec payload. Without it a single huge
/// sub-tool call would monopolize the exec result. Caps each native tool
/// result at 50 KB / 2000 lines.
const MAX_TOOL_RESULT_BYTES: usize = 50 * 1024;
/// Outer cap on the whole exec result sent back to the provider. Native
/// sub-tool results are already individually capped to `MAX_TOOL_RESULT_BYTES`;
/// this guards the aggregate (many sub-tool results + `logs`) so a long
/// `Promise.all` burst or a verbose `print` loop can't balloon the context.
const MAX_EXEC_RESULT_BYTES: usize = 200 * 1024;

/// Fixed per-event charge added to the round byte budget to cover
/// Vec/enum/dispatch overhead not captured by owned-string lengths.
const PER_EVENT_OVERHEAD: usize = 64;
/// Maximum accepted `lofi.agent({ timeoutMs })` value. A model-supplied
/// timeout is clamped to this before conversion so an extreme integer cannot
/// overflow `Duration` arithmetic (which panics) or push the total deadline
/// past `Instant`'s range.
const MAX_SUBAGENT_TIMEOUT_MS: u64 = 60 * 60 * 1000;

/// Where to durably commit a completed turn: the transcript path (and an
/// optional branch point). Passed into [`Agent::run_continuation`] so the
/// engine — which owns the timers and cost counter — is the sole writer of
/// the session log. The turn-end marker's model identity comes from the
/// agent's own resolved model, not from here.
#[derive(Debug, Clone)]
pub struct SessionCommit {
    /// Path to the session `.jsonl` file.
    pub path: PathBuf,
    /// The entry id to branch this turn from. `None` appends to the file's
    /// current active leaf (linear continuation); `Some(id)` starts a new
    /// branch as a sibling of `id`'s existing children — used when the user
    /// resumes from a selected entry in the tree picker rather than the
    /// active leaf.
    pub parent_hint: Option<String>,
}

/// Lock a mutex, recovering from poison by taking the guard anyway. The
/// native-tool capture callbacks run single-threaded within an `exec`, so
/// poison is not expected in practice; this keeps the calls `unwrap`-free
/// (the workspace denies `clippy::unwrap_used`).
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Per-turn accumulation of timing and cost, threaded through the round loop
/// so the engine (not the UI) owns the timers. `tool_starts` records when each
/// tool call began; `tool_elapsed` is filled as each call ends. `cost` sums
/// every round's usage against the model's pricing; `usage` holds the final
/// round's tokens for the context gauge.
struct TurnStats {
    turn_start: Instant,
    tool_starts: HashMap<String, Instant>,
    tool_elapsed: HashMap<String, Duration>,
    /// Wall-clock duration of each assistant thinking block this turn, in the
    /// order they were produced, so the UI's "Thought for Ns" marker can be
    /// restored from the transcript on freeze/resume.
    thinking_elapsed: Vec<Duration>,
    /// Native tool calls (`lofi.<tool>`) completed inside `exec` blocks this
    /// turn, captured so they can be written to the transcript and restored
    /// on resume. Keyed by insertion order; each carries its parent exec id.
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

    /// Record a tool call starting.
    fn tool_start(&mut self, id: &str) {
        self.tool_starts.insert(id.to_string(), Instant::now());
    }

    /// Record a tool call finishing; return its elapsed milliseconds.
    fn tool_end(&mut self, id: &str) -> u64 {
        let ms = self
            .tool_starts
            .get(id)
            .map_or(0, |s| s.elapsed().as_millis() as u64);
        self.tool_elapsed
            .insert(id.to_string(), Duration::from_millis(ms));
        ms
    }

    /// Fold a round's usage into the turn cost and keep the latest usage.
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
        // A flat per-request cost is billed once per round, independent of
        // token pricing. A round is one provider call, so multi-round turns
        // accumulate one per-request charge per round.
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

/// The agent: a provider, a model, a workspace root, and a system prompt.
///
/// Cheaply cloneable (the provider is held in an `Arc`) so a subagent callback
/// can capture a copy and run a nested loop without borrowing.
#[derive(Clone)]
pub struct Agent {
    provider: Arc<dyn Provider>,
    model: Model,
    root: PathBuf,
    /// Per-session tmp directory for bash full-output logs. Created in
    /// [`new`](Self::new) and passed to every exec so the sandbox's
    /// `lofi.bash_read` and bash log writer share one location.
    tmp_dir: PathBuf,
    /// Transient-error retry budget and backoff schedule.
    retry: crate::retry::RetryPolicy,
    system_prompt: String,
    max_output_tokens: Option<u64>,
    /// Hard-cap reserve for mid-run force-compaction: a round whose input
    /// tokens exceed `model.context_window - reserved` triggers a
    /// `ContextPressure` stop. 0 disables the hard cap.
    reserved_context_tokens: u64,
    /// Resolved `bash` child-env policy + output-redaction set.
    bash_env: BashEnv,
}

impl Agent {
    /// Construct a new agent.
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
    ) -> Self {
        let bash_env = BashEnv::from_config(bash);
        Self {
            provider: Arc::from(provider),
            model,
            root,
            tmp_dir,
            retry: crate::retry::RetryPolicy::default(),
            system_prompt,
            max_output_tokens,
            reserved_context_tokens,
            bash_env,
        }
    }

    /// Return a new agent bound to a different provider+model, reusing this
    /// agent's workspace root, per-session tmp directory, system prompt,
    /// retry budget, and bash policy. The `/model` selector uses this to
    /// switch models mid-session without orphaning the per-session tmp dir
    /// (which backs `lofi.bash` full-output logs and `lofi.bash_read`), so
    /// tool state written before the switch stays reachable after it.
    #[must_use]
    pub fn with_model(&self, provider: Box<dyn Provider>, model: Model) -> Self {
        Self {
            provider: Arc::from(provider),
            model,
            root: self.root.clone(),
            tmp_dir: self.tmp_dir.clone(),
            retry: self.retry,
            system_prompt: self.system_prompt.clone(),
            max_output_tokens: self.max_output_tokens,
            reserved_context_tokens: self.reserved_context_tokens,
            bash_env: self.bash_env.clone(),
        }
    }

    /// Return a new agent with its system prompt replaced. Used once at
    /// startup to fold global and per-directory `AGENTS.md` into the base
    /// [`SYSTEM_PROMPT`] after the agent is built; the `/model` switch
    /// reuses the existing agent's prompt as-is, so this is never re-applied
    /// on a switch.
    #[must_use]
    pub fn with_system_prompt(&self, system_prompt: String) -> Self {
        Self {
            provider: self.provider.clone(),
            model: self.model.clone(),
            root: self.root.clone(),
            tmp_dir: self.tmp_dir.clone(),
            retry: self.retry,
            system_prompt,
            max_output_tokens: self.max_output_tokens,
            reserved_context_tokens: self.reserved_context_tokens,
            bash_env: self.bash_env.clone(),
        }
    }

    /// The system prompt this agent runs with.
    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    /// The mid-run hard-cap threshold: a round whose input tokens exceed this
    /// triggers a `ContextPressure` force-stop. `None` when the reserve is 0
    /// or the model has no context window, disabling the hard cap.
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

    /// The workspace root file operations are confined to.
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// The per-session tmp directory (for bash full-output logs, etc.).
    #[must_use]
    pub fn tmp_dir(&self) -> &PathBuf {
        &self.tmp_dir
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
            thinking: self.model.thinking,
        }
    }

    /// Override the retry policy (used by tests to inject a fast backoff).
    #[must_use]
    pub fn with_retry(mut self, retry: crate::retry::RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// The maximum output tokens hint, if set. Applied as an override on the
    /// model's `max_tokens` and encoded per API by the IR builders
    /// (`max_completion_tokens` for Chat, `max_output_tokens` for Responses).
    #[must_use]
    pub fn max_output_tokens(&self) -> Option<u64> {
        self.max_output_tokens
    }

}

/// [`Agent`] is a [`RoundTrip`]: one `run_once` per call, used by subagents.
impl RoundTrip for Agent {
    fn round_trip<'a>(
        &'a self,
        messages: &'a mut Vec<Message>,
    ) -> LocalBoxFuture<'a, Result<bool>> {
        Box::pin(self.run_once(messages))
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