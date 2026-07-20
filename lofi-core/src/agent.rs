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
    ContentBlock, Message, Model, NativeToolRecord, Role, StreamingEvent, ThinkingLevel,
    Usage,
};
use tokio::sync::mpsc::Sender;

use crate::config_loader::load_config_or_default;
use crate::models::ModelRegistry;
use crate::session::recorder::SessionRecorder;
use crate::state;
use crate::subagent::{self, RoundTrip, SubagentCtx, SubagentOptions};
use lofi_code::{exec, AgentFn, ExecCtx, ExecOptions, ToolEvent};
use lofi_error::{Error, Result};
use lofi_providers::ir::chat::ToolSchema;
use lofi_providers::ir::codec::assemble_message;
use lofi_providers::{open, Provider};

/// The system prompt shipped with lofi, `include_str!`'d from
/// `prompts/system.md`.
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

/// Truncate a tool-result string to at most `max` bytes (on a UTF-8 char
/// boundary), keeping the head and appending a truncation marker.
fn cap_tool_result_to(content: &str, max: usize) -> String {
    if content.len() <= max {
        return content.to_string();
    }
    let mut end = max;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[output truncated: {} bytes total, {} shown]",
        &content[..end],
        content.len(),
        end
    )
}

/// Per-native-tool cap. See [`MAX_TOOL_RESULT_BYTES`].
fn cap_tool_result(content: &str) -> String {
    cap_tool_result_to(content, MAX_TOOL_RESULT_BYTES)
}

/// Exec-level outer cap. See [`MAX_EXEC_RESULT_BYTES`].
fn cap_exec_result(content: &str) -> String {
    cap_tool_result_to(content, MAX_EXEC_RESULT_BYTES)
}
/// Fixed per-event charge added to the round byte budget to cover
/// Vec/enum/dispatch overhead not captured by owned-string lengths.
const PER_EVENT_OVERHEAD: usize = 64;
/// Maximum accepted `lofi.agent({ timeoutMs })` value. A model-supplied
/// timeout is clamped to this before conversion so an extreme integer cannot
/// overflow `Duration` arithmetic (which panics) or push the total deadline
/// past `Instant`'s range.
const MAX_SUBAGENT_TIMEOUT_MS: u64 = 60 * 60 * 1000;

/// Events emitted by the agent loop to the TUI / print driver.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A user turn has begun with this prompt. Always the first event of a
    /// turn; the matching [`TurnEnd`](Self::TurnEnd) (or
    /// [`Error`](Self::Error)) closes it. Making the turn boundary explicit
    /// lets a consumer build its view from the event stream alone — no
    /// external "push a new turn" call — so the live path and the
    /// from-disk replay path share one builder.
    TurnStart {
        /// The user's prompt text.
        prompt: String,
    },
    /// A chunk of assistant text.
    Text(String),
    /// A chunk of the model's reasoning / chain-of-thought trace. Surfaced
    /// separately from [`Text`](Self::Text) so the UI can fold it while
    /// still streaming it live.
    Thinking(String),
    /// A thinking block just ended; `elapsed_ms` is its wall-clock duration,
    /// stamped by the engine so the "Thought for Ns" marker survives resume
    /// without a UI-owned timer (mirroring [`ToolEnd`](Self::ToolEnd) owning
    /// the tool timer). Always emitted after the [`Thinking`](Self::Thinking)
    /// deltas of one block and before the next block or a non-thinking event.
    ThinkingEnd {
        /// Wall-clock duration of the thinking block in milliseconds.
        elapsed_ms: u64,
    },
    /// A tool call has begun.
    ToolStart {
        /// The tool-call id assigned by the provider.
        id: String,
        /// The tool name (currently always `exec`).
        name: String,
    },
    /// The assembled input (the TypeScript `code`) for a tool call, emitted
    /// just before execution so the UI can show what ran — not just the id.
    ToolInput {
        /// The tool-call id.
        id: String,
        /// The TypeScript source passed to the sandbox.
        code: String,
        /// A short human label for the exec (from the `display` field), shown
        /// as `Exec <label>` by the UI. `None` when the model omitted it.
        label: Option<String>,
    },
    /// A streaming fragment of a tool call's `code` input, emitted as the
    /// model writes it so the UI can show the exec source growing live. The
    /// fragments concatenate into the same `code` that [`ToolInput`]
    /// finalizes (with the label and authoritative full text).
    ToolInputDelta {
        /// The tool-call id.
        id: String,
        /// A decoded fragment of the TypeScript `code` field.
        delta: String,
    },
    /// A tool call has completed with `result` (a JSON string for success, an
    /// error message for failure). `elapsed_ms` is the wall-clock duration from
    /// the matching [`AgentEvent::ToolStart`], stamped by the engine so it
    /// survives resume without a UI-owned timer.
    ToolEnd {
        /// The tool-call id.
        id: String,
        /// The tool result payload.
        result: String,
        /// Whether the call failed — drives the exec block's red background.
        is_error: bool,
        /// Wall-clock duration of the call in milliseconds.
        elapsed_ms: u64,
    },
    /// A native tool call inside an `exec` block has started. `parent` is the
    /// enclosing exec tool-call id; `id` is a per-exec counter.
    NativeToolStart {
        parent: String,
        id: u64,
        name: String,
        args: String,
    },
    /// A native tool call inside an `exec` block has finished.
    NativeToolEnd {
        parent: String,
        id: u64,
        result: String,
        is_error: bool,
    },
    /// A turn completed: its run label (`provider/model · level`), wall-clock
    /// duration, the accumulated USD cost across the turn's rounds, and the
    /// final round's token usage. Emitted once per turn by
    /// [`Agent::run_continuation`] and also written to the transcript, so the
    /// live view and a resumed view render the same `◇ label done in Ns`
    /// block — the label travels with the event rather than being re-derived
    /// from the (possibly switched) active model on resume.
    TurnEnd {
        /// `provider/model · level` label for the turn-end marker.
        label: String,
        /// Wall-clock duration of the turn in milliseconds.
        elapsed_ms: u64,
        /// Accumulated USD cost across the turn's rounds.
        cost: f64,
        /// Final round's token usage (drives the context gauge).
        usage: Usage,
    },
    /// A provider error was encountered mid-stream.
    Error(String),
    /// The provider returned a transient error and the agent is waiting out
    /// the backoff before retrying the last assistant round. Emitted before
    /// the sleep so the UI can show a retry indicator.
    RetryStart {
        /// 1-indexed retry attempt.
        attempt: u32,
        /// Maximum retry attempts.
        max_attempts: u32,
        /// Backoff duration in milliseconds.
        delay_ms: u64,
        /// The error message that triggered the retry.
        error: String,
    },
    /// A retry completed (either the retry succeeded or the budget was
    /// exhausted). Emitted after the retry's outcome is known.
    RetryEnd {
        /// Whether the retry ultimately succeeded (a non-error assistant
        /// message landed).
        success: bool,
        /// How many retries were attempted.
        attempt: u32,
        /// When `success` is false, the final error message that could not be
        /// recovered.
        final_error: Option<String>,
    },
    /// A provider round completed within the current turn, carrying the
    /// turn's cumulative USD cost so far and this round's token usage.
    /// Emitted after each `StreamingEvent::Done` so the UI can refresh the
    /// context gauge and cost counter per round instead of waiting for the
    /// turn's final [`TurnEnd`](Self::TurnEnd). Multi-round turns (tool-use
    /// loops) emit one per round; `TurnEnd` still fires once at the end with
    /// the same totals (so the resume path, which has no `RoundUsage`
    /// events, reconstructs identical state from `TurnEnd` alone).
    RoundUsage {
        /// Cumulative USD cost across the turn's rounds so far.
        cost: f64,
        /// This round's token usage (drives the context gauge).
        usage: Usage,
    },
    /// A turn's events were durably appended to the transcript file, covering
    /// the byte range `[byte_start, byte_end)`. The UI uses this to make the
    /// now-frozen turn file-backed (drop its in-memory blocks and re-materialize
    /// from this range on demand). Emitted only for persisted sessions, after
    /// the file is flushed.
    TurnCommitted {
        byte_start: u64,
        byte_end: u64,
    },
}

/// Where to durably commit a completed turn: the transcript path and the
/// run label to stamp on the turn-end marker. Passed into
/// [`Agent::run_continuation`] so the engine — which owns the timers and
/// cost counter — is the sole writer of the session log.
#[derive(Debug, Clone)]
pub struct SessionCommit {
    /// Path to the session `.jsonl` file.
    pub path: PathBuf,
    /// `provider/model · level` label for the turn-end marker.
    pub label: String,
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
            .map(|s| s.elapsed().as_millis() as u64)
            .unwrap_or(0);
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
    /// `lofi.read_tmp` and bash log writer share one location.
    tmp_dir: PathBuf,
    /// Transient-error retry budget and backoff schedule.
    retry: crate::retry::RetryPolicy,
    system_prompt: String,
    max_output_tokens: Option<u64>,
}

