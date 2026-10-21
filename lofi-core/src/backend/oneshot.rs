use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::Command;

use lofi_error::{Error, Result};
use lofi_types::ThinkingLevel;

use super::contract::{Completion, CompletionRequest};
use super::rpc;

/// Ceiling on the one-shot exchange. A healthy evaluator answers in seconds,
/// but the caller gates a tool call on it: a child that hangs on auth or the
/// network must fail the call instead of blocking the turn that waits for the
/// approval.
const EVALUATOR_TIMEOUT: Duration = Duration::from_mins(2);

/// Answer one prompt with text only.
///
/// The child runs without the exec bridge, without tools and without a saved
/// session: it cannot touch the workspace, it shares nothing with a running
/// conversation, and it dies with the call. That is what makes the shell
/// evaluator safe to run on the same backend as the turn it is judging.
pub(super) async fn complete(
    command: &str,
    root: &Path,
    model: Option<&str>,
    effort: &ThinkingLevel,
    request: &CompletionRequest,
) -> Result<Completion> {
    if command.trim().is_empty() {
        return Err(Error::Config("empty Claude Code command".into()));
    }
    if request.system.trim().is_empty() {
        return Err(Error::Config("empty Claude Code system prompt".into()));
    }
    let dir = crate::state::create_session_tmp_dir(root)?;
    let prompt_file = dir.path().join("system-prompt.md");
    std::fs::write(&prompt_file, &request.system)?;
    crate::state::ensure_private_file(&prompt_file)?;
    let id = uuid::Uuid::new_v4().to_string();
    let mut process = Command::new(command);
    process.args([
        "--print",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--tools",
        "",
        "--permission-mode",
        "dontAsk",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--settings",
        r#"{"disableAllHooks":true}"#,
        "--disable-slash-commands",
        "--no-chrome",
    ]);
    process
        .arg("--system-prompt-file")
        .arg(&prompt_file)
        .arg(format!("--session-id={id}"));
    if *effort != ThinkingLevel::Off {
        process.arg(format!("--effort={}", effort.as_str()));
    }
    if let Some(model) = model {
        process.arg(format!("--model={model}"));
    }
    // The CLI has no flag for the output cap, only the environment variable;
    // without it a caller's budget would be silently unbounded here while the
    // provider path honors it.
    if let Some(max) = request.max_tokens {
        process.env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", max.to_string());
    }
    let mut child = process
        .env("ENABLE_TOOL_SEARCH", "false")
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| Error::State("missing Claude Code stdin".into()))?;
    let mut output = BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| Error::State("missing Claude Code stdout".into()))?,
    );
    // The ceiling covers the whole exchange: a child that hangs before it
    // reads stdin blocks the write just as surely as one that never answers.
    let exchange = async {
        rpc::write(
            &mut input,
            &json!({"type":"user","session_id":id,
                "parent_tool_use_id":null,"message":{"role":"user","content":request.prompt}}),
        )
        .await?;
        let _ = input.flush().await;
        collect(&mut output).await
    };
    let text = match tokio::time::timeout(EVALUATOR_TIMEOUT, exchange).await {
        Ok(text) => text,
        Err(_) => Err(Error::State("Claude Code evaluator timed out".into())),
    };
    child.start_kill().ok();
    let _ = child.wait().await;
    text
}

async fn collect(output: &mut BufReader<tokio::process::ChildStdout>) -> Result<Completion> {
    let mut text = String::new();
    loop {
        let message = rpc::read_json(output).await?;
        match message["type"].as_str() {
            Some("assistant") => {
                let content = &message["message"]["content"];
                if let Some(blocks) = content.as_array() {
                    for block in blocks {
                        if block["type"] == "text" {
                            if let Some(delta) = block["text"].as_str() {
                                text.push_str(delta);
                            }
                        }
                        // A tool-free child cannot answer by calling a tool. A
                        // tool request means the isolation flags did not hold.
                        if block["type"] == "tool_use" {
                            return Err(Error::State(
                                "the evaluator requested a tool; it must run without tools".into(),
                            ));
                        }
                    }
                }
            }
            Some("result") => {
                if message["is_error"] == true {
                    let detail = message["result"]
                        .as_str()
                        .unwrap_or("Claude Code reported an error");
                    return Err(Error::State(detail.to_string()));
                }
                if text.trim().is_empty() {
                    if let Some(result) = message["result"].as_str() {
                        text.push_str(result);
                    }
                }
                return Ok(Completion {
                    text,
                    usage: usage_of(&message["usage"]),
                    cost: cost_of(&message),
                });
            }
            _ => {}
        }
    }
}

fn usage_of(value: &Value) -> lofi_types::Usage {
    let number = |key: &str| value[key].as_u64().unwrap_or(0);
    lofi_types::Usage {
        input_tokens: number("input_tokens"),
        output_tokens: number("output_tokens"),
        cache_read_tokens: number("cache_read_input_tokens"),
        cache_write_tokens: number("cache_creation_input_tokens"),
    }
}

fn cost_of(message: &Value) -> Option<f64> {
    message["total_cost_usd"]
        .as_f64()
        .filter(|cost| cost.is_finite() && *cost >= 0.0)
}
