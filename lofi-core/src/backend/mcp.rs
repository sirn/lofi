use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use base64::Engine;
use lofi_error::{Error, Result};
use lofi_types::ContentBlock;
use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};

use super::rpc;
use crate::agent::{ToolCallContext, ToolExecutor};

pub(super) struct Bridge {
    pub config: Value,
    active: Arc<Mutex<Option<ToolCallContext>>>,
    task: tokio::task::JoinHandle<Result<()>>,
}

impl Bridge {
    pub async fn start(executor: ToolExecutor, executable: &std::path::Path) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let token = uuid::Uuid::new_v4().to_string();
        let config = json!({"name":"lofi", "command":executable, "args":["--exec-mcp"],
            "env":[{"name":"LOFI_EXEC_ADDRESS","value":address.to_string()}, {"name":"LOFI_EXEC_TOKEN","value":token}]});
        let active = Arc::new(Mutex::new(None));
        let state = active.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let hello =
                tokio::time::timeout(std::time::Duration::from_secs(10), rpc::read(&mut reader))
                    .await
                    .map_err(|_| Error::State("exec bridge handshake timed out".into()))??;
            if hello["method"] != "_lofi/connect" || hello["params"]["token"] != token {
                return Err(Error::State("exec bridge authentication failed".into()));
            }
            serve(&mut reader, &mut writer, executor, state).await
        });
        Ok(Self {
            config,
            active,
            task,
        })
    }

    pub fn activate(&self, context: ToolCallContext) {
        *self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(context);
    }

    pub fn deactivate(&self) {
        if let Some(context) = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            drop(context);
        }
    }

    pub fn task_handle(&self) -> tokio::task::AbortHandle {
        self.task.abort_handle()
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.deactivate();
        self.task.abort();
    }
}

async fn serve(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    executor: ToolExecutor,
    active: Arc<Mutex<Option<ToolCallContext>>>,
) -> Result<()> {
    let mut initialized = false;
    let mut ready = false;
    let mut seen = HashSet::new();
    loop {
        let request = rpc::read(reader).await?;
        let method = request["method"].as_str().unwrap_or_default();
        let Some(id) = request.get("id") else {
            if method == "notifications/initialized" && initialized {
                ready = true;
            }
            continue;
        };
        if !(id.is_string() || id.is_number()) || seen.len() >= 4096 || !seen.insert(id.to_string())
        {
            return Err(Error::State(
                "duplicate or excessive MCP request IDs".into(),
            ));
        }
        let result = match method {
            "initialize" if !initialized => {
                initialized = true;
                json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},
                    "serverInfo":{"name":"lofi-exec","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            "tools/list" if ready => json!({"tools":[{"name":lofi_code::EXEC_TOOL_NAME,
                "description":lofi_code::EXEC_TOOL_DESCRIPTION, "inputSchema":lofi_code::exec_tool_input_schema()}]}),
            "tools/call" if ready => {
                let params = &request["params"];
                if params["name"] != "exec" || !params["arguments"]["code"].is_string() {
                    rpc::write(
                        writer,
                        &rpc::error(id, -32602, "expected exec with a code string"),
                    )
                    .await?;
                    continue;
                }
                let context = active
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                let Some(context) = context else {
                    rpc::write(writer, &rpc::error(id, -32600, "no active Lofi turn")).await?;
                    continue;
                };
                let call_id = format!("exec-{}", uuid::Uuid::new_v4());
                let block = executor
                    .execute(&call_id, &params["arguments"], context)
                    .await?;
                tool_result(block)?
            }
            _ => {
                rpc::write(writer, &rpc::error(id, -32601, "MCP method unavailable")).await?;
                continue;
            }
        };
        rpc::write(writer, &json!({"jsonrpc":"2.0","id":id,"result":result})).await?;
    }
}

fn tool_result(block: ContentBlock) -> Result<Value> {
    let ContentBlock::ToolResult {
        content,
        is_error,
        images,
        ..
    } = block
    else {
        return Err(Error::State("exec returned an invalid tool result".into()));
    };
    let mut blocks = vec![json!({"type":"text","text":content})];
    for image in images {
        blocks.push(json!({"type":"image", "mimeType":image.media_type,
            "data":base64::engine::general_purpose::STANDARD.encode(image.bytes)}));
    }
    Ok(json!({"content":blocks,"isError":is_error}))
}

/// Relay MCP stdio to the running Lofi session. This process never owns tools.
/// # Errors
/// Returns errors for missing bridge credentials, invalid addresses or broken pipes.
pub async fn run_stdio_proxy() -> Result<()> {
    let address: std::net::SocketAddr = std::env::var("LOFI_EXEC_ADDRESS")
        .map_err(|_| Error::Config("missing exec bridge address".into()))?
        .parse()
        .map_err(|_| Error::Config("invalid exec bridge address".into()))?;
    if !address.ip().is_loopback() {
        return Err(Error::Config("exec bridge must be local".into()));
    }
    let token = std::env::var("LOFI_EXEC_TOKEN")
        .map_err(|_| Error::Config("missing exec bridge token".into()))?;
    let mut stream = TcpStream::connect(address).await?;
    rpc::write(
        &mut stream,
        &json!({"jsonrpc":"2.0","method":"_lofi/connect","params":{"token":token}}),
    )
    .await?;
    let (mut reader, mut writer) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = tokio::io::copy(&mut stdin, &mut writer) => { result?; }
        result = tokio::io::copy(&mut reader, &mut stdout) => { result?; }
    }
    Ok(())
}
