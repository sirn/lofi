use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};

pub const WAIT: Duration = Duration::from_secs(15);

#[derive(Clone, Debug)]
pub struct MockRequest {
    pub path: String,
    pub headers: String,
    pub body: String,
}

#[derive(Clone, Debug)]
pub struct MockResponse {
    status: u16,
    content_type: &'static str,
    body: String,
    delay: Duration,
    chunks: Option<Vec<Vec<u8>>>,
    chunk_delay: Duration,
    content_length: Option<usize>,
    headers: Vec<(String, String)>,
}

impl MockResponse {
    pub fn sse(body: String) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body,
            delay: Duration::ZERO,
            chunks: None,
            chunk_delay: Duration::ZERO,
            content_length: None,
            headers: Vec::new(),
        }
    }

    pub fn delayed_sse(body: String, delay: Duration) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream",
            body,
            delay,
            chunks: None,
            chunk_delay: Duration::ZERO,
            content_length: None,
            headers: Vec::new(),
        }
    }

    pub fn json(body: &Value) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            body: body.to_string(),
            delay: Duration::ZERO,
            chunks: None,
            chunk_delay: Duration::ZERO,
            content_length: None,
            headers: Vec::new(),
        }
    }

    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: json!({ "error": { "message": message } }).to_string(),
            delay: Duration::ZERO,
            chunks: None,
            chunk_delay: Duration::ZERO,
            content_length: None,
            headers: Vec::new(),
        }
    }

    pub fn fragmented_sse(body: String, split_at: &[usize], delay: Duration) -> Self {
        let bytes = body.as_bytes();
        let mut chunks = Vec::new();
        let mut start = 0;
        for end in split_at
            .iter()
            .copied()
            .filter(|end| *end > 0 && *end < bytes.len())
        {
            chunks.push(bytes[start..end].to_vec());
            start = end;
        }
        chunks.push(bytes[start..].to_vec());
        let mut response = Self::sse(body);
        response.chunks = Some(chunks);
        response.chunk_delay = delay;
        response
    }

    pub fn truncated_sse(body: String, declared_extra_bytes: usize) -> Self {
        let mut response = Self::sse(body);
        response.content_length = Some(response.body.len() + declared_extra_bytes);
        response
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }
}

pub struct MockServer {
    addr: SocketAddr,
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    pub fn start(responses: Vec<MockResponse>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_responses = Arc::clone(&responses);
        let thread_requests = Arc::clone(&requests);
        let thread_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        if thread_stop.load(Ordering::Relaxed) {
                            break;
                        }
                        thread_requests
                            .lock()
                            .unwrap()
                            .push(read_request(&mut stream));
                        let response = thread_responses
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| MockResponse::error(500, "unexpected mock request"));
                        write_response(&mut stream, &response);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            addr,
            responses,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn requests(&self) -> Vec<MockRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn push(&self, response: MockResponse) {
        self.responses.lock().unwrap().push_back(response);
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.addr);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn read_request(stream: &mut TcpStream) -> MockRequest {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let mut body_start = None;
    let mut content_length = 0;
    loop {
        let read = stream.read(&mut chunk).unwrap_or(0);
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        if body_start.is_none() {
            body_start = bytes
                .windows(4)
                .position(|part| part == b"\r\n\r\n")
                .map(|index| index + 4);
            if let Some(start) = body_start {
                let headers = String::from_utf8_lossy(&bytes[..start]);
                content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
            }
        }
        if body_start.is_some_and(|start| bytes.len() >= start + content_length) {
            break;
        }
    }
    let start = body_start.unwrap_or(bytes.len());
    let headers = String::from_utf8_lossy(&bytes[..start]);
    let path = headers
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_string();
    MockRequest {
        path,
        headers: headers.into_owned(),
        body: String::from_utf8_lossy(&bytes[start..]).into_owned(),
    }
}

fn write_response(stream: &mut TcpStream, response: &MockResponse) {
    thread::sleep(response.delay);
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let content_length = response.content_length.unwrap_or(response.body.len());
    if write!(
        stream,
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status, response.content_type, content_length,
    )
    .is_ok()
    {
        for (name, value) in &response.headers {
            let _ = write!(stream, "{name}: {value}\r\n");
        }
        let _ = stream.write_all(b"\r\n");
        if let Some(chunks) = &response.chunks {
            for chunk in chunks {
                if stream.write_all(chunk).is_err() {
                    break;
                }
                let _ = stream.flush();
                thread::sleep(response.chunk_delay);
            }
        } else {
            let _ = stream.write_all(response.body.as_bytes());
        }
        let _ = stream.flush();
    }
}

pub fn delayed_tool_response(call_id: &str, code: &str, delay: Duration) -> MockResponse {
    let mut response = tool_response(call_id, code);
    response.delay = delay;
    response
}

pub fn tool_response(call_id: &str, code: &str) -> MockResponse {
    let arguments = json!({ "code": code }).to_string();
    let event = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": "exec", "arguments": arguments }
                }]
            }
        }]
    });
    MockResponse::sse(format!(
        "data: {event}

data: [DONE]

"
    ))
}