impl Agent {
    /// Construct a new agent.
    #[must_use]
    pub fn new(
        provider: Box<dyn Provider>,
        model: Model,
        root: PathBuf,
        tmp_dir: PathBuf,
        system_prompt: String,
        max_output_tokens: Option<u64>,
    ) -> Self {
        Self {
            provider: Arc::from(provider),
            model,
            root,
            tmp_dir,
            retry: crate::retry::RetryPolicy::default(),
            system_prompt,
            max_output_tokens,
        }
    }

    /// The system prompt this agent runs with.
    #[must_use]
    pub fn system_prompt(&self) -> &str {
        &self.system_prompt
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

    /// `provider/model · level` — the run label stamped onto
    /// [`AgentEvent::TurnEnd`] and the transcript's `TurnEnd` marker. Built
    /// from the resolved [`Model`] so it is authoritative across a model
    /// switch on resume (the UI no longer re-derives it from the active
    /// model, which would mismatch a persisted turn's original model).
    #[must_use]
    pub fn run_label(&self) -> String {
        format!(
            "{}/{}{}",
            self.model.provider,
            self.model.id,
            if self.model.thinking != ThinkingLevel::Off {
                format!(" · {}", self.model.thinking.as_str())
            } else {
                String::new()
            }
        )
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

    /// Run the interactive loop for a single user prompt, streaming events to
    /// `tx` until the assistant finishes a turn with no tool calls.
    ///
    /// Builds the initial `system` + `user` message history, then drives
    /// [`Self::run_once`] in a loop. If the receiver is dropped (the channel
    /// closes), the run exits gracefully.
    ///
    /// # Errors
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution failures that cannot be surfaced as a `ToolResult`.
    pub async fn run(&self, user_prompt: String, tx: Sender<AgentEvent>) -> Result<()> {
        let mut messages = initial_history(&self.system_prompt, &user_prompt);
        if !emit(Some(&tx), AgentEvent::TurnStart { prompt: user_prompt.clone() }).await {
            return Ok(());
        }
        loop {
            if tx.is_closed() {
                return Ok(());
            }
            let finished = match self.run_once_inner(&mut messages, Some(&tx), None).await {
                Ok(f) => f,
                // A gone receiver is a graceful cancellation, not a provider
                // error: stop the run cleanly instead of surfacing it.
                Err(Error::Cancelled) => return Ok(()),
                Err(e) => return Err(e),
            };
            if finished || tx.is_closed() {
                return Ok(());
            }
        }
    }

    /// Run one user turn appended to an existing conversation history.
    ///
    /// Unlike [`run`](Self::run), the message history is owned by the caller
    /// (e.g. the interactive TUI) so follow-up prompts keep the full prior
    /// context — assistant turns, tool calls, and tool results — instead of
    /// starting fresh each time. The first call seeds the system prompt.
    ///
    /// The engine owns the turn timer, per-tool timers, and cost counter. On
    /// normal completion it emits a single [`AgentEvent::TurnEnd`] summarizing
    /// the turn and, when `commit` is given, appends the turn's messages plus
    /// timing/cost events to the transcript — making the engine the
    /// sole writer of the session log so a resumed session reconstructs
    /// identically to the live one.
    ///
    /// # Errors
    ///
    /// Propagates [`Error`] from provider streaming, timeouts, or tool
    /// execution.
    pub async fn run_continuation(
        &self,
        messages: &mut Vec<Message>,
        user_prompt: String,
        tx: Sender<AgentEvent>,
        commit: Option<&SessionCommit>,
    ) -> Result<()> {
        let prev_len = messages.len();
        let prompt_for_event = user_prompt.clone();
        if messages.is_empty() {
            *messages = initial_history(&self.system_prompt, &user_prompt);
        } else {
            messages.push(Message {
                role: Role::User,
                blocks: vec![ContentBlock::Text {
                    text: user_prompt,
                }],
            });
        }
        // Checkpoint after seeding the user turn: if the receiver goes away
        // mid-round (e.g. after the assistant tool-use turn but before its
        // tool results), roll back so the caller-owned history is never left
        // in a protocol-invalid partial state.
        let checkpoint = messages.len();
        if !emit(Some(&tx), AgentEvent::TurnStart { prompt: prompt_for_event }).await {
            return Ok(());
        }
        let mut stats = TurnStats::new();
        let mut finished_normally = false;
        let mut err: Option<Error> = None;
        let retry = self.retry;
        let mut retry_attempt = 0u32;
        loop {
            if tx.is_closed() {
                break;
            }
            match self
                .run_once_inner(&mut *messages, Some(&tx), Some(&mut stats))
                .await
            {
                Ok(true) => {
                    finished_normally = true;
                    // A successful assistant response concludes any in-flight
                    // retry: a successful assistant response concludes any
                    // in-flight retry.
                    if retry_attempt > 0 {
                        let _ = tx
                            .send(AgentEvent::RetryEnd {
                                success: true,
                                attempt: retry_attempt,
                                final_error: None,
                            })
                            .await;
                    }
                    break;
                }
                Ok(false) if tx.is_closed() => break,
                Ok(false) => {}
                Err(Error::Cancelled) => {
                    // Abandoned: roll back the partial turn and commit nothing.
                    messages.truncate(checkpoint);
                    return Ok(());
                }
                Err(e) => {
                    // Transient provider errors are retried with exponential
                    // backoff: the failed
                    // assistant message is dropped and the round is restarted
                    // so the provider produces a fresh response.
                    if retry.can_retry(retry_attempt)
                        && crate::retry::is_retryable_error(&e)
                    {
                        retry_attempt += 1;
                        let delay = retry.delay_for(retry_attempt);
                        // Drop the partial assistant message the failed round
                        // appended (if any) so the retried round starts from a
                        // clean conversation tail.
                        if messages.last().is_some_and(|m| m.role == Role::Assistant)
                        {
                            messages.pop();
                        }
                        let _ = tx
                            .send(AgentEvent::RetryStart {
                                attempt: retry_attempt,
                                max_attempts: retry.max_retries,
                                delay_ms: delay.as_millis() as u64,
                                error: e.to_string(),
                            })
                            .await;
                        // Cancellable backoff: wait `delay` for the channel to
                        // stay open. If the receiver drops, roll back like a
                        // normal cancellation rather than driving a dead channel.
                        match tokio::time::timeout(delay, tx.closed()).await {
                            Err(_) => {
                                // Delay elapsed and the channel is still open;
                                // proceed to the retry.
                            }
                            Ok(_) => {
                                // The receiver dropped during the backoff.
                                messages.truncate(checkpoint);
                                return Ok(());
                            }
                        }
                        continue;
                    }
                    // The error is not retryable or the budget is exhausted:
                    // emit the final failure summary if a retry was in flight.
                    if retry_attempt > 0 {
                        let _ = tx
                            .send(AgentEvent::RetryEnd {
                                success: false,
                                attempt: retry_attempt,
                                final_error: Some(e.to_string()),
                            })
                            .await;
                    }
                    err = Some(e);
                    break;
                }
            }
        }
        // Commit the turn's messages and tool timings. A `TurnEnd` marker
        // (and its cost/usage summary) is written and emitted only when the
        // turn completed normally — an errored or abandoned turn has no
        // "done in Ns" summary, matching the live view's missing marker.
        let elapsed_ms = stats.turn_start.elapsed().as_millis() as u64;
        if finished_normally && !tx.is_closed() {
            let _ = tx
                .send(AgentEvent::TurnEnd {
                    label: self.run_label(),
                    elapsed_ms,
                    cost: stats.cost,
                    usage: stats.usage,
                })
                .await;
        }
        // The durable translation lives in `SessionRecorder`: hand it the
        // finalized message slice + a snapshot of the engine's accumulators
        // and it shapes/writes the `SessionEvent`s. The agent loop stays free
        // of on-disk-format concerns.
        if let Some(commit) = commit {
            let summary = stats.summary(elapsed_ms);
            let mut recorder = SessionRecorder::new(commit.path.clone(), commit.label.clone());
            match recorder.flush(&messages[prev_len..], finished_normally, &summary) {
                Ok(Some((byte_start, byte_end))) if !tx.is_closed() => {
                    let _ = tx
                        .send(AgentEvent::TurnCommitted { byte_start, byte_end })
                        .await;
                }
                _ => {}
            }
        }
        match err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// A single provider round-trip: stream one assistant turn, append it to
    /// `messages`, execute any tool calls, and append a `user`-role message
    /// carrying the `ToolResult` blocks.
    ///
    /// Returns `Ok(true)` when the assistant turn had no tool calls (the loop
    /// should stop), or `Ok(false)` when a tool was invoked and the loop
    /// should continue. This is the primitive subagents call via
    /// [`RoundTrip`]; it emits no events.
    ///
    /// # Errors
    /// Propagates [`Error`] from provider streaming or timeouts.
    pub async fn run_once(&self, messages: &mut Vec<Message>) -> Result<bool> {
        self.run_once_inner(messages, None, None).await
    }

    /// Shared core of [`run_once`] with an optional event sender.
    ///
    /// Events are emitted via an awaited [`Sender::send`] so a slow receiver
    /// applies backpressure without dropping events; the outer
    /// [`run`](Self::run) loop checks `tx.is_closed()` to exit when the
    /// consumer is gone.
    // A single large async dispatch over streaming events; splitting it
    // across helpers would scatter the shared mutable round state (usage,
    // byte budget, event log) through many params and hurt readability more
    // than the line count hurts here.
    #[allow(clippy::too_many_lines)]
    async fn run_once_inner(
        &self,
        messages: &mut Vec<Message>,
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
    ) -> Result<bool> {
        let schema = exec_tool_schema();
        // Effective output limit: an explicit agent override wins over the
        // model's static `max_tokens`, and is encoded per API by the IR
        // builders (previously OpenAI ignored it entirely).
        let mut model = self.model.clone();
        if let Some(mt) = self.max_output_tokens {
            model.max_tokens = Some(mt);
        }
        let stream = self.provider.stream(&model, messages, &[schema]).await?;
        let mut stream = stream;

        let mut events: Vec<StreamingEvent> = Vec::new();
        // Round usage is captured and emitted as `AgentEvent::Done` only when
        // the round is terminal (no tool calls), so `Done` remains a true
        // end-of-run signal rather than firing before tool execution.
        let mut round_usage: Option<Usage> = None;
        let mut round_bytes = 0usize;
        // Per tool-call accumulated raw input JSON and the byte length of the
        // `code` field already streamed to the UI, for live exec streaming.
        let mut tool_raw: HashMap<String, String> = HashMap::new();
        let mut tool_emitted: HashMap<String, usize> = HashMap::new();
        let collect = async {
            // Open thinking block timer: started on the first ThinkingDelta
            // of a run, closed when a non-thinking event arrives (or the
            // stream ends). Each closed duration is recorded in `stats` so it
            // can be persisted as a `ThinkingTiming` event.
            let mut thinking_open: Option<Instant> = None;
            loop {
                match tokio::time::timeout(
                    DEFAULT_STREAM_IDLE_TIMEOUT,
                    stream.next(),
                )
                .await
                {
                    Err(_) => return Err(Error::Provider("stream idle timeout".into())),
                    Ok(None) => break,
                    Ok(Some(ev)) => match ev {
                    Ok(e) => {
                        let is_thinking_ev = matches!(
                            e,
                            StreamingEvent::ThinkingDelta(_) | StreamingEvent::ThinkingSignature(_)
                        );
                        if !is_thinking_ev {
                            if let (Some(start), Some(s)) =
                                (thinking_open.take(), stats.as_deref_mut())
                            {
                                let elapsed = start.elapsed();
                                s.thinking_elapsed.push(elapsed);
                                if !emit(
                                    tx,
                                    AgentEvent::ThinkingEnd {
                                        elapsed_ms: elapsed.as_millis() as u64,
                                    },
                                )
                                .await
                                {
                                    return Err(Error::Cancelled);
                                }
                            }
                        }
                        match &e {
                            StreamingEvent::TextDelta(d) => {
                                if !emit(tx, AgentEvent::Text(d.clone())).await {
                                    return Err(Error::Cancelled);
                                }
                            }
                            StreamingEvent::ThinkingDelta(d) => {
                                if thinking_open.is_none() {
                                    thinking_open = Some(Instant::now());
                                }
                                if !emit(tx, AgentEvent::Thinking(d.clone())).await {
                                    return Err(Error::Cancelled);
                                }
                            }
                            StreamingEvent::ToolUseStart { id, name } => {
                                if let Some(s) = stats.as_deref_mut() {
                                    s.tool_start(id);
                                }
                                if !emit(
                                    tx,
                                    AgentEvent::ToolStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                    },
                                )
                                .await
                                {
                                    return Err(Error::Cancelled);
                                }
                            }
                            StreamingEvent::ToolUseInputDelta { id, delta } => {
                                let raw = tool_raw.entry(id.clone()).or_default();
                                raw.push_str(delta);
                                let decoded = extract_code_prefix(raw);
                                let prev = tool_emitted.get(id).copied().unwrap_or(0);
                                if let Some(chunk) = decoded.get(prev..) {
                                    if !chunk.is_empty() {
                                        if !emit(
                                            tx,
                                            AgentEvent::ToolInputDelta {
                                                id: id.clone(),
                                                delta: chunk.to_string(),
                                            },
                                        )
                                        .await
                                        {
                                            return Err(Error::Cancelled);
                                        }
                                        tool_emitted.insert(id.clone(), decoded.len());
                                    }
                                }
                            }
                            StreamingEvent::Done(usage) => {
                                round_usage = Some(*usage);
                                if let Some(s) = stats.as_deref_mut() {
                                    s.add_usage(*usage, &self.model);
                                    // Emit the turn's cumulative cost and
                                    // this round's usage immediately so the
                                    // UI's context gauge and cost counter
                                    // refresh per round, not just at turn end.
                                    if !emit(
                                        tx,
                                        AgentEvent::RoundUsage {
                                            cost: s.cost,
                                            usage: s.usage,
                                        },
                                    )
                                    .await
                                    {
                                        return Err(Error::Cancelled);
                                    }
                                }
                            }
                            StreamingEvent::Error(msg) => {
                                if !emit(tx, AgentEvent::Error(msg.clone())).await {
                                    return Err(Error::Cancelled);
                                }
                                return Err(Error::Provider(msg.clone()));
                            }
                            _ => {}
                        }
                        // Bound retained round bytes so a stream of many
                        // small valid events cannot exhaust memory.
                        // Charge every owned string plus a fixed per-event
                        // allowance for Vec/enum/dispatch overhead, so large
                        // IDs/signatures or many zero-text events are caught.
                        round_bytes = round_bytes.saturating_add(PER_EVENT_OVERHEAD);
                        round_bytes = round_bytes.saturating_add(match &e {
                            StreamingEvent::TextDelta(s)
                            | StreamingEvent::ThinkingDelta(s)
                            | StreamingEvent::ThinkingSignature(s)
                            | StreamingEvent::Error(s) => s.len(),
                            StreamingEvent::ToolUseStart { id, name } => id.len() + name.len(),
                            StreamingEvent::ToolUseInputDelta { id, delta } => {
                                id.len() + delta.len()
                            }
                            StreamingEvent::ToolUseEnd { id } => id.len(),
                            StreamingEvent::Done(_) => 0,
                        });
                        if round_bytes > MAX_ROUND_BYTES {
                            return Err(Error::Provider(format!(
                                "round exceeded {MAX_ROUND_BYTES} byte budget"
                            )));
                        }
                        events.push(e);
                    }
                    Err(err) => {
                        return Err(err);
                    }
                    },
                }
            }
            // Stream ended: close any still-open thinking block.
            if let (Some(start), Some(s)) = (thinking_open.take(), stats.as_deref_mut()) {
                let elapsed = start.elapsed();
                s.thinking_elapsed.push(elapsed);
                if !emit(
                    tx,
                    AgentEvent::ThinkingEnd {
                        elapsed_ms: elapsed.as_millis() as u64,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
            }
            Ok(())
        };

        collect.await?;

        let assistant = assemble_message(&events);
        messages.push(assistant.clone());

        let tool_uses: Vec<(&str, &str, &serde_json::Value)> = assistant
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect();

        if tool_uses.is_empty() {
            // The turn-end marker (and cost/usage) is emitted once by the
            // outer `run_continuation` from the accumulated `TurnStats`, not
            // per round, so a multi-round turn produces a single summary.
            return Ok(true);
        }

        let results = self.execute_tools(&tool_uses, tx, stats.as_deref_mut()).await?;

        // Tool results travel under the dedicated Tool role: each provider
        // converter emits them from its Role::Tool arm (Chat Completions
        // role:"tool", Responses function_call_output, Anthropic
        // tool_result). A User role would be skipped — collect_text ignores
        // ToolResult blocks — and the next round would 400 with "No tool
        // output found for function call".
        messages.push(Message {
            role: Role::Tool,
            blocks: results,
        });
        Ok(false)
    }

    /// Execute each tool call in `tool_uses` against the sandbox, emitting
    /// [`AgentEvent::ToolInput`] (the code) and [`AgentEvent::ToolEnd`] (the
    /// result) per call, and return the `tool_result` blocks to append.
    #[allow(clippy::too_many_lines)]
    async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
    ) -> Result<Vec<ContentBlock>> {
        let agent_fn = self.make_agent_fn();
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        // Native tool events are emitted from a *sync* `on_tool_event`
        // callback inside the sandbox, so they can't `await` on the bounded
        // UI channel. `try_send` would drop events when the channel fills
        // (e.g. a `Promise.all` burst of Start/End events), leaving native
        // tiles stuck "running" even after the exec succeeds. Relay them
        // through an unbounded channel whose forwarder drains onto the
        // bounded channel with `send().await` — lossless and order-preserving.
        let (relay_tx, mut relay_rx) =
            tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let relay_dst = tx.cloned();
        let _forwarder = tokio::spawn(async move {
            while let Some(ev) = relay_rx.recv().await {
                match &relay_dst {
                    Some(dst) => {
                        if dst.send(ev).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        drop(ev);
                    }
                }
            }
        });
        for (id, name, input) in tool_uses {
            // Only `exec` is advertised; an unknown tool name is a protocol
            // error, not a silent empty success.
            if *name != "exec" {
                let content = format!("unknown tool: {name}");
                let elapsed_ms = stats.as_deref_mut().map(|s| s.tool_end(id)).unwrap_or(0);
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.to_string(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                });
                continue;
            }
            let (code, strings, display) = parse_exec_input(input);
            if code.is_empty() {
                let content = "exec: missing required 'code' argument".to_string();
                let elapsed_ms = stats.as_deref_mut().map(|s| s.tool_end(id)).unwrap_or(0);
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.to_string(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                });
                continue;
            }
            // The ToolInput event must land before we invoke the sandbox;
            // if the consumer is gone, stop instead of running tools (which
            // may shell out or mutate files) for a dead receiver.
            if !emit(
                tx,
                AgentEvent::ToolInput {
                    id: id.to_string(),
                    code: code.clone(),
                    label: exec_label(&display),
                },
            )
            .await
            {
                return Err(Error::Cancelled);
            }
            // Forward native tool-call events (one per `lofi.<tool>`
            // invocation inside the sandbox) to the UI under this exec id, and
            // capture each completed call so it can be written to the
            // transcript and restored on resume. `pending` holds a Start's
            // name/args until the matching End supplies the result.
            let native_tx = relay_tx.clone();
            let parent = id.to_string();
            let native_pending: Arc<Mutex<HashMap<u64, (String, String)>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let native_completed: Arc<Mutex<Vec<NativeToolRecord>>> =
                Arc::new(Mutex::new(Vec::new()));
            let on_tool_event: Arc<dyn Fn(ToolEvent) + Send + Sync> = {
                let native_pending = native_pending.clone();
                let native_completed = native_completed.clone();
                Arc::new(move |ev: ToolEvent| match ev {
                    ToolEvent::Start { id, name, args } => {
                        lock(&native_pending).insert(id, (name.clone(), args.clone()));
                        let _ = native_tx.send(AgentEvent::NativeToolStart {
                            parent: parent.clone(),
                            id,
                            name,
                            args,
                        });
                    }
                    ToolEvent::End {
                        id,
                        result,
                        is_error,
                    } => {
                        // Cap each native tool result individually so one huge
                        // `lofi.read`/`lofi.bash` can't monopolize the exec
                        // payload, and many concurrent calls each keep a
                        // truncated slice rather than the first few whole and
                        // the rest dropped by the outer cap.
                        let result = cap_tool_result(&result);
                        if let Some((name, args)) = lock(&native_pending).remove(&id) {
                            lock(&native_completed).push(NativeToolRecord {
                                parent: parent.clone(),
                                call_id: id,
                                name,
                                args,
                                result: result.clone(),
                                is_error,
                            });
                        }
                        let _ = native_tx.send(AgentEvent::NativeToolEnd {
                            parent: parent.clone(),
                            id,
                            result,
                            is_error,
                        });
                    }
                }) as Arc<dyn Fn(ToolEvent) + Send + Sync>
            };
            let exec_ctx = ExecCtx {
                root: self.root.clone(),
                tmp_dir: self.tmp_dir.clone(),
                strings,
                agent: Some(agent_fn.clone()),
                on_tool_event: Some(on_tool_event),
            };
            let outcome = exec(&code, &exec_ctx, &ExecOptions::default()).await;
            // Drain the native tool calls that completed inside this exec into
            // the turn stats, so they are persisted with the turn.
            let captured = lock(&native_completed).drain(..).collect::<Vec<_>>();
            if let Some(s) = stats.as_deref_mut() {
                s.native_tools.extend(captured);
            }
            let (content, is_error) = match outcome {
                Ok(r) => {
                    let payload = serde_json::json!({ "value": r.value, "logs": r.logs });
                    let content =
                        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
                    (cap_exec_result(&content), false)
                }
                Err(e) => (cap_exec_result(&e.to_string()), true),
            };
            let elapsed_ms = stats.as_deref_mut().map(|s| s.tool_end(id)).unwrap_or(0);
            if !emit(
                tx,
                AgentEvent::ToolEnd {
                    id: id.to_string(),
                    result: content.clone(),
                    is_error,
                    elapsed_ms,
                },
            )
            .await
            {
                return Err(Error::Cancelled);
            }
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content,
                is_error,
            });
        }
        Ok(results)
    }

    /// Build the `lofi.agent` / `lofi.spawn` callback used by [`ExecCtx`].
    ///
    /// The closure clones the agent (cheap — `Arc` provider) and runs
    /// [`subagent::run`] with a fresh nested loop reusing the same provider,
    /// model, and workspace root. The subagent's final assistant text becomes
    /// the `lofi.agent()` return value inside the sandbox.
    fn make_agent_fn(&self) -> AgentFn {
        let self_clone = self.clone();
        Arc::new(move |req: lofi_code::AgentRequest| {
            let agent = self_clone.clone();
            Box::pin(async move {
                // Honor a caller-supplied `timeoutMs` (per round-trip); fall
                // back to the default. A total deadline of 20× the per-round
                // timeout bounds runaway loops that complete within each call.
                let per_rt_ms = req
                    .opts
                    .as_ref()
                    .and_then(|o| o.get("timeoutMs").and_then(serde_json::Value::as_u64))
                    .map_or(2 * 60 * 1000, |ms| ms.min(MAX_SUBAGENT_TIMEOUT_MS));
                let per_rt = Duration::from_millis(per_rt_ms);
                // `checked_mul` so a large (clamped) per-round value cannot
                // overflow `Duration` multiplication (which panics in debug).
                let total = per_rt.checked_mul(20).unwrap_or(per_rt);
                let opts = SubagentOptions {
                    system: agent.system_prompt.clone(),
                    timeout: per_rt,
                    total_timeout: Some(total),
                };
                let parent = SubagentCtx {
                    root: agent.root.clone(),
                    strings: HashMap::new(),
                };
                subagent::run(&parent, &agent, &req.prompt, &opts).await
            })
        })
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

/// The single LLM-facing tool schema: `exec`.
#[must_use]
pub fn exec_tool_schema() -> ToolSchema {
    ToolSchema {
        name: "exec".to_string(),
        description: "Compile and run a TypeScript program in a sandboxed QuickJS runtime. The program has access to a `lofi` object with file/shell/search tools (read, ls, find, grep, write, edit, bash) and a `lofi.agent(prompt, opts?)` subagent helper. Top-level await and return are supported. The returned value is sent back as the tool result; keep it compact and final.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "TypeScript source. Top-level await/return supported."
                },
                "strings": {
                    "type": "object",
                    "description": "Named string constants exposed as the global `lofi_strings` object."
                },
                "display": {
                    "type": "object",
                    "description": "Optional display metadata; ignored by the runtime."
                }
            },
            "required": ["code"]
        }),
    }
}

