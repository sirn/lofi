#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::{openpty, Winsize};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(15);

struct MockServer {
    addr: std::net::SocketAddr,
    responses: Arc<Mutex<VecDeque<String>>>,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn start(responses: Vec<String>) -> Self {
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
                        let request = read_request(&mut stream);
                        thread_requests.lock().unwrap().push(request);
                        let body = thread_responses
                            .lock()
                            .unwrap()
                            .pop_front()
                            .unwrap_or_else(|| text_response("unexpected mock request"));
                        write_response(&mut stream, &body);
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

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn push(&self, response: String) {
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

fn read_request(stream: &mut TcpStream) -> String {
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
                .map(|i| i + 4);
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
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn write_response(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    stream.flush().unwrap();
}

fn tool_response(call_id: &str, code: &str) -> String {
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
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

fn text_response(text: &str) -> String {
    let event = json!({ "choices": [{ "delta": { "content": text } }] });
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

struct Fixture {
    _root: tempfile::TempDir,
    workspace: PathBuf,
    config: PathBuf,
    policy: PathBuf,
    state: PathBuf,
}

impl Fixture {
    fn new(server: &MockServer) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let state = root.path().join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        let config = root.path().join("config.toml");
        let policy = root.path().join("policy.toml");
        std::fs::write(
            &config,
            format!(
                "default_model = \"mock/test\"\n\n[providers.mock]\nbase_url = \"{}\"\napi_type = \"openai-completions\"\nno_auth = true\n\n[providers.mock.models.test]\nname = \"E2E Mock\"\ncontext_window = 100000\n",
                server.url()
            ),
        )
        .unwrap();
        std::fs::write(&policy, "mode = \"unrestricted\"\n").unwrap();
        Self {
            _root: root,
            workspace,
            config,
            policy,
            state,
        }
    }

    fn spawn(&self, args: &[&str]) -> Tui {
        Tui::spawn(self, args)
    }

    fn events(&self) -> Vec<Value> {
        let mut files = Vec::new();
        collect_files(&self.state.join("lofi").join("sessions"), &mut files);
        let path = files
            .into_iter()
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .expect("session transcript");
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
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

struct Tui {
    child: Child,
    input: File,
    output: Arc<Mutex<Vec<u8>>>,
    reader: Option<thread::JoinHandle<()>>,
}

impl Tui {
    fn spawn(fixture: &Fixture, args: &[&str]) -> Self {
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
            .current_dir(&fixture.workspace)
            .env("TERM", "xterm-256color")
            .env("COLUMNS", "120")
            .env("LINES", "40")
            .env("LOFI_CONFIG", &fixture.config)
            .env("LOFI_POLICY", &fixture.policy)
            .env("LOFI_STATE_HOME", &fixture.state)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(slave));
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
        tui.wait_for("mock/test", WAIT);
        tui
    }

    fn send(&mut self, bytes: &[u8]) {
        self.input.write_all(bytes).unwrap();
        self.input.flush().unwrap();
    }

    fn submit(&mut self, text: &str) {
        self.send(text.as_bytes());
        self.send(b"\r");
    }

    fn clear_output(&self) {
        self.output.lock().unwrap().clear();
    }

    fn wait_for(&mut self, needle: &str, timeout: Duration) {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if String::from_utf8_lossy(&self.output.lock().unwrap()).contains(needle) {
                return;
            }
            if self.child.try_wait().unwrap().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let output = String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned();
        panic!(
            "timed out waiting for {needle:?}; terminal output:
{output}"
        );
    }

    fn kill_now(&mut self) {
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

fn process_is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

struct ProcessGuard(Option<i32>);

impl ProcessGuard {
    fn new(pid: i32) -> Self {
        Self(Some(pid))
    }

    fn pid(&self) -> i32 {
        self.0.expect("armed process guard")
    }

    fn disarm(&mut self) {
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

fn wait_for_process_exit(pid: i32) {
    let start = Instant::now();
    while start.elapsed() < WAIT {
        if !process_is_alive(pid) {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("process {pid} did not exit");
}

fn job_events(events: &[Value], kind: &str) -> Vec<u64> {
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

fn spawned_pid(fixture: &Fixture) -> i32 {
    fixture
        .events()
        .iter()
        .find_map(|event| find_number(event, "pid"))
        .and_then(|pid| i32::try_from(pid).ok())
        .expect("job pid in transcript")
}

#[test]
fn completion_keeps_its_owner_across_later_execs() {
    let server = MockServer::start(vec![
        tool_response(
            "spawn",
            "return await lofi.jobSpawn({ cmd: \"sleep 1\", notify: false });",
        ),
        tool_response("later", "return { laterExec: true };"),
        text_response("completion scenario settled"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("run completion scenario");
    tui.wait_for("completion scenario settled", WAIT);
    let mut job = ProcessGuard::new(spawned_pid(&fixture));
    wait_for_process_exit(job.pid());
    tui.submit("/jobs");
    tui.wait_for("completed", WAIT);
    job.disarm();

    let events = fixture.events();
    let started = job_events(&events, "job_started");
    let finished = job_events(&events, "job_finished");
    assert_eq!(server.request_count(), 3);
    assert_eq!(started.len(), 1);
    assert_eq!(finished, started);
    let start_index = events
        .iter()
        .position(|event| event.get("type") == Some(&Value::String("job_started".into())))
        .unwrap();
    let finish_index = events
        .iter()
        .position(|event| event.get("type") == Some(&Value::String("job_finished".into())))
        .unwrap();
    assert!(start_index < finish_index);
}

#[test]
fn branch_switch_retains_then_releases_owned_job() {
    let server = MockServer::start(vec![
        tool_response(
            "spawn",
            "return await lofi.jobSpawn({ cmd: \"sleep 60\", notify: false });",
        ),
        tool_response("later", "return { laterExec: true };"),
        text_response("branch scenario settled"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("run branch scenario");
    tui.wait_for("branch scenario settled", WAIT);
    let mut job = ProcessGuard::new(spawned_pid(&fixture));
    let pid = job.pid();
    assert!(process_is_alive(pid));

    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("user: loading", WAIT);
    tui.wait_for("exec: jobSpawn sleep 60", WAIT);
    tui.send(b"\x1b[A\r");
    thread::sleep(Duration::from_millis(200));
    assert!(process_is_alive(pid));

    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("user: loading", WAIT);
    tui.wait_for("exec: jobSpawn sleep 60", WAIT);
    for _ in 0..8 {
        tui.send(b"\x1b[A");
    }
    tui.send(b"\r");
    wait_for_process_exit(pid);
    job.disarm();

    tui.send(b"\x03");
    tui.clear_output();
    tui.submit("/jobs");
    tui.wait_for("no background jobs this session", WAIT);
    let events = fixture.events();
    assert_eq!(job_events(&events, "job_started").len(), 1);
    assert!(job_events(&events, "job_finished").is_empty());
}

#[test]
fn resume_reports_job_from_interrupted_process_as_stale() {
    let server = MockServer::start(vec![
        tool_response(
            "spawn",
            "return await lofi.jobSpawn({ cmd: \"sleep 60\", notify: false });",
        ),
        text_response("stale scenario settled"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("run stale scenario");
    first.wait_for("stale scenario settled", WAIT);
    let mut job = ProcessGuard::new(spawned_pid(&fixture));
    let pid = job.pid();
    assert!(process_is_alive(pid));
    first.kill_now();
    drop(first);

    server.push(text_response("resume scenario settled"));
    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("check resumed session");
    resumed.wait_for("resume scenario settled", WAIT);
    let requests = server.requests();
    let resume_request = requests.last().unwrap();
    assert!(resume_request.contains("session resumed: jobs ["));
    assert!(resume_request.contains("their ids are stale"));

    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
    wait_for_process_exit(pid);
    job.disarm();
}