pub fn tool_response_with_usage(call_id: &str, code: &str, input_tokens: u64) -> MockResponse {
    let arguments = json!({ "code": code }).to_string();
    let tool = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": "exec", "arguments": arguments }
                }]
            }
        }]
    });
    let usage = json!({
        "choices": [],
        "usage": { "prompt_tokens": input_tokens, "completion_tokens": 3 },
    });
    MockResponse::sse(format!("data: {tool}\n\ndata: {usage}\n\ndata: [DONE]\n\n"))
}

pub fn fragmented_tool_response(call_id: &str, code: &str) -> MockResponse {
    let arguments = json!({ "code": code }).to_string();
    let split = arguments.len() / 2;
    let (first, second) = arguments.split_at(split);
    let start = json!({
        "choices": [{ "delta": { "tool_calls": [{
            "index": 0,
            "id": call_id,
            "type": "function",
            "function": { "name": "exec", "arguments": first }
        }] } }]
    });
    let end = json!({
        "choices": [{ "delta": { "tool_calls": [{
            "index": 0,
            "function": { "arguments": second }
        }] } }]
    });
    MockResponse::sse(format!(
        "data: {start}

data: {end}

data: [DONE]

"
    ))
}

pub fn parallel_tool_response(calls: &[(&str, &str)]) -> MockResponse {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, code))| {
            json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {
                    "name": "exec",
                    "arguments": json!({ "code": code }).to_string(),
                }
            })
        })
        .collect();
    let event = json!({ "choices": [{ "delta": { "tool_calls": calls } }] });
    MockResponse::sse(format!(
        "data: {event}

data: [DONE]

"
    ))
}

pub fn responses_tool_response(call_id: &str, code: &str) -> MockResponse {
    let item_id = format!("item-{call_id}");
    let arguments = json!({ "code": code }).to_string();
    let split = arguments.len() / 2;
    let (first_delta, second_delta) = arguments.split_at(split);
    let added = json!({
        "type": "response.output_item.added",
        "item": {
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": "exec",
            "arguments": "",
        },
    });
    let first = json!({
        "type": "response.function_call_arguments.delta",
        "item_id": item_id,
        "delta": first_delta,
    });
    let second = json!({
        "type": "response.function_call_arguments.delta",
        "item_id": item_id,
        "delta": second_delta,
    });
    let done = json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": "exec",
            "arguments": arguments,
        },
    });
    let completed = json!({
        "type": "response.completed",
        "response": { "usage": { "input_tokens": 5, "output_tokens": 3 } },
    });
    MockResponse::sse(format!(
        "data: {added}

data: {first}

data: {second}

data: {done}

data: {completed}

data: [DONE]

"
    ))
}

