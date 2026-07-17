//! The agent loop.
//!
//! Drives a multi-turn conversation with a model, streaming [`AgentEvent`]s to
//! callers and dispatching the single LLM-facing tool — `exec` — into the
//! code-mode sandbox ([`crate::code`]). The agent owns the resolved provider,
//! the selected model, the workspace root, and the system prompt; subagents
//! reuse the same shape via the [`crate::subagent::RoundTrip`] impl.
//!
//! ## No iteration cap (v1)
//!
//! Following the plan, neither [`Agent::run`] nor subagent runs impose an
//! iteration cap. Runaway loops are bounded by **per-call timeouts**: each
//! provider stream is wrapped in [`tokio::time::timeout`] with
//! [`DEFAULT_STREAM_TIMEOUT`], and each `exec` call inherits
//! [`crate::code::DEFAULT_GUEST_TIMEOUT`] via [`crate::code::ExecOptions`]. A
//! genuine infinite loop would eventually trip one of those.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::LocalBoxFuture;
use futures::StreamExt;
use lofi_types::{ContentBlock, Message, Model, Role, StreamingEvent, Usage};
use tokio::sync::mpsc::Sender;

use crate::code::{exec, AgentFn, ExecCtx, ExecOptions};
use crate::config_loader::{apply_api_key_override, load_config_or_default};
use crate::error::{Error, Result};
use crate::ir::chat::ToolSchema;
use crate::ir::codec::assemble_message;
use crate::models::ModelRegistry;
use crate::providers::{open, Provider};
use crate::subagent::{self, RoundTrip, SubagentCtx, SubagentOptions, DEFAULT_ROUND_TRIP_TIMEOUT};

/// The system prompt shipped with lofi, `include_str!`'d from
/// `prompts/system.md`.
pub const SYSTEM_PROMPT: &str = include_str!("prompts/system.md");

/// Per-stream wall-clock budget. The provider HTTP client already has its own
/// 5min timeout; this is an outer guard so a stalled stream cannot hang the
/// agent indefinitely.
const DEFAULT_STREAM_TIMEOUT: Duration = Duration::from_mins(5);

