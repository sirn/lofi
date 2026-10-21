use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lofi_error::{Error, Result};
use lofi_types::ThinkingLevel;
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::{mcp::Bridge, rpc};

#[cfg(test)]
mod tests;
use crate::agent::{AgentEvent, ToolCallContext, ToolExecutor};

const EXEC: &str = "mcp__lofi__exec";

/// Effort levels `--effort` accepts, in picker order. `Off` is absent: it
/// means "pass no flag", which leaves the CLI default.
const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

/// The declared levels, as the picker shows them.
pub(super) fn effort_levels() -> Vec<lofi_types::ThinkingLevel> {
    EFFORT_LEVELS
        .iter()
        .map(|level| match lofi_types::ThinkingLevel::parse(level) {
            Some(level) => level,
            None => lofi_types::ThinkingLevel::Custom((*level).into()),
        })
        .collect()
}

/// `Some("--effort=high")`, or None to leave the CLI default.
///
/// # Errors
/// Returns a configuration error for a level outside `EFFORT_LEVELS`.
fn effort_flag(level: &lofi_types::ThinkingLevel) -> Result<Option<String>> {
    if *level == lofi_types::ThinkingLevel::Off {
        return Ok(None);
    }
    if !effort_levels().contains(level) {
        return Err(Error::Config(format!(
            "thinking level \"{}\" is not supported by Claude Code; allowed: {}",
            level.as_str(),
            EFFORT_LEVELS.join(", ")
        )));
    }
    Ok(Some(format!("--effort={}", level.as_str())))
}

/// The `message.content` of one user frame: a plain string when the turn is
/// text-only, an array of Anthropic blocks when it carries images.
pub(super) fn content_value(text: &str, attachments: &[lofi_types::ContentBlock]) -> Value {
    use base64::Engine as _;
    let images: Vec<&lofi_types::ContentBlock> = attachments
        .iter()
        .filter(|block| matches!(block, lofi_types::ContentBlock::Image { .. }))
        .collect();
    if images.is_empty() {
        return Value::String(text.to_owned());
    }
    let mut blocks = vec![json!({"type": "text", "text": text})];
    for block in images {
        if let lofi_types::ContentBlock::Image { bytes, media_type } = block {
            blocks.push(json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": base64::engine::general_purpose::STANDARD.encode(bytes),
                }
            }));
        }
    }
    Value::Array(blocks)
}

/// The largest auto-compact window the CLI accepts. Lofi owns compaction on
/// this transport: its own hard cap fires well before this, so the child never
/// rewrites a conversation Lofi is tracking.
const NO_CHILD_COMPACTION: &str = "1000000";
/// How long a dead tool bridge waits for the answer the child may already have
/// written. The bridge dies when the child exits, which can follow the result
/// frame by less than one poll: the turn prefers the answer for this long.
const BRIDGE_GRACE: Duration = Duration::from_secs(2);

/// Every flag pins the CLI to the exec bridge: no built-in tools, no user
/// settings, no hooks, and no slash commands.
const FIXED_ARGS: [&str; 22] = [
    "--print",
    "--verbose",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--include-partial-messages",
    "--tools",
    "",
    "--allowedTools",
    EXEC,
    "--permission-mode",
    "dontAsk",
    "--strict-mcp-config",
    "--setting-sources",
    "",
    "--settings",
    "{\"disableAllHooks\":true}",
    "--disable-slash-commands",
    "--no-chrome",
    "--autocompact",
    NO_CHILD_COMPACTION,
];

/// Private inputs of one child process: the system prompt and the bridge
/// configuration. Neither may travel in argv: a single argv entry is limited
/// to about 128 KiB, and every local user can read argv from /proc while the
/// bridge token grants tool execution.
struct PrivateFiles {
    _dir: crate::state::SessionTempDir,
    system_prompt: std::path::PathBuf,
    mcp_config: std::path::PathBuf,
}