pub fn anthropic_tool_response(call_id: &str, code: &str) -> MockResponse {
    let arguments = json!({ "code": code }).to_string();
    let split = arguments.len() / 2;
    let (first_delta, second_delta) = arguments.split_at(split);
    let start = json!({
        "index": 0,
        "content_block": { "type": "tool_use", "id": call_id, "name": "exec" },
    });
    let first = json!({
        "index": 0,
        "delta": { "type": "input_json_delta", "partial_json": first_delta },
    });
    let second = json!({
        "index": 0,
        "delta": { "type": "input_json_delta", "partial_json": second_delta },
    });
    let stop = json!({ "index": 0 });
    MockResponse::sse(format!(
        "event: content_block_start
data: {start}

event: content_block_delta
data: {first}

event: content_block_delta
data: {second}

event: content_block_stop
data: {stop}

event: message_stop
data: {{}}

"
    ))
}

pub fn anthropic_text_response(text: &str) -> MockResponse {
    let delta = json!({
        "index": 0,
        "delta": { "type": "text_delta", "text": text },
    });
    MockResponse::sse(format!(
        "event: content_block_delta
data: {delta}

event: message_stop
data: {{}}

"
    ))
}

pub fn anthropic_usage_response(thinking: &str, signature: &str, text: &str) -> MockResponse {
    let start = json!({
        "message": {
            "usage": {
                "input_tokens": 11,
                "cache_read_input_tokens": 3,
                "cache_creation_input_tokens": 2,
            },
        },
    });
    let thinking = json!({
        "index": 0,
        "delta": { "type": "thinking_delta", "thinking": thinking },
    });
    let signature = json!({
        "index": 0,
        "delta": { "type": "signature_delta", "signature": signature },
    });
    let text = json!({
        "index": 1,
        "delta": { "type": "text_delta", "text": text },
    });
    let usage = json!({ "usage": { "output_tokens": 7 } });
    MockResponse::sse(format!(
        "event: message_start\ndata: {start}\n\nevent: content_block_delta\ndata: {thinking}\n\nevent: content_block_delta\ndata: {signature}\n\nevent: content_block_delta\ndata: {text}\n\nevent: message_delta\ndata: {usage}\n\nevent: message_stop\ndata: {{}}\n\n"
    ))
}

pub fn google_tool_response(call_id: &str, code: &str) -> MockResponse {
    let event = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [{
                    "functionCall": {
                        "id": call_id,
                        "name": "exec",
                        "args": { "code": code },
                    },
                    "thoughtSignature": "Z29vZ2xlLXNpZ25hdHVyZS1tYXJrZXI=",
                }]
            },
            "finishReason": "STOP",
        }]
    });
    MockResponse::sse(format!(
        "data: {event}

"
    ))
}

pub fn google_text_response(text: &str) -> MockResponse {
    let event = json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": text }] },
            "finishReason": "STOP",
        }]
    });
    MockResponse::sse(format!(
        "data: {event}

"
    ))
}

pub fn google_thinking_usage_response(thinking: &str, text: &str) -> MockResponse {
    let event = json!({
        "candidates": [{
            "content": {
                "role": "model",
                "parts": [
                    {
                        "text": thinking,
                        "thought": true,
                        "thoughtSignature": "Z29vZ2xlLXRob3VnaHQtc2lnbmF0dXJl",
                    },
                    { "text": text },
                ],
            },
            "finishReason": "STOP",
        }],
        "usageMetadata": {
            "promptTokenCount": 13,
            "cachedContentTokenCount": 4,
            "candidatesTokenCount": 5,
            "thoughtsTokenCount": 3,
        },
    });
    MockResponse::sse(format!("data: {event}\n\n"))
}

pub fn text_response(text: &str) -> MockResponse {
    let event = json!({ "choices": [{ "delta": { "content": text } }] });
    MockResponse::sse(format!(
        "data: {event}

data: [DONE]

"
    ))
}

pub fn delayed_text_response(text: &str, delay: Duration) -> MockResponse {
    let event = json!({ "choices": [{ "delta": { "content": text } }] });
    MockResponse::delayed_sse(format!("data: {event}\n\ndata: [DONE]\n\n"), delay)
}

