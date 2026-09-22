use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lofi_error::{Error, Result};
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use super::{mcp::Bridge, rpc};
use crate::agent::{AgentEvent, ToolCallContext, ToolExecutor};

const EXEC: &str = "mcp__lofi__exec";

pub(super) struct ClaudeSession {
    child: Child,
    group: Option<lofi_code::tools::PgrpKillGuard>,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    bridge: Bridge,
    id: String,
    initialized: bool,
    model: Option<String>,
    poisoned: bool,
    round_usage: Option<lofi_types::Usage>,
    round_model: Option<String>,
    display_model: Option<String>,
    context_window: Option<u64>,
}

impl ClaudeSession {
    pub async fn open(
        command: &str,
        model: Option<&str>,
        root: &Path,
        executable: &Path,
        executor: ToolExecutor,
        resume: Option<&str>,
    ) -> Result<Self> {
        if command.trim().is_empty() {
            return Err(Error::Config("empty Claude Code command".into()));
        }
        let id = match resume {
            Some(id) => uuid::Uuid::parse_str(id)
                .map_err(|_| Error::State("invalid Claude Code session ID".into()))?,
            None => uuid::Uuid::new_v4(),
        }
        .to_string();
        let system_prompt = executor.system_prompt();
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
        let mut process = Command::new(command);
        process.args([
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
            "--mcp-config",
            &mcp.to_string(),
            "--setting-sources",
            "",
            "--settings",
            "{\"disableAllHooks\":true}",
            "--disable-slash-commands",
            "--no-chrome",
            "--system-prompt",
            &system_prompt,
        ]);
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
            model: model.map(str::to_owned),
            poisoned: false,
            round_usage: None,
            round_model: None,
            display_model: None,
            context_window: None,
        })
    }

    pub fn session_id(&self) -> &str {
        &self.id
    }

    pub async fn prompt(&mut self, text: &str, mut context: ToolCallContext) -> Result<String> {
        if self.poisoned {
            return Err(Error::State(
                "backend session disconnected; explicit resume required".into(),
            ));
        }
        let cancel = context
            .cancel
            .clone()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let tool_cancel = Arc::new(AtomicBool::new(false));
        context.cancel = Some(tool_cancel.clone());
        let events = context.events.clone();
        let bridge = self.bridge.task_handle();
        let interrupted = async {
            loop {
                if cancel.load(Ordering::Relaxed)
                    || events
                        .as_ref()
                        .is_some_and(tokio::sync::mpsc::Sender::is_closed)
                    || bridge.is_finished()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        let result = tokio::select! {
            biased;
            () = interrupted => Err(Error::Cancelled),
            result = tokio::time::timeout(Duration::from_hours(2), self.prompt_inner(text, &context)) => {
                result.unwrap_or_else(|_| Err(Error::State("Claude Code turn timed out".into())))
            }
        };
        tool_cancel.store(true, Ordering::Relaxed);
        self.bridge.deactivate();
        if result.is_err() {
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

    async fn send_model(&self, context: &ToolCallContext) -> Result<()> {
        if let (Some(tx), Some(id), Some(display)) =
            (&context.events, &self.round_model, &self.display_model)
        {
            tx.send(AgentEvent::BackendModel {
                id: id.clone(),
                display: display.clone(),
            })
            .await
            .map_err(|_| Error::Cancelled)?;
        }
        Ok(())
    }

    async fn prompt_inner(&mut self, text: &str, context: &ToolCallContext) -> Result<String> {
        self.round_usage = None;
        self.send_model(context).await?;
        if self.initialized {
            self.bridge.activate(context.clone());
        }
        rpc::write(
            &mut self.input,
            &json!({"type":"user","session_id":self.id,
            "parent_tool_use_id":null,"message":{"role":"user","content":text}}),
        )
        .await?;
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
                    let event = &message["event"];
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
                        if let (Some(tx), Some(update)) = (context.events.as_ref(), update) {
                            tx.send(update).await.map_err(|_| Error::Cancelled)?;
                        }
                    }
                }
                Some("assistant")
                    if self.initialized && message["parent_tool_use_id"].is_null() =>
                {
                    self.report_model(&message["message"]["model"], context)
                        .await?;
                    if let Some(usage) = parse_usage(&message["message"]["usage"]) {
                        self.round_usage = Some(usage);
                    }
                }
                // Complete assistant messages repeat the partial stream. Tool
                // results are recorded by Lofi's executor, never by the CLI.
                _ => {}
            }
        }
    }

    async fn report_result(&mut self, message: &Value, context: &ToolCallContext) -> Result<()> {
        let models = message["modelUsage"].as_object();
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
        if let Some(tx) = &context.events {
            tx.send(AgentEvent::BackendUsage {
                context: self.round_usage.map(|usage| lofi_types::ContextUsage {
                    used: usage.context_tokens(),
                    size: self.context_window,
                }),
                usage: parse_usage(&message["usage"]),
                cost: message["total_cost_usd"]
                    .as_f64()
                    .filter(|cost| cost.is_finite() && *cost >= 0.0),
            })
            .await
            .map_err(|_| Error::Cancelled)?;
        }
        Ok(())
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