fn private_files(root: &Path, system_prompt: &str, mcp: &Value) -> Result<PrivateFiles> {
    if system_prompt.trim().is_empty() {
        return Err(Error::Config("empty Claude Code system prompt".into()));
    }
    let dir = crate::state::create_session_tmp_dir(root)?;
    let prompt = dir.path().join("system-prompt.md");
    std::fs::write(&prompt, system_prompt)?;
    crate::state::ensure_private_file(&prompt)?;
    let mcp_config = dir.path().join("mcp.json");
    let mcp = serde_json::to_vec(mcp).map_err(|e| Error::State(e.to_string()))?;
    std::fs::write(&mcp_config, mcp)?;
    crate::state::ensure_private_file(&mcp_config)?;
    Ok(PrivateFiles {
        _dir: dir,
        system_prompt: prompt,
        mcp_config,
    })
}

/// Everything the CLI child is opened with, apart from the workspace and
/// the tool executor.
pub(super) struct Launch<'a> {
    pub command: &'a str,
    pub model: Option<&'a str>,
    pub effort: ThinkingLevel,
    pub resume: Option<&'a str>,
    /// The prompt the child runs under. A fork carries its own, so a subagent
    /// role prompt reaches a CLI turn the same way it reaches a provider one.
    pub system_prompt: &'a str,
}

#[allow(clippy::struct_excessive_bools)] // transport state flags
pub(super) struct ClaudeSession {
    child: Child,
    group: Option<lofi_code::tools::PgrpKillGuard>,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    bridge: Bridge,
    id: String,
    initialized: bool,
    /// Whether the current turn's prompt reached the CLI's stdin. A frame
    /// over the pipe limit or a dead child leaves it false.
    prompt_written: bool,
    model: Option<String>,
    poisoned: bool,
    round_usage: Option<lofi_types::Usage>,
    /// The usage of the newest API call, held until the next call or the
    /// result frame reports it with the cost the turn has reached.
    unreported_usage: Option<lofi_types::Usage>,
    round_model: Option<String>,
    display_model: Option<String>,
    context_window: Option<u64>,
    /// The CLI's own stop reason for the last assistant frame, mapped to the
    /// same vocabulary a provider reports.
    stop_reason: Option<lofi_types::StopReason>,
    /// Spend of this child process so far, as the CLI reports it. The CLI
    /// counts from zero on every spawn, so the turn cost is a delta.
    reported_cost: f64,
    /// Spend of the current turn: it accumulates across the turn's exchanges
    /// and resets when the turn's first one starts.
    pending_cost: f64,
    /// Whether this turn already reported its usage. A turn that ends before
    /// the CLI sent its result frame reports what the last assistant frame
    /// carried instead.
    reported: bool,
    /// When the open thinking block started, so the banner can report how
    /// long the model reasoned before it answered.
    thinking_start: Option<Instant>,
    /// The prompt and the bridge token live in files; argv is public.
    _files: PrivateFiles,
}