pub fn text_response_with_usage(text: &str, input_tokens: u64) -> MockResponse {
    let text = json!({ "choices": [{ "delta": { "content": text } }] });
    let usage = json!({
        "choices": [],
        "usage": { "prompt_tokens": input_tokens, "completion_tokens": 3 },
    });
    MockResponse::sse(format!("data: {text}\n\ndata: {usage}\n\ndata: [DONE]\n\n"))
}

pub fn thinking_response(thinking: &str, text: &str) -> MockResponse {
    let thinking = json!({ "choices": [{ "delta": { "reasoning_content": thinking } }] });
    let text = json!({ "choices": [{ "delta": { "content": text } }] });
    MockResponse::sse(format!(
        "data: {thinking}

data: {text}

data: [DONE]

"
    ))
}

pub fn responses_response(thinking: &str, text: &str) -> MockResponse {
    let thinking = json!({
        "type": "response.reasoning_summary_text.delta",
        "item_id": "reasoning-1",
        "delta": thinking,
    });
    let reasoning_done = json!({
        "type": "response.output_item.done",
        "item": {
            "type": "reasoning",
            "id": "reasoning-1",
            "summary": [{ "type": "summary_text", "text": thinking["delta"] }],
            "encrypted_content": "encrypted-reasoning-marker",
        },
    });
    let text = json!({ "type": "response.output_text.delta", "delta": text });
    let done = json!({
        "type": "response.completed",
        "response": {
            "usage": {
                "input_tokens": 7,
                "output_tokens": 5,
                "input_tokens_details": { "cached_tokens": 2 }
            }
        }
    });
    MockResponse::sse(format!(
        "data: {thinking}

data: {reasoning_done}

data: {text}

data: {done}

data: [DONE]

"
    ))
}

pub fn truncated_responses_response(text: &str) -> MockResponse {
    let text = json!({ "type": "response.output_text.delta", "delta": text });
    MockResponse::sse(format!(
        "data: {text}

"
    ))
}

pub struct Fixture {
    _root: tempfile::TempDir,
    pub workspace: PathBuf,
    pub config: PathBuf,
    pub policy: PathBuf,
    pub state: PathBuf,
}

impl Fixture {
    pub fn new(server: &MockServer) -> Self {
        Self::with_policy(server, "unrestricted")
    }

    pub fn with_policy(server: &MockServer, mode: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = root.path().join("config.toml");
        let policy = root.path().join("policy.toml");
        std::fs::write(&config, config_text(&server.url())).unwrap();
        std::fs::write(
            &policy,
            format!(
                "mode = \"{mode}\"
"
            ),
        )
        .unwrap();
        Self {
            _root: root,
            workspace,
            config,
            policy,
            state,
        }
    }