/// Events emitted by the agent loop to the TUI / print driver.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// A chunk of assistant text.
    Text(String),
    /// A tool call has begun.
    ToolStart {
        /// The tool-call id assigned by the provider.
        id: String,
        /// The tool name (currently always `exec`).
        name: String,
    },
    /// A tool call has completed with `result` (a JSON string for success, an
    /// error message for failure).
    ToolEnd {
        /// The tool-call id.
        id: String,
        /// The tool result payload.
        result: String,
    },
    /// The run finished cleanly with token usage.
    Done(Usage),
    /// A provider error was encountered mid-stream.
    Error(String),
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
        system_prompt: String,
        max_output_tokens: Option<u64>,
    ) -> Self {
        Self {
            provider: Arc::from(provider),
            model,
            root,
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

    /// The maximum output tokens hint, if set. Not yet wired into the
    /// provider request body in v1.
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
        loop {
            let finished = self.run_once_inner(&mut messages, Some(&tx)).await?;
            if finished || tx.is_closed() {
                return Ok(());
            }
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
        self.run_once_inner(messages, None).await
    }

    /// Shared core of [`run_once`] with an optional event sender.
    ///
    /// Events are emitted via [`Sender::try_send`] (non-blocking) so a slow
    /// receiver never stalls the stream; the outer [`run`](Self::run) loop
    /// checks `tx.is_closed()` to exit when the consumer is gone.
    async fn run_once_inner(
        &self,
        messages: &mut Vec<Message>,
        tx: Option<&Sender<AgentEvent>>,
    ) -> Result<bool> {
        let schema = exec_tool_schema();
        let stream = self
            .provider
            .stream(&self.model, messages, &[schema])
            .await?;
        let mut stream = stream;

        let mut events: Vec<StreamingEvent> = Vec::new();
        let collect = async {
            while let Some(ev) = stream.next().await {
                match ev {
                    Ok(e) => {
                        match &e {
                            StreamingEvent::TextDelta(d) => {
                                emit(tx, AgentEvent::Text(d.clone()));
                            }
                            StreamingEvent::ToolUseStart { id, name } => {
                                emit(
                                    tx,
                                    AgentEvent::ToolStart {
                                        id: id.clone(),
                                        name: name.clone(),
                                    },
                                );
                            }
                            StreamingEvent::Done(usage) => {
                                emit(tx, AgentEvent::Done(*usage));
                            }
                            StreamingEvent::Error(msg) => {
                                emit(tx, AgentEvent::Error(msg.clone()));
                            }
                            _ => {}
                        }
                        events.push(e);
                    }
                    Err(err) => {
                        emit(tx, AgentEvent::Error(err.to_string()));
                        return Err(err);
                    }
                }
            }
            Ok(())
        };

        tokio::time::timeout(DEFAULT_STREAM_TIMEOUT, collect)
            .await
            .map_err(|_| Error::Provider("stream timeout".into()))??;

        let assistant = assemble_message(&events);
        messages.push(assistant.clone());

        let tool_uses: Vec<(&str, &serde_json::Value)> = assistant
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, input, .. } => Some((id.as_str(), input)),
                _ => None,
            })
            .collect();

        if tool_uses.is_empty() {
            return Ok(true);
        }

        let agent_fn = self.make_agent_fn();
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        for (id, input) in tool_uses {
            let (code, strings, _display) = parse_exec_input(input);
            let exec_ctx = ExecCtx {
                root: self.root.clone(),
                strings,
                agent: Some(agent_fn.clone()),
            };
            let outcome = exec(&code, &exec_ctx, &ExecOptions::default()).await;
            let (content, is_error) = match outcome {
                Ok(r) => {
                    let payload = serde_json::json!({ "value": r.value, "logs": r.logs });
                    let content =
                        serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
                    (content, false)
                }
                Err(e) => (e.to_string(), true),
            };
            emit(
                tx,
                AgentEvent::ToolEnd {
                    id: id.to_string(),
                    result: content.clone(),
                },
            );
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content,
                is_error,
            });
        }

        messages.push(Message {
            role: Role::User,
            blocks: results,
        });
        Ok(false)
    }

    /// Build the `pi.agent` / `pi.spawn` callback used by [`ExecCtx`].
    ///
    /// The closure clones the agent (cheap — `Arc` provider) and runs
    /// [`subagent::run`] with a fresh nested loop reusing the same provider,
    /// model, and workspace root. The subagent's final assistant text becomes
    /// the `pi.agent()` return value inside the sandbox.
    fn make_agent_fn(&self) -> AgentFn {
        let self_clone = self.clone();
        Arc::new(move |req: crate::code::AgentRequest| {
            let agent = self_clone.clone();
            Box::pin(async move {
                let opts = SubagentOptions {
                    system: agent.system_prompt.clone(),
                    timeout: DEFAULT_ROUND_TRIP_TIMEOUT,
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

/// The single LLM-facing tool schema: `exec` with Pi's `fabric_exec` shape so
/// prompts transfer.
#[must_use]
pub fn exec_tool_schema() -> ToolSchema {
    ToolSchema {
        name: "exec".to_string(),
        description: "Compile and run a TypeScript program in a sandboxed QuickJS runtime. The program has access to a `pi` object with file/shell/search tools (read, ls, find, grep, write, edit, bash) and an `agent(prompt, opts?)` subagent helper. Top-level await and return are supported. The returned value is sent back as the tool result; keep it compact and final.".to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "TypeScript source. Top-level await/return supported."
                },
                "strings": {
                    "type": "object",
                    "description": "Named string constants exposed as the global `π` / `pi_strings` objects."
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

/// Parse an `exec` tool input into `(code, strings, display)`.
///
/// Missing `code` yields an empty string (which compiles to a no-op).
/// `strings` values are coerced to strings via `serde_json` for non-string
/// entries. `display` is returned as-is for future use.
fn parse_exec_input(
    input: &serde_json::Value,
) -> (String, HashMap<String, String>, serde_json::Value) {
    let code = input
        .get("code")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
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

/// Best-effort non-blocking event emit. A full or closed channel drops the
/// event rather than stalling the stream; the outer `run` loop detects a
/// closed channel and exits gracefully.
fn emit(tx: Option<&Sender<AgentEvent>>, ev: AgentEvent) {
    if let Some(t) = tx {
        let _ = t.try_send(ev);
    }
}

/// Options for the interactive TUI session.
///
/// Mirrors the `--print` flags' resolution fields but with no prompt — the
/// prompts come from the TUI input box at runtime.
#[derive(Debug, Clone)]
pub struct InteractiveOptions {
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Optional override for the config file path.
    pub config_path: Option<PathBuf>,
    /// Optional `--provider` selection.
    pub provider: Option<String>,
    /// Optional `--model` id / qualifier / pattern.
    pub model: Option<String>,
    /// Optional literal `--api-key` override for the selected provider.
    pub api_key: Option<String>,
}

impl InteractiveOptions {
    /// Construct an `InteractiveOptions` with a workspace root, leaving the
    /// flag-style fields at their defaults.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            config_path: None,
            provider: None,
            model: None,
            api_key: None,
        }
    }

    /// Override the config file path.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Override the selected provider.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Override the model id / pattern.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the API key for the selected provider.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// Build an [`Agent`] and its selected [`Model`] from the user config and
/// the CLI overrides.
///
/// Shared by [`run_print`] and [`run_interactive`] so the resolution ladder
/// (config load → provider selection → `--api-key` override → registry +
/// discovery → model resolution → transport construction) stays in one place.
/// Remote discovery failures fall back to static-only [`ModelRegistry::load`],
/// matching the v1 "do not fatal on discovery" policy.
///
/// # Errors
/// Propagates [`Error`] from config load, provider/model resolution, or
/// provider construction.
async fn build_agent(
    config_path: Option<&std::path::Path>,
    provider: Option<&str>,
    model: Option<&str>,
    api_key: Option<&str>,
    root: &std::path::Path,
) -> Result<(Agent, Model)> {
    let config_path = match config_path {
        Some(p) => p.to_path_buf(),
        None => crate::config_loader::user_config_path()?,
    };
    let mut config = load_config_or_default(&config_path).await?;

    let provider_name = select_provider(&config, provider)?;

    if let Some(key) = api_key {
        apply_api_key_override(&mut config, &provider_name, key.to_string())?;
    }

    let registry = match ModelRegistry::load_async(&config).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "async model load failed; falling back to static");
            ModelRegistry::load(&config)?
        }
    };

    let model = select_model(
        &registry,
        &provider_name,
        config.default_model.as_deref(),
        model,
    )?;

    let provider_cfg = config
        .providers
        .get(&provider_name)
        .ok_or_else(|| Error::Config(format!("provider not found: {provider_name}")))?;
    let provider = open(model.api, provider_cfg)?;

    let agent = Agent::new(
        provider,
        model.clone(),
        root.to_path_buf(),
        SYSTEM_PROMPT.to_string(),
        None,
    );
    Ok((agent, model))
}

/// Run the interactive TUI session.
///
/// Builds the agent exactly like [`run_print`] (via [`build_agent`]) and
/// hands it to [`crate::tui::run`]. Terminal setup/teardown is owned by the
/// TUI's `Drop` guard; this function returns after the user quits.
///
/// # Errors
/// Propagates [`Error`] from agent construction or the terminal session.
pub async fn run_interactive(opts: InteractiveOptions) -> Result<()> {
    let (agent, model) = build_agent(
        opts.config_path.as_deref(),
        opts.provider.as_deref(),
        opts.model.as_deref(),
        opts.api_key.as_deref(),
        opts.root.as_path(),
    )
    .await?;
    let label = format!("{}/{}", model.provider, model.id);
    crate::tui::run(agent, label).await
}

/// Options for the non-interactive `--print` path.
///
/// `config_path` defaults to the user config file
/// ([`crate::config_loader::user_config_path`]) when `None`. `provider` and
/// `model` mirror the `--provider` / `--model` flags; `api_key` is the literal
/// `--api-key` override applied to the selected provider after config load.
#[derive(Debug, Clone)]
pub struct PrintOptions {
    /// The user prompt to send.
    pub prompt: String,
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Optional override for the config file path.
    pub config_path: Option<PathBuf>,
    /// Optional `--provider` selection.
    pub provider: Option<String>,
    /// Optional `--model` id / qualifier / pattern.
    pub model: Option<String>,
    /// Optional literal `--api-key` override for the selected provider.
    pub api_key: Option<String>,
}

impl PrintOptions {
    /// Construct a `PrintOptions` with a prompt and workspace root, leaving
    /// all flag-style fields at their defaults.
    #[must_use]
    pub fn new(prompt: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            root: root.into(),
            config_path: None,
            provider: None,
            model: None,
            api_key: None,
        }
    }

    /// Override the config file path.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Override the selected provider.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Override the model id / pattern.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the API key for the selected provider.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// Resolve which provider to use given the request and config.
///
/// Prefers an explicit `--provider`; otherwise falls back to the config's
/// `default_provider`; otherwise the first provider in the config (stable
/// insertion order from `toml`'s `HashMap` is not guaranteed, but a config
/// with exactly one provider is the common case).
fn select_provider(config: &lofi_types::Config, requested: Option<&str>) -> Result<String> {
    if let Some(name) = requested {
        if !config.providers.contains_key(name) {
            return Err(Error::Config(format!("unknown provider: {name}")));
        }
        return Ok(name.to_string());
    }
    if let Some(default) = &config.default_provider {
        if !config.providers.contains_key(default) {
            return Err(Error::Config(format!(
                "default_provider refers to unknown provider: {default}"
            )));
        }
        return Ok(default.clone());
    }
    // Single-provider configs need no `default_provider`.
    if config.providers.len() == 1 {
        return config
            .providers
            .keys()
            .next()
            .cloned()
            .ok_or_else(|| Error::Config("no providers configured".into()));
    }
    Err(Error::Config(
        "no provider selected: pass --provider, set default_provider in config, or set OPENAI_API_KEY / ANTHROPIC_API_KEY"
            .into(),
    ))
}

/// Pick the model to run against, given a registry, the selected provider,
/// and an optional `--model` query.
///
/// Resolution ladder:
/// 1. if `model_query` is given, try [`ModelRegistry::resolve`] (qualified
///    `provider/id`) then [`ModelRegistry::resolve_by_pattern`];
/// 2. else use the config's `default_model` (resolved the same way);
/// 3. else fall back to the first available model for the selected provider;
/// 4. else the first available model overall.
///
/// Only models from [`ModelRegistry::available`] (provider has a resolved
/// key) are eligible, so a configured-but-keyless provider is never silently
/// selected.
///
/// # Errors
/// Returns [`Error::Config`] with a clear message if the query cannot be
/// resolved or no available model exists.
pub(crate) fn select_model(
    registry: &ModelRegistry,
    provider: &str,
    default_model: Option<&str>,
    model_query: Option<&str>,
) -> Result<Model> {
    let available = registry.available();
    if available.is_empty() {
        return Err(Error::Config(
            "no models available (no provider has a resolved api_key)".into(),
        ));
    }

    let query = model_query.or(default_model);
    if let Some(q) = query {
        if let Some(m) = registry.resolve(q) {
            return Ok(m.clone());
        }
        if let Some(m) = registry.resolve_by_pattern(q) {
            return Ok(m.clone());
        }
        return Err(Error::Config(format!(
            "could not resolve model `{q}`; available: {}",
            registry.list_models_print()
        )));
    }

    // No explicit query: prefer the selected provider's models, else any.
    if let Some(m) = available.iter().find(|m| m.provider == provider) {
        return Ok(m.clone());
    }
    Ok(available
        .first()
        .ok_or_else(|| Error::Config("no available model".into()))?
        .clone())
}

/// Run a single non-interactive prompt and stream assistant text to stdout.
///
/// Loads the user config (resolving `api_key`/`header` values), applies the
/// optional `--api-key` override, builds a [`ModelRegistry`] with remote
/// discovery, resolves the provider + model, opens the provider transport,
/// then drives [`Agent::run`] on a background task. The caller subscribes to
/// [`AgentEvent`]s:
/// - [`AgentEvent::Text`] deltas are written to stdout via
///   `std::io::stdout().write_all` (never `println!`, which is denied by the
///   workspace lints);
/// - [`AgentEvent::Error`] is written to stderr;
/// - [`AgentEvent::Done`] writes a trailing newline to stdout;
/// - `ToolStart` / `ToolEnd` are surfaced as short stderr markers so stdout
///   stays clean for piping.
///
/// # Errors
/// Propagates [`Error`] from config load, model resolution, provider
/// construction, or the agent run. The binary caller is responsible for
/// translating the returned error into a nonzero exit code.
pub async fn run_print(opts: PrintOptions) -> Result<()> {
    let (agent, _model) = build_agent(
        opts.config_path.as_deref(),
        opts.provider.as_deref(),
        opts.model.as_deref(),
        opts.api_key.as_deref(),
        opts.root.as_path(),
    )
    .await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);

    let prompt = opts.prompt.clone();
    let agent_run = agent.run(prompt, tx);

    // The agent future is not `Send` (the QuickJS `AsyncContext` is not
    // `Sync`), so it cannot be `tokio::spawn`'d. Drive it concurrently with
    // the event consumer on the same task: when the agent finishes it drops
    // `tx`, `rx.recv()` returns `None`, and the consumer drains.
    let consumer = async {
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::Text(delta) => {
                    stdout.write_all(delta.as_bytes())?;
                }
                AgentEvent::Error(msg) => {
                    writeln!(stderr, "error: {msg}")?;
                }
                AgentEvent::ToolStart { name, .. } => {
                    writeln!(stderr, "[{name}]")?;
                }
                AgentEvent::ToolEnd { .. } => {}
                AgentEvent::Done(_) => {
                    stdout.write_all(b"\n")?;
                }
            }
        }
        stdout.flush()?;
        stderr.flush()?;
        Ok::<(), Error>(())
    };

    let (agent_res, consumer_res) = tokio::join!(agent_run, consumer);
    consumer_res?;
    agent_res?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use async_trait::async_trait;
    use futures::stream::{self, StreamExt as _};
    use lofi_types::{Api, Usage};
    use tempfile::tempdir;

    /// A canned provider that pops one `Vec<StreamingEvent>` per call.
    struct MockProvider {
        rounds: std::sync::Mutex<Vec<Vec<StreamingEvent>>>,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn list_models(&self) -> Result<Vec<Model>> {
            Ok(Vec::new())
        }
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
            supports_image: false,
            context_window: None,
            max_tokens: None,
        }
    }

    fn agent_with(rounds: Vec<Vec<StreamingEvent>>, root: &std::path::Path) -> Agent {
        Agent {
            provider: Arc::new(MockProvider {
                rounds: std::sync::Mutex::new(rounds),
            }),
            model: model(),
            root: root.to_path_buf(),
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
        // assistant turn + user-role tool results.
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2].role, Role::User);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &messages[2].blocks[0]
        else {
            panic!("expected tool_result");
        };
        assert!(!*is_error);
        assert!(content.contains("2"), "content was {content}");

        let finished = agent.run_once(&mut messages).await.unwrap();
        assert!(finished);
    }

    #[tokio::test]
    async fn run_once_tool_error_marks_result_error() {
        let dir = tempdir().unwrap();
        let tool_input =
            serde_json::json!({ "code": "await pi.read('../escape'); return 1;" }).to_string();
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
        let ContentBlock::ToolResult { is_error, .. } = &messages[2].blocks[0] else {
            panic!("expected tool_result");
        };
        assert!(*is_error);
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
        let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(8);
        // Drop the receiver before running so the channel is closed.
        drop(rx);
        // The run should exit gracefully (Ok) rather than hang.
        let result =
            tokio::time::timeout(Duration::from_secs(2), agent.run("hi".to_string(), tx)).await;
        assert!(result.is_ok(), "run did not exit on closed channel");
    }

    fn registry_with(
        providers: std::collections::HashMap<String, lofi_types::ProviderConfig>,
    ) -> ModelRegistry {
        let cfg = lofi_types::Config {
            providers,
            default_provider: None,
            default_model: None,
        };
        ModelRegistry::load(&cfg).unwrap()
    }

    fn provider_with_key(
        api: Api,
        models: Vec<lofi_types::ModelConfig>,
    ) -> lofi_types::ProviderConfig {
        lofi_types::ProviderConfig {
            base_url: "https://api.example.com/v1".to_string(),
            api,
            api_key: Some("sk-test".to_string()),
            headers: None,
            models,
            discover: None,
        }
    }

    fn mc(id: &str) -> lofi_types::ModelConfig {
        lofi_types::ModelConfig {
            id: id.to_string(),
            name: None,
            reasoning: None,
            supports_image: None,
            context_window: None,
            max_tokens: None,
        }
    }

    #[test]
    fn print_options_builder() {
        let opts = PrintOptions::new("hello", "/tmp/ws")
            .with_provider("openai")
            .with_model("gpt-4o")
            .with_api_key("sk-override")
            .with_config_path("/tmp/cfg.toml");
        assert_eq!(opts.prompt, "hello");
        assert_eq!(opts.root, PathBuf::from("/tmp/ws"));
        assert_eq!(opts.provider.as_deref(), Some("openai"));
        assert_eq!(opts.model.as_deref(), Some("gpt-4o"));
        assert_eq!(opts.api_key.as_deref(), Some("sk-override"));
        assert_eq!(
            opts.config_path.as_deref(),
            Some(std::path::Path::new("/tmp/cfg.toml"))
        );
    }

    #[test]
    fn select_model_explicit_qualified() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![mc("gpt-4o")]),
        );
        let reg = registry_with(providers);
        let m = select_model(&reg, "openai", None, Some("openai/gpt-4o")).unwrap();
        assert_eq!(m.id, "gpt-4o");
        assert_eq!(m.provider, "openai");
    }

    #[test]
    fn select_model_pattern_fallback() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![mc("gpt-4o-mini")]),
        );
        let reg = registry_with(providers);
        let m = select_model(&reg, "openai", None, Some("mini")).unwrap();
        assert_eq!(m.id, "gpt-4o-mini");
    }

    #[test]
    fn select_model_uses_default_model_when_no_query() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(
                Api::OpenAiCompletions,
                vec![mc("gpt-4o"), mc("gpt-4o-mini")],
            ),
        );
        let reg = registry_with(providers);
        let m = select_model(&reg, "openai", Some("openai/gpt-4o-mini"), None).unwrap();
        assert_eq!(m.id, "gpt-4o-mini");
    }

    #[test]
    fn select_model_defaults_to_provider_first_available() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![mc("gpt-4o")]),
        );
        let reg = registry_with(providers);
        let m = select_model(&reg, "openai", None, None).unwrap();
        assert_eq!(m.id, "gpt-4o");
        assert_eq!(m.provider, "openai");
    }

    #[test]
    fn select_model_errors_when_query_unresolvable() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![mc("gpt-4o")]),
        );
        let reg = registry_with(providers);
        let err = select_model(&reg, "openai", None, Some("nope")).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn select_model_errors_when_nothing_available() {
        // Provider with no key -> nothing available.
        let mut providers = std::collections::HashMap::new();
        let mut p = provider_with_key(Api::OpenAiCompletions, vec![mc("gpt-4o")]);
        p.api_key = None;
        providers.insert("openai".to_string(), p);
        let reg = registry_with(providers);
        let err = select_model(&reg, "openai", None, None).unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[test]
    fn select_provider_explicit_unknown_errors() {
        let cfg = lofi_types::Config {
            providers: std::collections::HashMap::new(),
            default_provider: None,
            default_model: None,
        };
        assert!(select_provider(&cfg, Some("ghost")).is_err());
    }

    #[test]
    fn select_provider_single_provider_default() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![]),
        );
        let cfg = lofi_types::Config {
            providers,
            default_provider: None,
            default_model: None,
        };
        assert_eq!(select_provider(&cfg, None).unwrap(), "openai");
    }

    #[test]
    fn select_provider_uses_config_default() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![]),
        );
        providers.insert(
            "anthropic".to_string(),
            provider_with_key(Api::AnthropicMessages, vec![]),
        );
        let cfg = lofi_types::Config {
            providers,
            default_provider: Some("anthropic".to_string()),
            default_model: None,
        };
        assert_eq!(select_provider(&cfg, None).unwrap(), "anthropic");
    }

    #[test]
    fn select_provider_ambiguous_without_default_errors() {
        let mut providers = std::collections::HashMap::new();
        providers.insert(
            "openai".to_string(),
            provider_with_key(Api::OpenAiCompletions, vec![]),
        );
        providers.insert(
            "anthropic".to_string(),
            provider_with_key(Api::AnthropicMessages, vec![]),
        );
        let cfg = lofi_types::Config {
            providers,
            default_provider: None,
            default_model: None,
        };
        assert!(select_provider(&cfg, None).is_err());
    }
}