impl ClaudeSession {
    pub async fn open(
        launch: Launch<'_>,
        root: &Path,
        executable: &Path,
        executor: ToolExecutor,
    ) -> Result<Self> {
        let Launch {
            command,
            model,
            effort,
            resume,
            system_prompt,
        } = launch;
        if command.trim().is_empty() {
            return Err(Error::Config("empty Claude Code command".into()));
        }
        let id = match resume {
            Some(id) => uuid::Uuid::parse_str(id)
                .map_err(|_| Error::State("invalid Claude Code session ID".into()))?,
            None => uuid::Uuid::new_v4(),
        }
        .to_string();
        let bridge = Bridge::start(executor, executable).await?;
        let env: serde_json::Map<String, Value> = bridge.config["env"]
            .as_array()
            .ok_or_else(|| Error::State("missing exec bridge environment".into()))?
            .iter()
            .filter_map(|entry| Some((entry["name"].as_str()?.into(), entry["value"].clone())))
            .collect();
        let mcp = json!({"mcpServers":{"lofi":{
            "type":"stdio", "command":bridge.config["command"],
            "args":bridge.config["args"], "env":env
        }}});
        let files = private_files(root, system_prompt, &mcp)?;
        let mut process = Command::new(command);
        process.args(FIXED_ARGS);
        process.arg("--mcp-config").arg(&files.mcp_config);
        process
            .arg("--system-prompt-file")
            .arg(&files.system_prompt);
        // Off selects no effort flag, which leaves the CLI default.
        if let Some(flag) = effort_flag(&effort)? {
            process.arg(flag);
        }
        process.arg(format!(
            "{}={id}",
            if resume.is_some() {
                "--resume"
            } else {
                "--session-id"
            }
        ));
        if let Some(model) = model {
            process.arg(format!("--model={model}"));
        }
        // Deferred tool discovery can add ToolSearch even with no built-in tools.
        let mut child = process
            .env("ENABLE_TOOL_SEARCH", "false")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()?;
        let group = Some(lofi_code::tools::PgrpKillGuard::new(child.id()));
        let input = child
            .stdin
            .take()
            .ok_or_else(|| Error::State("missing Claude Code stdin".into()))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| Error::State("missing Claude Code stdout".into()))?,
        );
        Ok(Self {
            child,
            group,
            input,
            output,
            bridge,
            id,
            initialized: false,
            prompt_written: false,
            model: model.map(str::to_owned),
            poisoned: false,
            round_usage: None,
            unreported_usage: None,
            round_model: None,
            display_model: None,
            context_window: None,
            stop_reason: None,
            reported_cost: 0.0,
            pending_cost: 0.0,
            reported: false,
            thinking_start: None,
            _files: files,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.id
    }

    /// The context window the CLI reported for the current model.
    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    /// The usage of the last assistant frame of the current turn.
    #[allow(dead_code)]
    pub fn round_usage(&self) -> Option<lofi_types::Usage> {
        self.round_usage
    }

    /// The stop reason of the last assistant frame of the current turn.
    pub fn stop_reason(&self) -> Option<lofi_types::StopReason> {
        self.stop_reason
    }

    pub async fn prompt(&mut self, frame: Value, mut context: ToolCallContext) -> Result<String> {
        if self.poisoned {
            return Err(Error::State(
                "backend session disconnected; explicit resume required".into(),
            ));
        }
        self.prompt_written = false;
        self.reported = false;
        // Usage belongs to the turn that reported it: a turn that ends
        // before any frame must not re-report the call before it.
        self.round_usage = None;
        self.unreported_usage = None;
        let cancel = context
            .cancel
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let tool_cancel = Arc::new(AtomicBool::new(false));
        context.cancel = Some(tool_cancel.clone());
        let events = context.events.clone();
        let bridge = self.bridge.task_handle();
        // The watcher reports why it fired. A set flag or a closed consumer is
        // a cancellation; a dead bridge is a transport failure. Reporting both
        // as `Cancelled` settled the turn as a detach and answered the prompt
        // with silence.
        let interrupted = async {
            loop {
                if cancel.load(Ordering::Relaxed)
                    || events
                        .as_ref()
                        .is_some_and(crate::agent::turn_session::EventSink::is_closed)
                {
                    break Error::Cancelled;
                }
                if bridge.is_finished() {
                    break Error::State("Claude Code tool bridge stopped".into());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        // The prompt future holds the mutable borrow of self for as long as it
        // lives, so the select runs in its own scope and drops it before the
        // failure path touches the session again.
        let (result, lost_child) = {
            let prompt = self.prompt_inner(frame, &context);
            tokio::pin!(prompt);
            let mut lost_child = false;
            let result = tokio::select! {
                biased;
                reason = interrupted => match reason {
                    // A set flag is the user's: nothing may wait after it.
                    Error::Cancelled => Err(Error::Cancelled),
                    // A child that writes its result frame and exits leaves a
                    // dead bridge and a finished answer in the same poll
                    // window, and a biased select reads the death first. The
                    // exchange gets a short grace to deliver the answer it
                    // already has, so a completed turn is not reported as a
                    // transport failure.
                    error => match tokio::time::timeout(BRIDGE_GRACE, &mut prompt).await {
                        Ok(answer) => {
                            lost_child = true;
                            // The grace is the only window where the
                            // watcher is not running: a flag set while it
                            // waited still wins over the answer.
                            if cancel.load(Ordering::Relaxed)
                                || events.as_ref().is_some_and(
                                    crate::agent::turn_session::EventSink::is_closed,
                                )
                            {
                                Err(Error::Cancelled)
                            } else {
                                answer
                            }
                        }
                        Err(_) => Err(error),
                    },
                },
                result = tokio::time::timeout(Duration::from_hours(2), &mut prompt) => result
                    .unwrap_or_else(|_| Err(Error::State("Claude Code turn timed out".into()))),
            };
            (result, lost_child)
        };
        tool_cancel.store(true, Ordering::Relaxed);
        // A finished turn serves no tool call: the context holds a clone of the
        // turn's event sender, and a live one would keep the sink open.
        self.bridge.deactivate();
        if lost_child {
            // The answer is the turn's, but the session that wrote it lost its
            // bridge: it can serve no tool call and its child is gone. Giving
            // it up now spares the next turn a write into a dead pipe.
            let _ = self.close().await;
        }
        if result.is_err() {
            // The CLI reports usage once, in its result frame. A turn that
            // ended early still spent the rounds it ran, so it reports what
            // the last assistant frame carried.
            self.report_partial(&context).await;
            self.close().await?;
        }
        result
    }

    async fn report_model(&mut self, value: &Value, context: &ToolCallContext) -> Result<()> {
        let Some(id) = value.as_str().filter(|id| !id.trim().is_empty()) else {
            return Ok(());
        };
        self.display_model.get_or_insert_with(|| id.into());
        if self.round_model.as_deref() != Some(id) {
            self.round_model = Some(id.into());
            self.send_model(context).await?;
        }
        Ok(())
    }

    /// Fold one partial-stream event into the round state and forward the
    /// deltas the UI renders.
    async fn handle_stream_event(
        &mut self,
        event: &Value,
        context: &ToolCallContext,
    ) -> Result<()> {
        if event["type"] == "message_start" {
            self.round_usage = parse_usage(&event["message"]["usage"]);
            self.report_model(&event["message"]["model"], context)
                .await?;
        } else if event["type"] == "message_delta" {
            if let Some(usage) = &mut self.round_usage {
                if let Some(output) = event["usage"]["output_tokens"].as_u64() {
                    usage.output_tokens = output;
                }
            }
            if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                self.stop_reason = Some(stop_reason(reason));
            }
        }
        if event["type"] == "content_block_start" && event["content_block"]["type"] == "thinking" {
            self.thinking_start = Some(Instant::now());
        } else if event["type"] == "content_block_stop" {
            self.close_thinking(context).await?;
        }
        if event["type"] == "content_block_delta" {
            let delta = &event["delta"];
            let update = match delta["type"].as_str() {
                Some("text_delta") => delta["text"]
                    .as_str()
                    .map(|text| AgentEvent::Text(text.into())),
                Some("thinking_delta") => delta["thinking"]
                    .as_str()
                    .map(|text| AgentEvent::Thinking(text.into())),
                _ => None,
            };
            if let (Some(events), Some(update)) = (context.events.as_ref(), update) {
                events.forward(update).await?;
            }
        }
        Ok(())
    }

    /// Close the open thinking block, if any, with the time it took. The
    /// provider path reports the same boundary, so a resume reconstructs the
    /// same thinking durations on either transport.
    async fn close_thinking(&mut self, context: &ToolCallContext) -> Result<()> {
        let Some(start) = self.thinking_start.take() else {
            return Ok(());
        };

        if let Some(events) = &context.events {
            events
                .forward(AgentEvent::ThinkingEnd {
                    elapsed_ms: start.elapsed().as_millis() as u64,
                })
                .await?;
        }
        Ok(())
    }

    async fn send_model(&self, context: &ToolCallContext) -> Result<()> {
        if let (Some(events), Some(id), Some(display)) =
            (&context.events, &self.round_model, &self.display_model)
        {
            events
                .forward(AgentEvent::ModelReported {
                    id: id.clone(),
                    display: display.clone(),
                })
                .await?;
        }
        Ok(())
    }

    async fn prompt_inner(&mut self, frame: Value, context: &ToolCallContext) -> Result<String> {
        self.round_usage = None;
        self.stop_reason = None;
        self.send_model(context).await?;
        if self.initialized {
            self.bridge.activate(context.clone());
        }
        rpc::write(
            &mut self.input,
            &json!({"type":"user","session_id":self.id,
            "parent_tool_use_id":null,"message":{"role":"user","content":frame}}),
        )
        .await?;
        self.prompt_written = true;
        loop {
            let message = if self.initialized {
                rpc::read_json(&mut self.output).await?
            } else {
                tokio::time::timeout(Duration::from_secs(30), rpc::read_json(&mut self.output))
                    .await
                    .map_err(|_| Error::State("Claude Code initialization timed out".into()))??
            };
            if let Some(id) = message["session_id"].as_str() {
                if id != self.id {
                    return Err(Error::State("Claude Code changed session identity".into()));
                }
            }
            match message["type"].as_str() {
                Some("system") if message["subtype"] == "init" => {
                    validate_init(&message)?;
                    self.display_model = message["model"]
                        .as_str()
                        .filter(|id| !id.trim().is_empty())
                        .map(str::to_owned);
                    self.report_model(&message["model"], context).await?;
                    self.initialized = true;
                    self.bridge.activate(context.clone());
                }
                Some("result") => {
                    self.close_thinking(context).await?;
                    self.report_result(&message, context).await?;
                    if message["is_error"] != false || message["subtype"] != "success" {
                        return Err(Error::State(
                            "Claude Code could not complete the turn; check `claude auth status` and the saved session. Use `claude auth login` if not logged in.".into(),
                        ));
                    }
                    if !self.initialized {
                        return Err(Error::State(
                            "Claude Code returned a result before initialization".into(),
                        ));
                    }
                    return Ok("end_turn".into());
                }
                Some("stream_event") if self.initialized => {
                    self.handle_stream_event(&message["event"], context).await?;
                }
                Some("assistant")
                    if self.initialized && message["parent_tool_use_id"].is_null() =>
                {
                    self.report_model(&message["message"]["model"], context)
                        .await?;
                    if let Some(usage) = parse_usage(&message["message"]["usage"]) {
                        self.round_usage = Some(usage);
                        self.report_call(usage, context).await;
                    }
                    if let Some(reason) = message["message"]["stop_reason"].as_str() {
                        self.stop_reason = Some(stop_reason(reason));
                    }
                }
                // Complete assistant messages repeat the partial stream. Tool
                // results are recorded by Lofi's executor, never by the CLI.
                _ => {}
            }
        }
    }

    /// Hold the usage of one API call until the call after it arrives.
    ///
    /// A turn runs one call per tool cycle, and both the session totals and the
    /// subagent budget need every one of them. The CLI prices a turn only in
    /// its result frame, though, and a consumer keeps the last event of a turn,
    /// so the newest call's usage waits: the next call, or the result frame,
    /// reports it with the cost that had arrived by then.
    async fn report_call(&mut self, usage: lofi_types::Usage, context: &ToolCallContext) {
        let Some(previous) = self.unreported_usage.replace(usage) else {
            return;
        };
        if let Some(events) = &context.events {
            let _ = events
                .emit(AgentEvent::RoundUsage {
                    cost: self.pending_cost,
                    usage: previous,
                })
                .await;
        }
    }

    /// Report the call whose usage no later call superseded, now that the turn
    /// has its final cost.
    async fn report_last_call(&mut self, message: Option<&Value>, context: &ToolCallContext) {
        // The newest frame wins, then the result frame's own usage, then
        // whatever the last frame reported at all: a turn cancelled mid-call
        // has no completed call to hold, and its spend must still reach the
        // session totals.
        let usage = self
            .unreported_usage
            .take()
            .or_else(|| message.and_then(|message| parse_usage(&message["usage"])))
            .or(self.round_usage);
        let Some(usage) = usage else {
            return;
        };
        if let Some(events) = &context.events {
            let _ = events
                .emit(AgentEvent::RoundUsage {
                    cost: self.pending_cost,
                    usage,
                })
                .await;
        }
    }

    /// Report the usage of a turn that ended before the CLI sent its result
    /// frame. The cost stays zero: the CLI only prices a finished turn.
    async fn report_partial(&mut self, context: &ToolCallContext) {
        if self.reported {
            return;
        }
        self.reported = true;
        self.report_last_call(None, context).await;
        let Some(usage) = self.round_usage else {
            return;
        };
        if let Some(events) = &context.events {
            let _ = events
                .emit(AgentEvent::ContextReported {
                    context: Some(lofi_types::ContextUsage {
                        used: usage.context_tokens(),
                        size: self.context_window,
                    }),
                })
                .await;
        }
    }

    async fn report_result(&mut self, message: &Value, context: &ToolCallContext) -> Result<()> {
        self.reported = true;
        let models = message["modelUsage"].as_object();
        let total = message["total_cost_usd"]
            .as_f64()
            .filter(|cost| cost.is_finite() && *cost >= 0.0);
        if let Some(total) = total {
            let delta = if total >= self.reported_cost {
                total - self.reported_cost
            } else {
                total
            };
            self.reported_cost = total;
            self.pending_cost += delta;
        }
        self.context_window = models
            .and_then(|models| {
                self.round_model
                    .as_ref()
                    .and_then(|model| models.get(model))
                    .or_else(|| {
                        (models.len() == 1)
                            .then(|| models.values().next())
                            .flatten()
                    })
            })
            .and_then(|usage| usage["contextWindow"].as_u64().filter(|size| *size > 0));
        // The trigger the UI's compaction policy reads, on the same cadence a
        // provider round reports it: one event at turn end.
        self.report_last_call(Some(message), context).await;
        if let Some(events) = &context.events {
            events
                .forward(AgentEvent::ContextReported {
                    context: self.round_usage.map(|usage| lofi_types::ContextUsage {
                        used: usage.context_tokens(),
                        size: self.context_window,
                    }),
                })
                .await?;
        }
        Ok(())
    }

    /// Whether a cancelled or failed turn left the child unusable.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.poisoned
    }

    /// Whether the CLI reported its init message. Only then does the session
    /// id Lofi saved exist on the CLI side.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Whether the current turn's prompt reached the CLI's stdin.
    #[must_use]
    pub fn prompt_written(&self) -> bool {
        self.prompt_written
    }

    /// Take the spend of the turn that just ended.
    pub fn take_cost(&mut self) -> f64 {
        std::mem::take(&mut self.pending_cost)
    }

    pub async fn set_model(&mut self, model: Option<&str>) -> Result<()> {
        if self.poisoned {
            return Err(Error::State(
                "backend session disconnected; explicit resume required".into(),
            ));
        }
        if self.model.as_deref() == model {
            return Ok(());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            rpc::write(
                &mut self.input,
                &json!({"type":"control_request",
                "request_id":id,"request":{"subtype":"set_model","model":model}}),
            )
            .await?;
            loop {
                let message = rpc::read_json(&mut self.output).await?;
                if message["type"] == "control_response" && message["response"]["request_id"] == id
                {
                    if message["response"]["subtype"] != "success" {
                        return Err(Error::State("Claude Code rejected the model change".into()));
                    }
                    return Ok(());
                }
            }
        })
        .await
        .unwrap_or_else(|_| Err(Error::State("Claude Code model change timed out".into())));
        if result.is_err() {
            self.close().await?;
        }
        result?;
        self.model = model.map(str::to_owned);
        self.round_model = None;
        self.display_model = None;
        self.context_window = None;
        Ok(())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.bridge.deactivate();
        self.poisoned = true;
        drop(self.group.take());
        self.child.wait().await?;
        Ok(())
    }
}