    pub fn without_models() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = root.path().join("config.toml");
        let policy = root.path().join("policy.toml");
        std::fs::write(
            &config,
            "[providers]
",
        )
        .unwrap();
        std::fs::write(
            &policy,
            "mode = \"confirm\"
",
        )
        .unwrap();
        Self {
            _root: root,
            workspace,
            config,
            policy,
            state,
        }
    }

    pub fn spawn(&self, args: &[&str]) -> Tui {
        Tui::spawn(self, args, &self.workspace)
    }

    pub fn spawn_in(&self, cwd: &Path, args: &[&str]) -> Tui {
        Tui::spawn(self, args, cwd)
    }

    pub fn enable_auto_compaction(&self, max_context_tokens: u64) {
        let current = std::fs::read_to_string(&self.config).unwrap();
        std::fs::write(
            &self.config,
            format!(
                "[compaction.auto]\nenable = true\nmax_context_tokens = {max_context_tokens}\n\n{current}"
            ),
        )
        .unwrap();
    }

    pub fn set_truncation(&self, max_lines: usize, max_bytes: usize) {
        let current = std::fs::read_to_string(&self.config).unwrap();
        std::fs::write(
            &self.config,
            format!("[truncate]\nmax_lines = {max_lines}\nmax_bytes = {max_bytes}\n\n{current}"),
        )
        .unwrap();
    }

    pub fn output(&self, args: &[&str]) -> Output {
        self.output_with_env(args, &[])
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lofi"));
        command.args(args).current_dir(&self.workspace);
        configure_command(&mut command, self);
        command
    }

    pub fn output_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        command.output().unwrap()
    }

    pub fn events(&self) -> Vec<Value> {
        let path = self
            .session_files()
            .into_iter()
            .next()
            .expect("session transcript");
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    pub fn wait_for_event_count(&self, kind: &str, count: usize) {
        let start = Instant::now();
        while start.elapsed() < WAIT {
            let found = self
                .session_files()
                .iter()
                .filter_map(|path| std::fs::read_to_string(path).ok())
                .flat_map(|text| {
                    text.lines()
                        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                        .collect::<Vec<_>>()
                })
                .filter(|event| event["type"] == kind)
                .count();
            if found >= count {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("timed out waiting for {count} {kind} events");
    }

    pub fn session_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_files(&self.state.join("lofi").join("sessions"), &mut files);
        files.retain(|path| path.extension().is_some_and(|ext| ext == "jsonl"));
        files.sort();
        files
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub fn append_tree_siblings(&self, count: usize) {
        let path = self
            .session_files()
            .into_iter()
            .next()
            .expect("session transcript");
        let events = self.events();
        let parent_id = events
            .iter()
            .rev()
            .find_map(|event| event["leaf_id"].as_str().or_else(|| event["id"].as_str()))
            .expect("session leaf");
        let file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        let mut writer = std::io::BufWriter::new(file);
        for index in 0..count {
            serde_json::to_writer(
                &mut writer,
                &json!({
                    "id": format!("f{index:031x}"),
                    "parent_id": parent_id,
                    "type": "message",
                    "role": "user",
                    "blocks": [{
                        "type": "text",
                        "text": format!("synthetic tree prompt {index:06}"),
                    }],
                }),
            )
            .unwrap();
            writer.write_all(b"\n").unwrap();
        }
        writer.flush().unwrap();
    }
}

fn config_text(base_url: &str) -> String {
    format!(
        r#"default_model = "mock/chat"

[retry]
max_retries = 2
base_delay_ms = 1
max_delay_ms = 1

[providers.mock]
base_url = "{base_url}"
api_type = "openai-completions"
no_auth = true

[providers.mock.models.chat]
name = "A Chat"
context_window = 100000
reasoning = true
supports_image = true
thinking_level = "medium"
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.mock.models.alt]
name = "B Alternate"
context_window = 100000
reasoning = true
thinking_level = "medium"
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.responses]
base_url = "{base_url}"
api_type = "openai-responses"
no_auth = true

[providers.responses.models.reasoning]
name = "C Responses"
context_window = 100000
reasoning = true
thinking_level = "medium"
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.anthropic]
base_url = "{base_url}"
api_type = "anthropic-messages"
no_auth = true

[providers.anthropic.models.tools]
name = "D Anthropic"
context_window = 100000
reasoning = true
thinking_level = "medium"
thinking_levels = ["low", "medium", "high", "xhigh"]

[providers.google]
base_url = "{base_url}/v1beta"
api_type = "google-generative-ai"
no_auth = true

[providers.google.models.tools]
name = "E Google"
context_window = 100000
reasoning = true
supports_image = true
thinking_level = "medium"
thinking_levels = ["low", "medium", "high", "xhigh"]
"#
    )
}