/// Build the initial message history (system + user).
fn initial_history(system: &str, user_prompt: &str) -> Vec<Message> {
    let mut messages = Vec::with_capacity(2);
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
            text: user_prompt.to_string(),
        }],
    });
    messages
}

fn decode_json_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    let mut escape = false;
    while let Some(c) = chars.next() {
        if escape {
            match c {
                'n' => out.push(char::from(0x0A)),
                't' => out.push(char::from(0x09)),
                'r' => out.push(char::from(0x0D)),
                'b' => out.push(char::from(0x08)),
                'f' => out.push(char::from(0x0C)),
                'u' => {
                    let mut hex = String::with_capacity(4);
                    for _ in 0..4 {
                        match chars.next() {
                            Some(h) => hex.push(h),
                            None => return out,
                        }
                    }
                    if let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                        out.push(ch);
                    }
                }
                other => out.push(other),
            }
            escape = false;
        } else if c == char::from(0x5C) {
            escape = true;
        } else if c == char::from(0x22) {
            return out;
        } else {
            out.push(c);
        }
    }
    out
}

/// Best-effort incremental extraction of the `code` string field from a
/// partial tool-input JSON buffer. Returns the decoded content available so
/// far, so the exec source can be streamed live as the model writes it.
fn extract_code_prefix(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let n = bytes.len();
    let code_key: &[u8] = &[0x22, b'c', b'o', b'd', b'e', 0x22];
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < n {
        let c = bytes[i];
        if in_str {
            if escape {
                escape = false;
            } else if c == 0x5C {
                escape = true;
            } else if c == 0x22 {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == 0x22 {
            if i + code_key.len() <= n && &bytes[i..i + code_key.len()] == code_key {
                i += code_key.len();
                while i < n && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= n || bytes[i] != b':' {
                    return String::new();
                }
                i += 1;
                while i < n && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i >= n {
                    return String::new();
                }
                if bytes[i] != 0x22 {
                    return String::new();
                }
                i += 1;
                return decode_json_string(&raw[i..]).trim_matches('\n').to_string();
            }
            in_str = true;
            i += 1;
            continue;
        }
        i += 1;
    }
    String::new()
}

/// Parse an `exec` tool input into `(code, strings, display)`.
///
/// Missing `code` yields an empty string (which compiles to a no-op).
/// `strings` values are coerced to strings via `serde_json` for non-string
/// entries. `display` is returned as-is for future use.
pub fn parse_exec_input(
    input: &serde_json::Value,
) -> (String, HashMap<String, String>, serde_json::Value) {
    let code = input
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim_matches('\n')
        .to_string();
    let mut strings = HashMap::new();
    if let Some(obj) = input.get("strings").and_then(serde_json::Value::as_object) {
        for (k, v) in obj {
            let s = match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            strings.insert(k.clone(), s);
        }
    }
    let display = input
        .get("display")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    (code, strings, display)
}

/// Extract a short UI label from an exec call's `display` field: a bare
/// string is used directly, an object is probed for `name`/`title`/
/// `description`.
#[must_use]
pub fn exec_label(display: &serde_json::Value) -> Option<String> {
    if let Some(s) = display.as_str() {
        let s = s.trim();
        return (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(obj) = display.as_object() {
        for key in ["name", "title", "description", "task"] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str()) {
                let s = s.trim();
                if !s.is_empty() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

/// UI-facing extract: the TypeScript `code` and optional `display` label for
/// an `exec` tool-call input. Used by the TUI when restoring a session.
#[must_use]
pub fn exec_input_code_and_label(input: &serde_json::Value) -> (String, Option<String>) {
    let (code, _strings, display) = parse_exec_input(input);
    (code, exec_label(&display))
}

/// UI-facing extract of an `exec` call's result: on success the payload is
/// `{ "value": ..., "logs": [...] }` and we surface `value` (a string as-is,
/// anything else pretty-printed); on error the raw message is returned.
#[must_use]
pub fn exec_result_display(result: &str, is_error: bool) -> String {
    if is_error {
        return result.to_string();
    }
    serde_json::from_str::<serde_json::Value>(result)
        .ok()
        .and_then(|v| v.get("value").cloned())
        .map_or_else(|| result.to_string(), |v| {
            if let Some(s) = v.as_str() {
                s.to_string()
            } else {
                serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string())
            }
        })
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

/// Build an [`Agent`] and its selected [`Model`] from the user config and
/// an optional `--model provider/model[:level]` query.
///
/// Shared by the `lofi-ui` presentation drivers (`run_print`, `run_interactive`)
/// so the resolution ladder (config load, registry + discovery, model +
/// thinking-level resolution, transport construction) stays in one place.
/// With no `--model`, the first available model (config order) is selected; if
/// no provider has credentials (or the selected provider is disabled),
/// [`Error::NoModels`] is returned so the interactive UI can launch and show
/// a friendly message. Remote discovery failures
/// fall back to static-only [`ModelRegistry::load`].
///
/// # Errors
/// Propagates [`Error`] from config load, model resolution, thinking-level
/// validation, or provider construction.
pub async fn build_agent(
    config_path: Option<&std::path::Path>,
    model: Option<&str>,
    root: &std::path::Path,
) -> Result<(Agent, Model, ThinkingLevel)> {
    let config_path = match config_path {
        Some(p) => p.to_path_buf(),
        None => crate::config_loader::user_config_path()?,
    };
    let config = load_config_or_default(&config_path).await?;

    let registry = match ModelRegistry::load_async(&config).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "async model load failed; falling back to static");
            ModelRegistry::load(&config)?
        }
    };

    let (mut model_obj, level) = select_model(&registry, &config, model)?;
    // The effective thinking level is resolved here, not in the registry, so
    // the same cached registry serves runs at different levels.
    model_obj.thinking = level;

    let provider_cfg = config
        .providers
        .get(&model_obj.provider)
        .ok_or_else(|| Error::Config(format!("provider not found: {}", model_obj.provider)))?;
    let provider = open(model_obj.api, provider_cfg)?;

    let agent = Agent::new(
        provider,
        model_obj.clone(),
        root.to_path_buf(),
        state::create_session_tmp_dir()?,
        SYSTEM_PROMPT.to_string(),
        None,
    );
    Ok((agent, model_obj, level))
}

/// A parsed --model query: provider/model[:level].
struct ModelQuery {
    provider: String,
    model: String,
    level: Option<ThinkingLevel>,
}

/// Parse a `--model` argument of the form `provider/model[:level]`.
///
/// The provider qualifier is mandatory: bare ids are rejected so a prompt
/// always names the endpoint it runs against. `level` is an optional
/// `:off`/`:low`/`:medium`/`:high`/`:xhigh` suffix; an unrecognized suffix is
/// an error rather than silently ignored.
fn parse_model_query(query: &str) -> Result<ModelQuery> {
    // Split off a trailing :level only when it parses as a level, so a model
    // id that happens to contain : is not misread.
    let (qual, level) = match query.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() => match ThinkingLevel::parse(tail) {
            Some(l) => (head, Some(l)),
            None => {
                return Err(Error::Config(format!(
                    "unknown thinking level `{tail}` in `{query}` (expected off|low|medium|high|xhigh)"
                )));
            }
        },
        _ => (query, None),
    };
    let (provider, model) = qual.split_once('/').ok_or_else(|| {
        Error::Config(format!(
            "model `{query}` must be qualified as `provider/model[:level]`"
        ))
    })?;
    if provider.is_empty() || model.is_empty() {
        return Err(Error::Config(format!("malformed model query `{query}`")));
    }
    Ok(ModelQuery {
        provider: provider.to_string(),
        model: model.to_string(),
        level,
    })
}

const NO_MODELS_HINT: &str = "No models configured.";

/// Pick the model to run against and resolve its thinking level.
///
/// With a `model_query` of `provider/model[:level]`, the named model is resolved
/// within the named provider and must be available (its provider
/// authenticated or `no_auth`). Without a query, the first available model in
/// registry (config) order is chosen. The thinking level resolves from the
/// CLI `:level`, then the model/provider/agent defaults, then `medium`; it must
/// be `off` or one of the model's declared `thinking_levels`.
///
/// # Errors
/// Returns [`Error::NoModels`] when no model is available, or when the named
/// model's provider is disabled (no credentials); [`Error::Config`] for an
/// unresolvable query, an unknown model/provider, or an unsupported thinking
/// level.
pub fn select_model(
    registry: &ModelRegistry,
    config: &lofi_types::Config,
    model_query: Option<&str>,
) -> Result<(Model, ThinkingLevel)> {
    let available = registry.available();
    let (provider_name, model_id, explicit_level) = if let Some(q) = model_query {
        let mq = parse_model_query(q)?;
        (mq.provider, mq.model, mq.level)
    } else if let Some(default) = config.default_model.as_deref() {
        // `default_model` wins: parse it as a `provider/model[:level]` query
        // so a bare id is rejected the same way an explicit `--model` is.
        let mq = parse_model_query(default)?;
        (mq.provider, mq.model, mq.level)
    } else if let Some(provider) = config.default_provider.as_deref() {
        // `default_provider` selects that provider's first available model.
        let m = available
            .iter()
            .find(|a| a.provider == provider)
            .ok_or_else(|| {
                Error::Config(format!(
                    "default_provider `{provider}` has no available model; available:\n{}",
                    registry.list_models_print()
                ))
            })?;
        (m.provider.clone(), m.id.clone(), None)
    } else {
        let m = available
            .first()
            .ok_or_else(|| Error::NoModels(NO_MODELS_HINT.to_string()))?;
        (m.provider.clone(), m.id.clone(), None)
    };

    let model = registry
        .resolve(&format!("{provider_name}/{model_id}"))
        .ok_or_else(|| {
            Error::Config(format!(
                "no model `{model_id}` for provider `{provider_name}`; available:\n{}",
                registry.list_models_print()
            ))
        })?
        .clone();
    // Require the resolved model's provider to be available so a keyless
    // provider's model is never silently selected and then fails at request
    // time.
    if !available
        .iter()
        .any(|a| a.id == model.id && a.provider == model.provider)
    {
        return Err(Error::NoModels(format!(
            "model `{provider_name}/{model_id}` is not available: its provider has no API key set. \
             Set its env var (see the provider's `env_name`) or configure credentials in the config file."
        )));
    }

    // Use the registry's (possibly augmented) providers so auto-discovered
    // models — which load_async injects only into the registry's copy — are
    // resolvable here for thinking-level lookup. `config.agent` is unaffected
    // by discovery and supplies the agent-level default.
    let pcfg = registry
        .providers()
        .get(&provider_name)
        .ok_or_else(|| Error::Config(format!("unknown provider: {provider_name}")))?;
    let mc = pcfg
        .models
        .get(&model_id)
        .ok_or_else(|| Error::Config(format!("no model `{model_id}` for provider `{provider_name}`")))?;
    let level = resolve_thinking_level(explicit_level, mc, pcfg, config.agent.thinking_level)?;
    Ok((model, level))
}

/// Resolve the effective thinking level for a run.
///
/// Precedence: explicit CLI `:level`, then model default, then provider
/// default, then agent default, then `medium`. `off` is always allowed. A
/// non-`off` level must appear in the model's declared `thinking_levels`; if
/// the model declares none it does not support thinking and a non-`off`
/// explicit request is an error (an implicit default is silently clamped to
/// `off`).
fn resolve_thinking_level(
    explicit: Option<ThinkingLevel>,
    mc: &lofi_types::ModelConfig,
    pcfg: &lofi_types::ProviderConfig,
    agent: Option<ThinkingLevel>,
) -> Result<ThinkingLevel> {
    let desired = explicit
        .or(mc.thinking_level)
        .or(pcfg.thinking_level)
        .or(agent)
        .unwrap_or(ThinkingLevel::Medium);
    if desired == ThinkingLevel::Off {
        return Ok(ThinkingLevel::Off);
    }
    if mc.thinking_levels.is_empty() {
        if explicit.is_some() {
            return Err(Error::Config(format!(
                "model does not support thinking (no thinking_levels declared); cannot use `{}`",
                desired.as_str()
            )));
        }
        return Ok(ThinkingLevel::Off);
    }
    if !mc.thinking_levels.contains(&desired) {
        let allowed: Vec<&str> = mc.thinking_levels.iter().map(|l| l.as_str()).collect();
        return Err(Error::Config(format!(
            "thinking level `{}` not supported by this model; allowed: {}",
            desired.as_str(),
            allowed.join(", ")
        )));
    }
    Ok(desired)
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use async_trait::async_trait;
    use futures::stream;
    use lofi_types::{Api, Usage};
    use tempfile::tempdir;

    /// A canned provider that pops one `Vec<StreamingEvent>` per call.
    struct MockProvider {
        rounds: std::sync::Mutex<Vec<Vec<StreamingEvent>>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn stream(
            &self,
            _model: &Model,
            _messages: &[Message],
            _tools: &[ToolSchema],
        ) -> Result<futures::stream::BoxStream<'static, Result<StreamingEvent>>> {
            let mut rounds = self.rounds.lock().unwrap();
            let evs = if rounds.is_empty() {
                Vec::new()
            } else {
                rounds.remove(0)
            };
            Ok(Box::pin(stream::iter(evs.into_iter().map(Ok))))
        }
    }

    fn model() -> Model {
        Model {
            id: "m".to_string(),
            name: "m".to_string(),
            provider: "p".to_string(),
            api: Api::OpenAiCompletions,
            reasoning: false,
            thinking: lofi_types::ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: None,
            base_url: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        }
    }

    fn agent_with(rounds: Vec<Vec<StreamingEvent>>, root: &std::path::Path) -> Agent {
        Agent {
            provider: Arc::new(MockProvider {
                rounds: std::sync::Mutex::new(rounds),
            }),
            model: model(),
            root: root.to_path_buf(),
            tmp_dir: std::env::temp_dir().join("lofi-agent-test"),
            retry: crate::retry::RetryPolicy::default(),
            system_prompt: "sys".to_string(),
            max_output_tokens: None,
        }
    }

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }

    #[test]
    fn cap_tool_result_keeps_short_unchanged() {
        assert_eq!(cap_tool_result("hello"), "hello");
    }

    #[test]
    fn cap_tool_result_truncates_long_with_marker() {
        let big = "x".repeat(MAX_TOOL_RESULT_BYTES + 1000);
        let capped = cap_tool_result(&big);
        assert!(capped.len() < big.len());
        assert!(capped.contains("[output truncated:"));
        assert!(capped.contains(&format!("{} bytes total", big.len())));
        // Head preserved.
        assert!(capped.starts_with("xxxx"));
    }

    #[test]
    fn cap_tool_result_respects_char_boundary() {
        // Fill to just past the cap with multibyte chars so the boundary cut
        // would land mid-char if unhandled.
        let unit = "é"; // 2 bytes
        let n = MAX_TOOL_RESULT_BYTES / 2 + 50;
        let big = unit.repeat(n);
        let capped = cap_tool_result(&big);
        // The truncated string must be valid UTF-8 (String guarantees it, but
        // confirm it ends with a complete marker line).
        assert!(capped.contains("[output truncated:"));
    }

    #[test]
    fn cap_exec_result_uses_larger_outer_limit() {
        // The exec-level cap is larger than the per-tool cap, so a payload
        // bigger than MAX_TOOL_RESULT_BYTES but under MAX_EXEC_RESULT_BYTES is
        // preserved by cap_exec_result but would be truncated by cap_tool_result.
        let mid = "x".repeat(MAX_TOOL_RESULT_BYTES + 1000);
        assert!(mid.len() < MAX_EXEC_RESULT_BYTES);
        assert_eq!(cap_exec_result(&mid), mid);
        assert_ne!(cap_tool_result(&mid), mid);
        // And the outer cap still truncates when exceeded.
        let huge = "x".repeat(MAX_EXEC_RESULT_BYTES + 5000);
        let capped = cap_exec_result(&huge);
        assert!(capped.contains("[output truncated:"));
        assert!(capped.contains(&format!("{} bytes total", huge.len())));
    }

    #[test]
    fn exec_schema_shape() {
        let s = exec_tool_schema();
        assert_eq!(s.name, "exec");
        assert_eq!(s.input_schema["properties"]["code"]["type"], "string");
        assert_eq!(s.input_schema["required"][0], "code");
        assert!(s.input_schema["properties"].get("strings").is_some());
        assert!(s.input_schema["properties"].get("display").is_some());
    }

    #[test]
    fn parse_exec_input_extracts_fields() {
        let input = serde_json::json!({
            "code": "return 1",
            "strings": { "a": "x", "b": 2 },
            "display": { "hint": "row" }
        });
        let (code, strings, display) = parse_exec_input(&input);
        assert_eq!(code, "return 1");
        assert_eq!(strings.get("a").unwrap(), "x");
        assert_eq!(strings.get("b").unwrap(), "2");
        assert_eq!(display["hint"], "row");
    }

    #[test]
    fn parse_exec_input_missing_code_is_empty() {
        let (code, strings, display) = parse_exec_input(&serde_json::Value::Null);
        assert!(code.is_empty());
        assert!(strings.is_empty());
        assert!(display.is_null());
    }

    #[test]
    fn parse_exec_input_trims_surrounding_newlines() {
        let input = serde_json::json!({ "code": "\nreturn 1\n" });
        let (code, _, _) = parse_exec_input(&input);
        assert_eq!(code, "return 1");
    }

    #[test]
    fn extract_code_prefix_streams_and_trims_leading_newline() {
        // Fragments arrive as the model writes the JSON; the leading newline
        // in the code value must be trimmed so line 1 is real content.
        let cases = [
            (r#"{"code":"\nle"#, "le"),
            (r#"{"code":"\nlet x"#, "let x"),
            (r#"{"code":"\nlet x\nlet y"#, "let x\nlet y"),
            (r#"{"display":"z","code":"\nlet x"#, "let x"),
        ];
        for (raw, want) in cases {
            assert_eq!(extract_code_prefix(raw), want, "raw: {raw}");
        }
    }

    #[test]
    fn initial_history_with_and_without_system() {
        let h = initial_history("sys", "hi");
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].role, Role::System);
        let h = initial_history("", "hi");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].role, Role::User);
    }

    #[tokio::test]
    async fn run_once_text_only_finishes() {
        let dir = tempdir().unwrap();
        let agent = agent_with(
            vec![vec![
                StreamingEvent::TextDelta("hello".to_string()),
                StreamingEvent::Done(Usage::default()),
            ]],
            dir.path(),
        );
        let mut messages = vec![user_msg("hi")];
        let finished = agent.run_once(&mut messages).await.unwrap();
        assert!(finished);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].role, Role::Assistant);
        match &messages[1].blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hello"),
            other => panic!("unexpected block {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_once_tool_call_executes_and_appends_result() {
        let dir = tempdir().unwrap();
        let tool_input = serde_json::json!({ "code": "return 1+1" }).to_string();
        let round1 = vec![
            StreamingEvent::ToolUseStart {
                id: "t1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "t1".to_string(),
                delta: tool_input,
            },
            StreamingEvent::ToolUseEnd {
                id: "t1".to_string(),
            },
            StreamingEvent::Done(Usage::default()),
        ];
        let round2 = vec![
            StreamingEvent::TextDelta("done".to_string()),
            StreamingEvent::Done(Usage::default()),
        ];
        let agent = agent_with(vec![round1, round2], dir.path());
        let mut messages = vec![user_msg("go")];

        let finished = agent.run_once(&mut messages).await.unwrap();
        assert!(!finished);
        // assistant turn + tool-role tool results.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::Tool);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &messages[2].blocks[0]
        else {
            panic!("expected tool_result");
        };
        assert!(!*is_error);
        assert!(content.contains('2'), "content was {content}");

        let finished = agent.run_once(&mut messages).await.unwrap();
        assert!(finished);
    }

    #[tokio::test]
    async fn run_once_tool_error_marks_result_error() {
        let dir = tempdir().unwrap();
        let tool_input =
            serde_json::json!({ "code": "await lofi.read('../escape'); return 1;" }).to_string();
        let round1 = vec![
            StreamingEvent::ToolUseStart {
                id: "t1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "t1".to_string(),
                delta: tool_input,
            },
            StreamingEvent::ToolUseEnd {
                id: "t1".to_string(),
            },
            StreamingEvent::Done(Usage::default()),
        ];
        let agent = agent_with(vec![round1], dir.path());
        let mut messages = vec![user_msg("go")];
        let finished = agent.run_once(&mut messages).await.unwrap();
        assert!(!finished);
        assert_eq!(messages[2].role, Role::Tool);
        let ContentBlock::ToolResult { is_error, .. } = &messages[2].blocks[0] else {
            panic!("expected tool_result");
        };
        assert!(*is_error);
    }

    #[tokio::test]
    async fn run_retries_transient_provider_errors() {
        // First round: a transient 429. Second round: success. The agent
        // should emit RetryStart/RetryEnd and recover.
        let dir = tempdir().unwrap();
        let round1 = vec![StreamingEvent::Error("HTTP 429 Too Many Requests".into())];
        let round2 = vec![
            StreamingEvent::TextDelta("recovered".into()),
            StreamingEvent::Done(Usage::default()),
        ];
        let agent = agent_with(vec![round1, round2], dir.path())
            .with_retry(crate::retry::RetryPolicy {
                max_retries: 3,
                base_delay: Duration::from_millis(1),
            });
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let mut messages = vec![user_msg("go")];
        let result = agent
            .run_continuation(&mut messages, "go".to_string(), tx, None)
            .await;
        assert!(result.is_ok(), "should recover: {result:?}");
        // Drain events and confirm a RetryStart then RetryEnd(success) fired.
        let mut got_start = false;
        let mut got_end_success = false;
        while let Ok(Some(ev)) = tokio::time::timeout(
            Duration::from_millis(100),
            rx.recv(),
        )
        .await
        {
            match ev {
                AgentEvent::RetryStart { attempt, .. } => {
                    assert_eq!(attempt, 1);
                    got_start = true;
                }
                AgentEvent::RetryEnd { success, attempt, .. } => {
                    assert_eq!(attempt, 1);
                    if success {
                        got_end_success = true;
                    }
                }
                _ => {}
            }
        }
        assert!(got_start, "expected a RetryStart event");
        assert!(
            got_end_success,
            "expected a RetryEnd(success) event"
        );
    }

    #[tokio::test]
    async fn run_does_not_retry_non_transient_errors() {
        // A 401 Unauthorized is not retryable: the run should surface the error
        // immediately without consuming a second round.
        let dir = tempdir().unwrap();
        let round1 = vec![StreamingEvent::Error("401 Unauthorized".into())];
        let round2 = vec![
            StreamingEvent::TextDelta("should-not-happen".into()),
            StreamingEvent::Done(Usage::default()),
        ];
        let agent = agent_with(vec![round1, round2], dir.path())
            .with_retry(crate::retry::RetryPolicy {
                max_retries: 3,
                base_delay: Duration::from_millis(1),
            });
        let (tx, _rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
        let mut messages = vec![user_msg("go")];
        let result = agent
            .run_continuation(&mut messages, "go".to_string(), tx, None)
            .await;
        assert!(result.is_err(), "non-retryable errors should propagate");
    }

    #[tokio::test]
    async fn run_exits_when_receiver_dropped() {
        let dir = tempdir().unwrap();
        let agent = agent_with(
            vec![vec![
                StreamingEvent::TextDelta("hi".to_string()),
                StreamingEvent::Done(Usage::default()),
            ]],
            dir.path(),
        );
        let (tx, rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        // Drop the receiver before running so the channel is closed.
        drop(rx);
        // The run should exit gracefully (Ok) rather than hang or surface an
        // error: a dropped receiver is a cancellation, not a provider fault.
        let result =
            tokio::time::timeout(Duration::from_secs(2), agent.run("hi".to_string(), tx)).await;
        let inner = result.unwrap(); // timeout => run hung
        assert!(inner.is_ok(), "run should exit gracefully, got {inner:?}");
    }

use indexmap::IndexMap;
    use lofi_types::{
        AgentConfig, ApiTypeMapping, Config, ModelConfig, PricingConvention,
        PricingFieldMappings, ProviderConfig, ThinkingLevel,
    };

    /// A model config declaring the full low/medium/high/xhigh set, no default.
    fn mc() -> ModelConfig {
        ModelConfig {
            name: None,
            api_type: None,
            reasoning: None,
            supports_image: None,
            context_window: None,
            max_tokens: None,
            thinking_levels: vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::XHigh,
            ],
            thinking_level: None,
            base_url: None,
            input_price: None,
            output_price: None,
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        }
    }

    fn mc_levels(levels: &[ThinkingLevel]) -> ModelConfig {
        let mut m = mc();
        m.thinking_levels = levels.to_vec();
        m
    }

    fn mc_default(level: ThinkingLevel) -> ModelConfig {
        let mut m = mc();
        m.thinking_level = Some(level);
        m
    }

    fn models(entries: &[(&str, ModelConfig)]) -> IndexMap<String, ModelConfig> {
        let mut map = IndexMap::new();
        for (k, v) in entries {
            map.insert((*k).to_string(), v.clone());
        }
        map
    }

    fn provider(
        api: Api,
        key: Option<&str>,
        models: IndexMap<String, ModelConfig>,
        thinking_level: Option<ThinkingLevel>,
    ) -> ProviderConfig {
        let mut mappings = IndexMap::new();
        mappings.insert(
            api.as_str().to_string(),
            ApiTypeMapping {
                path: None,
                pricing_field_mappings: None,
            },
        );
        ProviderConfig {
            api_type: Some(api),
            api_types: mappings,
            base_url: Some("https://api.example.com".to_string()),
            pricing_convention: PricingConvention::PerToken,
            pricing_field_mappings: PricingFieldMappings::default(),
            env_name: None,
            api_key: key.map(str::to_string),
            headers: None,
            models,
            auto_models: None,
            no_auth: false,
            thinking_level,
            thinking_levels: Vec::new(),
        }
    }

    fn build(providers: IndexMap<String, ProviderConfig>) -> (Config, ModelRegistry) {
        let cfg = Config {
            agent: AgentConfig::default(),
            default_provider: None,
            default_model: None,
            providers,
        };
        let reg = ModelRegistry::load(&cfg).unwrap();
        (cfg, reg)
    }

    fn one_provider(p: ProviderConfig) -> IndexMap<String, ProviderConfig> {
        let mut m = IndexMap::new();
        m.insert("openai".to_string(), p);
        m
    }

    #[test]
    fn select_model_explicit_qualified() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk-test"),
            models(&[("gpt-4o", mc())]),
            None,
        )));
        let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
        assert_eq!(m.id, "gpt-4o");
        assert_eq!(m.provider, "openai");
        assert_eq!(level, ThinkingLevel::Medium);
    }

    #[test]
    fn select_model_explicit_level_suffix() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk-test"),
            models(&[("gpt-4o", mc())]),
            None,
        )));
        let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:xhigh")).unwrap();
        assert_eq!(m.id, "gpt-4o");
        assert_eq!(level, ThinkingLevel::XHigh);
    }

    #[test]
    fn select_model_off_level_always_allowed() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk-test"),
            models(&[("gpt-4o", mc_levels(&[]))]),
            None,
        )));
        let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:off")).unwrap();
        assert_eq!(m.id, "gpt-4o");
        assert_eq!(level, ThinkingLevel::Off);
    }

    #[test]
    fn select_model_rejects_bare_model() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk-test"),
            models(&[("gpt-4o", mc())]),
            None,
        )));
        let err = select_model(&reg, &cfg, Some("gpt-4o")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn select_model_rejects_unknown_level() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk-test"),
            models(&[("gpt-4o", mc())]),
            None,
        )));
        let err = select_model(&reg, &cfg, Some("openai/gpt-4o:bogus")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn select_model_rejects_level_not_declared() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk-test"),
            models(&[("gpt-4o", mc_levels(&[ThinkingLevel::Low, ThinkingLevel::Medium]))]),
            None,
        )));
        let err = select_model(&reg, &cfg, Some("openai/gpt-4o:high")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn select_model_first_available_when_no_query() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            provider(Api::OpenAiCompletions, Some("sk"), models(&[("gpt-4o", mc())]), None),
        );
        providers.insert(
            "anthropic".to_string(),
            provider(Api::AnthropicMessages, Some("sk"), models(&[("claude", mc())]), None),
        );
        let (cfg, reg) = build(providers);
        let (m, _) = select_model(&reg, &cfg, None).unwrap();
        assert_eq!(m.provider, "openai");
        assert_eq!(m.id, "gpt-4o");
    }

    #[test]
    fn select_model_uses_default_model_when_no_query() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            provider(Api::OpenAiCompletions, Some("sk"), models(&[("gpt-4o", mc())]), None),
        );
        providers.insert(
            "anthropic".to_string(),
            provider(Api::AnthropicMessages, Some("sk"), models(&[("claude", mc())]), None),
        );
        let cfg = Config {
            agent: AgentConfig::default(),
            default_provider: None,
            default_model: Some("anthropic/claude".to_string()),
            providers,
        };
        let reg = ModelRegistry::load(&cfg).unwrap();
        let (m, _) = select_model(&reg, &cfg, None).unwrap();
        // default_model wins over first-available.
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.id, "claude");
    }

    #[test]
    fn select_model_uses_default_provider_when_no_query() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            provider(Api::OpenAiCompletions, Some("sk"), models(&[("gpt-4o", mc())]), None),
        );
        providers.insert(
            "anthropic".to_string(),
            provider(Api::AnthropicMessages, Some("sk"), models(&[("claude", mc())]), None),
        );
        let cfg = Config {
            agent: AgentConfig::default(),
            default_provider: Some("anthropic".to_string()),
            default_model: None,
            providers,
        };
        let reg = ModelRegistry::load(&cfg).unwrap();
        let (m, _) = select_model(&reg, &cfg, None).unwrap();
        // default_provider selects that provider's first available model.
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.id, "claude");
    }

    #[test]
    fn select_model_default_model_overrides_default_provider() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            provider(Api::OpenAiCompletions, Some("sk"), models(&[("gpt-4o", mc())]), None),
        );
        providers.insert(
            "anthropic".to_string(),
            provider(Api::AnthropicMessages, Some("sk"), models(&[("claude", mc())]), None),
        );
        let cfg = Config {
            agent: AgentConfig::default(),
            default_provider: Some("anthropic".to_string()),
            default_model: Some("openai/gpt-4o".to_string()),
            providers,
        };
        let reg = ModelRegistry::load(&cfg).unwrap();
        let (m, _) = select_model(&reg, &cfg, None).unwrap();
        // default_model wins over default_provider.
        assert_eq!(m.provider, "openai");
        assert_eq!(m.id, "gpt-4o");
    }

    #[test]
    fn select_model_explicit_query_overrides_defaults() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            provider(Api::OpenAiCompletions, Some("sk"), models(&[("gpt-4o", mc())]), None),
        );
        providers.insert(
            "anthropic".to_string(),
            provider(Api::AnthropicMessages, Some("sk"), models(&[("claude", mc())]), None),
        );
        let cfg = Config {
            agent: AgentConfig::default(),
            default_provider: Some("openai".to_string()),
            default_model: Some("openai/gpt-4o".to_string()),
            providers,
        };
        let reg = ModelRegistry::load(&cfg).unwrap();
        let (m, _) = select_model(&reg, &cfg, Some("anthropic/claude")).unwrap();
        // Explicit --model wins over both defaults.
        assert_eq!(m.provider, "anthropic");
        assert_eq!(m.id, "claude");
    }

    #[test]
    fn select_model_no_models_error() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            None,
            models(&[("gpt-4o", mc())]),
            None,
        )));
        let err = select_model(&reg, &cfg, None).unwrap_err();
        assert!(matches!(err, Error::NoModels(_)));
    }

    #[test]
    fn select_model_rejects_keyless_provider_model() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            provider(Api::OpenAiCompletions, Some("sk"), models(&[("gpt-4o", mc())]), None),
        );
        providers.insert(
            "local".to_string(),
            provider(Api::OpenAiCompletions, None, models(&[("local-model", mc())]), None),
        );
        let (cfg, reg) = build(providers);
        let err = select_model(&reg, &cfg, Some("local/local-model")).unwrap_err();
        // A disabled (keyless) provider now reports as "no models" rather than
        // a config error, so the TUI can launch in no-model mode.
        assert!(matches!(err, Error::NoModels(_)));
    }

    #[test]
    fn select_model_thinking_precedence_model_default() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc_default(ThinkingLevel::High))]),
            None,
        )));
        let (_, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
        assert_eq!(level, ThinkingLevel::High);
    }

    #[test]
    fn select_model_thinking_precedence_provider_default() {
        let (cfg, reg) = build(one_provider(provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            Some(ThinkingLevel::Low),
        )));
        let (_, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
        assert_eq!(level, ThinkingLevel::Low);
    }

    #[test]
    fn add_usage_bills_cache_tokens_at_their_own_rate() {
        let model = Model {
            id: "m".to_string(),
            name: "m".to_string(),
            provider: "p".to_string(),
            api: Api::OpenAiCompletions,
            reasoning: false,
            thinking: ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: None,
            base_url: None,
            input_price: Some(1.0),
            output_price: Some(2.0),
            cache_read_price: Some(0.1),
            cache_write_price: Some(0.5),
            per_request_price: None,
        };
        let mut stats = TurnStats::new();
        stats.add_usage(
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 2_000_000,
                cache_write_tokens: 1_000_000,
            },
            &model,
        );
        // 1M input @ $1 + 2M cache-read @ $0.1 + 1M cache-write @ $0.5 + 1M output @ $2
        // = 1 + 0.2 + 0.5 + 2 = $3.70
        assert!((stats.cost - 3.70).abs() < 1e-9, "cost was {}", stats.cost);
    }

    #[test]
    fn add_usage_falls_back_to_input_rate_when_cache_prices_unset() {
        let model = Model {
            id: "m".to_string(),
            name: "m".to_string(),
            provider: "p".to_string(),
            api: Api::OpenAiCompletions,
            reasoning: false,
            thinking: ThinkingLevel::Off,
            supports_image: false,
            context_window: None,
            max_tokens: None,
            base_url: None,
            input_price: Some(1.0),
            output_price: Some(2.0),
            cache_read_price: None,
            cache_write_price: None,
            per_request_price: None,
        };
        let mut stats = TurnStats::new();
        stats.add_usage(
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_read_tokens: 2_000_000,
                cache_write_tokens: 1_000_000,
            },
            &model,
        );
        // 1M input + 2M cache-read + 1M cache-write all @ $1 = $4, plus 1M output @ $2 = $6
        assert!((stats.cost - 6.0).abs() < 1e-9, "cost was {}", stats.cost);
    }
}