fn stop_reason(reason: &str) -> lofi_types::StopReason {
    match reason {
        "end_turn" | "stop_sequence" => lofi_types::StopReason::EndTurn,
        "max_tokens" => lofi_types::StopReason::MaxTokens,
        "tool_use" => lofi_types::StopReason::ToolUse,
        _ => lofi_types::StopReason::Other,
    }
}

fn parse_usage(value: &Value) -> Option<lofi_types::Usage> {
    Some(lofi_types::Usage {
        input_tokens: value["input_tokens"].as_u64()?,
        output_tokens: value["output_tokens"].as_u64()?,
        cache_read_tokens: value["cache_read_input_tokens"].as_u64().unwrap_or(0),
        cache_write_tokens: value["cache_creation_input_tokens"].as_u64().unwrap_or(0),
    })
}

fn validate_init(message: &Value) -> Result<()> {
    if message["tools"] != json!([EXEC]) {
        return Err(Error::State(
            "Claude Code must expose only mcp__lofi__exec".into(),
        ));
    }
    let servers = message["mcp_servers"].as_array();
    if !servers.is_some_and(|servers| {
        servers.len() == 1 && servers[0]["name"] == "lofi" && servers[0]["status"] == "connected"
    }) {
        return Err(Error::State(
            "Claude Code must connect only to the Lofi MCP server".into(),
        ));
    }
    Ok(())
}