fn configure_command(command: &mut Command, fixture: &Fixture) {
    command
        .env("LOFI_CONFIG", &fixture.config)
        .env("LOFI_POLICY", &fixture.policy)
        .env("LOFI_STATE_HOME", &fixture.state)
        .env_remove("OPENAI_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("GEMINI_API_KEY")
        .env_remove("GOOGLE_API_KEY");
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

pub struct Tui {
    child: Child,
    input: File,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl Tui {
    fn spawn(fixture: &Fixture, args: &[&str], cwd: &Path) -> Self {
        Self::spawn_with_env(fixture, args, cwd, &[])
    }

    fn spawn_with_env(fixture: &Fixture, args: &[&str], cwd: &Path, env: &[(&str, &str)]) -> Self {
        let pty = openpty(
            Some(&Winsize {
                ws_row: 40,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            }),
            None,
        )
        .unwrap();
        let master = File::from(pty.master);
        let slave = File::from(pty.slave);
        let stdin = slave.try_clone().unwrap();
        let stdout = slave.try_clone().unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lofi"));
        command
            .args(args)
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLUMNS", "120")
            .env("LINES", "40")
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(slave));
        configure_command(&mut command, fixture);
        command.envs(env.iter().copied());
        let child = command.spawn().unwrap();
        let mut reader_file = master.try_clone().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let reader_output = Arc::clone(&output);
        let reader = thread::spawn(move || {
            let mut chunk = [0_u8; 8192];
            while let Ok(read) = reader_file.read(&mut chunk) {
                if read == 0 {
                    break;
                }
                reader_output
                    .lock()
                    .unwrap()
                    .extend_from_slice(&chunk[..read]);
            }
        });
        let mut tui = Self {
            child,
            input: master,
            output,
            reader: Some(reader),
        };
        tui.wait_for_any(
            &[
                "mock/chat",
                "mock/alt",
                "responses/reasoning",
                "anthropic/tools",
                "google/tools",
                "(no model)",
            ],
            WAIT,
        );
        tui
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).unwrap();
        self.input.flush().unwrap();
    }

    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    pub fn resident_kib(&self) -> u64 {
        let status = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).unwrap();
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
            .expect("VmRSS")
    }

    pub fn submit(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\r");
    }

    pub fn clear_output(&self) {
        self.output.lock().unwrap().clear();
    }

    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    pub fn wait_for(&mut self, needle: &str, timeout: Duration) {
        self.wait_for_any(&[needle], timeout);
    }

    pub fn wait_for_any(&mut self, needles: &[&str], timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let found = {
                let output = self.output.lock().unwrap();
                let output = String::from_utf8_lossy(&output);
                needles.iter().any(|needle| output.contains(needle))
            };
            if found {
                return;
            }
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "timed out waiting for {needles:?}; terminal output:
{}",
            self.output()
        );
    }

    pub fn wait_exit(&mut self) {
        let start = Instant::now();
        while start.elapsed() < WAIT {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!(
            "lofi did not exit; terminal output:
{}",
            self.output()
        );
    }

    pub fn kill_now(&mut self) {
        self.child.kill().unwrap();
        let _ = self.child.wait();
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.reader.take();
    }
}

pub fn process_is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

pub struct ProcessGuard(Option<i32>);

impl ProcessGuard {
    pub fn new(pid: i32) -> Self {
        Self(Some(pid))
    }

    pub fn pid(&self) -> i32 {
        self.0.expect("armed process guard")
    }

    pub fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
        }
    }
}

pub fn wait_for_process_exit(pid: i32) {
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if !process_is_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} did not exit");
}

pub fn job_events(events: &[Value], kind: &str) -> Vec<u64> {
    events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some(kind))
        .filter_map(|event| event.get("job_id").and_then(Value::as_u64))
        .collect()
}

fn find_number(value: &Value, key: &str) -> Option<u64> {
    match value {
        Value::Object(object) => object
            .get(key)
            .and_then(Value::as_u64)
            .or_else(|| object.values().find_map(|value| find_number(value, key))),
        Value::Array(array) => array.iter().find_map(|value| find_number(value, key)),
        Value::String(text) => serde_json::from_str(text)
            .ok()
            .and_then(|value| find_number(&value, key)),
        _ => None,
    }
}

pub fn spawned_pid(fixture: &Fixture) -> i32 {
    fixture
        .events()
        .iter()
        .find_map(|event| find_number(event, "pid"))
        .and_then(|pid| i32::try_from(pid).ok())
        .expect("job pid in transcript")
}

pub fn event_types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| event.get("type").and_then(Value::as_str))
        .collect()
}

pub fn transcript_text(events: &[Value]) -> String {
    events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join(
            "
",
        )
}
