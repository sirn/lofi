use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use crate::support::{text_response, text_response_with_usage, Fixture, MockServer, WAIT};
use serde_json::{json, Value};

fn claude_fixture() -> Fixture {
    let fixture = Fixture::without_models();
    install_fake_claude(&fixture);
    fixture
}

fn install_fake_claude(fixture: &Fixture) {
    use std::os::unix::fs::PermissionsExt;
    let command = fixture.workspace.join("fake-claude");
    let executable = std::env::current_exe().unwrap();
    std::fs::write(&command, format!(
        "#!/bin/sh\nprintf '%s\\0' \"$@\" > claude-args\nLOFI_FAKE_CLAUDE=1 exec '{}' --exact backends::fake_claude --nocapture 3>&1 1>/dev/null\n",
        executable.display()
    )).unwrap();
    std::fs::set_permissions(&command, std::fs::Permissions::from_mode(0o700)).unwrap();
    let current = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            "{current}[backends.claude]\ntype = \"claude-code\"\ncommand = {}\nmodel = \"test-model\"\n",
            json!(command)
        ),
    )
    .unwrap();
}

fn read(reader: &mut impl BufRead) -> Value {
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0, "protocol EOF");
    serde_json::from_str(&line).unwrap()
}

fn send(writer: &mut impl Write, value: &Value) {
    writeln!(writer, "{value}").unwrap();
    writer.flush().unwrap();
}

struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(config: &Value) -> Self {
        let mut command = Command::new(config["command"].as_str().unwrap());
        for arg in config["args"].as_array().unwrap() {
            command.arg(arg.as_str().unwrap());
        }
        for env in config["env"].as_array().unwrap() {
            command.env(
                env["name"].as_str().unwrap(),
                env["value"].as_str().unwrap(),
            );
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut mcp = Self {
            child,
            input,
            output,
            next: 0,
        };
        let initialized = mcp.call("initialize", &json!({"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"mock","version":"1"}}));
        assert_eq!(initialized["capabilities"]["tools"], json!({}));
        send(
            &mut mcp.input,
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        );
        let tools = mcp.call("tools/list", &json!({}));
        assert_eq!(tools["tools"].as_array().unwrap().len(), 1);
        assert_eq!(tools["tools"][0]["name"], "exec");
        mcp
    }

    fn call(&mut self, method: &str, params: &Value) -> Value {
        let id = self.next;
        self.next += 1;
        send(
            &mut self.input,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        );
        let response = read(&mut self.output);
        assert_eq!(response["id"], id);
        assert!(response.get("error").is_none(), "{response}");
        response["result"].clone()
    }
}
impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fake_claude_setup() -> (String, Mcp) {
    let args = std::fs::read_to_string("claude-args").unwrap();
    let args: Vec<_> = args.trim_end_matches('\0').split('\0').collect();
    let value = |flag| args[args.iter().position(|arg| *arg == flag).unwrap() + 1];
    // The system prompt is a file argument: argv has a per-argument limit.
    assert!(!args.contains(&"--system-prompt"));
    let prompt_path = value("--system-prompt-file");
    let prompt = std::fs::read_to_string(prompt_path).unwrap();
    assert!(prompt.contains("You are lofi"));
    std::fs::write("claude-system-prompt-path", prompt_path).unwrap();
    std::fs::write("claude-system-prompt-bytes", prompt.len().to_string()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(prompt_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "system prompt file must stay private");
    }
    let effort = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--effort="))
        .unwrap_or("default");
    std::fs::write("claude-effort", effort).unwrap();
    assert_eq!(value("--tools"), "");
    assert_eq!(value("--allowedTools"), "mcp__lofi__exec");
    assert_eq!(value("--setting-sources"), "");
    assert_eq!(value("--settings"), r#"{"disableAllHooks":true}"#);
    assert_eq!(value("--permission-mode"), "dontAsk");
    assert!(args.contains(&"--strict-mcp-config"));
    assert!(args.contains(&"--disable-slash-commands"));
    assert!(args.contains(&"--no-chrome"));
    // Lofi owns compaction: the child is told to compact far past any window
    // Lofi budgets, so it never rewrites a transcript Lofi tracks.
    assert_eq!(value("--autocompact"), "1000000");
    let model = args
        .iter()
        .find_map(|arg| arg.strip_prefix("--model="))
        .unwrap_or("default");
    std::fs::write("selected-model", model).unwrap();
    assert_eq!(std::env::var("ENABLE_TOOL_SEARCH").unwrap(), "false");
    let session_arg = args
        .iter()
        .find(|arg| arg.starts_with("--session-id=") || arg.starts_with("--resume="))
        .unwrap();
    let (method, id) = session_arg.split_once('=').unwrap();
    if method == "--resume" {
        std::fs::write("claude-resumed", id).unwrap();
    }
    std::fs::write("claude-session", id).unwrap();
    std::fs::write("claude-method", method).unwrap();
    // The bridge token must not be readable from argv.
    assert!(!args.iter().any(|arg| arg.contains("LOFI_EXEC_TOKEN")));
    let mcp_path = value("--mcp-config");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(mcp_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "bridge config must stay private");
    }
    let config: Value = serde_json::from_slice(&std::fs::read(mcp_path).unwrap()).unwrap();
    assert_eq!(config["mcpServers"].as_object().unwrap().len(), 1);
    let config = &config["mcpServers"]["lofi"];
    let env: Vec<_> = config["env"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, value)| json!({"name":name,"value":value}))
        .collect();
    let mcp = Mcp::start(&json!({"command":config["command"],"args":config["args"],"env":env}));
    (id.to_owned(), mcp)
}

/// Stand in for the evaluator child: record what the caller sent and answer
/// with a verdict. A child that ignored the caller's system prompt would run
/// under the session's, which the prompt these files hold would show.
fn fake_claude_oneshot() {
    let args = std::fs::read_to_string("claude-args").unwrap();
    let args: Vec<_> = args.trim_end_matches('\0').split('\0').collect();
    let value = |flag| args[args.iter().position(|arg| *arg == flag).unwrap() + 1];
    let prompt = std::fs::read_to_string(value("--system-prompt-file")).unwrap();
    std::fs::write("claude-evaluator-prompt", prompt).unwrap();
    std::fs::write(
        "claude-evaluator-max-tokens",
        std::env::var("CLAUDE_CODE_MAX_OUTPUT_TOKENS").unwrap_or_default(),
    )
    .unwrap();
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/fd/3")
        .unwrap();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let _ = read(&mut input);
    send(
        &mut output,
        &json!({"type":"result","subtype":"success","is_error":false,
            "result":"{\"risk\":\"low\",\"reason\":\"cli approval\"}"}),
    );
}

fn fake_claude_control(prompt: &Value, output: &mut impl Write) -> bool {
    if prompt["type"] != "control_request" {
        return false;
    }
    assert_eq!(prompt["request"]["subtype"], "set_model");
    let model = prompt["request"]["model"].as_str().unwrap_or("default");
    std::fs::write("selected-model", model).unwrap();
    send(
        output,
        &json!({"type":"control_response","response":{
        "subtype":if model == "rejected" { "error" } else { "success" },
        "request_id":prompt["request_id"]}}),
    );
    true
}

fn fake_claude_model() -> String {
    if let Ok(model) = std::fs::read_to_string("response-model") {
        return model;
    }
    match std::fs::read_to_string("selected-model").unwrap().as_str() {
        "default" | "sonnet" => "claude-sonnet-test".into(),
        model => model.into(),
    }
}

fn fake_claude_init(output: &mut impl Write, id: &str, text: &str) {
    let tools = if text == "extra tool" {
        json!(["mcp__lofi__exec", "Bash"])
    } else {
        json!(["mcp__lofi__exec"])
    };
    let servers = if text == "extra server" {
        json!([{"name":"lofi","status":"connected"},{"name":"other","status":"connected"}])
    } else {
        json!([{"name":"lofi","status":"connected"}])
    };
    let model = std::fs::read_to_string("init-model")
        .ok()
        .or_else(|| match text {
            "stream model" | "assistant model" | "no model" => None,
            _ => Some(fake_claude_model()),
        });
    send(
        output,
        &json!({"type":"system","subtype":"init","session_id":id,"tools":tools,"mcp_servers":servers,"model":model}),
    );
}

fn fake_claude_code(text: &str) -> &'static str {
    match text {
        "shell" => r#"return await lofi.bash({cmd:"touch denied-marker",timeoutMs:1000})"#,
        "cancel" => "while (true) {}",
        "job" => r#"return await lofi.jobSpawn({cmd:"sleep 1",timeoutMs:3000})"#,
        "subagent" => {
            r#"const out = await lofi.subagent({prompt:"child task",timeoutMs:5000}); await lofi.write({path:"subagent-result",text:JSON.stringify(out)}); return out"#
        }
        "subagent shell" => {
            r#"const out = await lofi.subagent({prompt:"shell",timeoutMs:5000}); await lofi.write({path:"subagent-result",text:JSON.stringify(out)}); return out"#
        }
        "subagent override" => {
            r#"const out = await lofi.subagent({prompt:"child task",model:"mock/child",timeoutMs:5000}); await lofi.write({path:"subagent-result",text:JSON.stringify(out)}); return out"#
        }
        _ => {
            r#"await lofi.write({path:"backend-result",text:"real exec"}); return await lofi.read("backend-result")"#
        }
    }
}

fn fake_claude_usage(
    output: &mut impl Write,
    id: &str,
    model: Option<&str>,
    text: &str,
    turn: u32,
) {
    send(
        output,
        &json!({"type":"assistant","session_id":id,"message":{"model":model,
        "usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":2000,"cache_creation_input_tokens":3000},
        "content":[{"type":"text","text":text}]}}),
    );
    send(
        output,
        &json!({"type":"result","subtype":"success","is_error":false,"session_id":id,"result":text,
        "usage":{"input_tokens":10_000,"output_tokens":2000,"cache_read_input_tokens":20_000,"cache_creation_input_tokens":30_000},
        "total_cost_usd": f64::from(turn) * 0.02,
        "modelUsage":{(model.unwrap_or("test-model")):{"contextWindow":128_000}}}),
    );
}

fn fake_claude_stream(output: &mut impl Write, id: &str, text: &str) {
    let block = |kind: &str| {
        json!({"type":"stream_event","session_id":id,
            "event":{"type":"content_block_start","index":0,"content_block":{"type":kind}}})
    };
    send(output, &block("thinking"));
    for chunk in ["con", "sidering"] {
        send(
            output,
            &json!({"type":"stream_event","session_id":id,"event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":chunk}}}),
        );
    }
    send(
        output,
        &json!({"type":"stream_event","session_id":id,
            "event":{"type":"content_block_stop","index":0}}),
    );
    send(output, &block("text"));
    let split = text.len() / 2;
    for chunk in [&text[..split], &text[split..]] {
        send(
            output,
            &json!({"type":"stream_event","session_id":id,"event":{"type":"content_block_delta","delta":{"type":"text_delta","text":chunk}}}),
        );
    }
}

fn fake_claude_prompt(prompt: &Value, id: &str) -> String {
    assert_eq!(prompt["type"], "user");
    assert_eq!(prompt["session_id"], id);
    let text = prompt["message"]["content"].as_str().unwrap();
    std::fs::write("claude-prompt", text).unwrap();
    assert!(!text.contains("You are lofi"));
    text.into()
}

fn fake_claude_failure(output: &mut impl Write, id: &str, subtype: &str) {
    send(
        output,
        &json!({"type":"result","subtype":subtype,"is_error":true,"session_id":id}),
    );
}

/// One completed tool cycle, then the failure: the turn keeps the round it
/// finished, and the next turn resumes with it.
fn fake_claude_tool_then_error(mcp: &mut Mcp, output: &mut impl Write, id: &str, text: &str) {
    let result = mcp.call(
        "tools/call",
        &json!({"name":"exec","arguments":{"code":fake_claude_code(text)}}),
    );
    assert!(result["isError"] != true, "{result}");
    fake_claude_stream(output, id, "claude answer");
    fake_claude_failure(output, id, "error_during_execution");
}

#[test]
fn fake_claude() {
    if std::env::var_os("LOFI_FAKE_CLAUDE").is_none() {
        return;
    }
    // A one-shot call is another protocol: no session, no bridge, one frame
    // in and one result out.
    if std::fs::read_to_string("claude-args").is_ok_and(|args| {
        let mut args = args.split('\0');
        // No bridge and no session means the one-shot protocol.
        !args.any(|arg| arg == "--mcp-config")
    }) {
        fake_claude_oneshot();
        return;
    }
    let (id, mut mcp) = fake_claude_setup();
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/fd/3")
        .unwrap();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut initialized = false;
    let mut turn = 0;
    loop {
        let prompt = read(&mut input);
        if fake_claude_control(&prompt, &mut output) {
            continue;
        }
        let text = fake_claude_prompt(&prompt, &id);
        let text = text.as_str();
        let report_usage = !matches!(
            text,
            "no usage" | "init model" | "stream model" | "assistant model"
        );
        if !initialized {
            fake_claude_init(&mut output, &id, text);
            initialized = true;
        }
        if text.starts_with("extra ") {
            std::thread::sleep(std::time::Duration::from_secs(10));
            panic!("adapter did not reject extra capabilities");
        }
        if text == "disconnect" {
            return;
        }
        if text == "login error" || std::fs::read_to_string("selected-model").unwrap() == "rejected"
        {
            fake_claude_failure(&mut output, &id, "success");
            continue;
        }
        if text == "error" {
            fake_claude_failure(&mut output, &id, "error_during_execution");
            continue;
        }
        if text == "fail after tool" {
            fake_claude_tool_then_error(&mut mcp, &mut output, &id, text);
            continue;
        }
        if text == "wrong session" {
            send(
                &mut output,
                &json!({"type":"result","subtype":"success","is_error":false,"session_id":"wrong"}),
            );
            continue;
        }
        let model = (text != "no model").then(fake_claude_model);
        if text == "stream model" {
            send(
                &mut output,
                &json!({"type":"stream_event","session_id":id,"event":{
                "type":"message_start","message":{"model":model}}}),
            );
        }
        if text == "assistant model" {
            send(
                &mut output,
                &json!({"type":"assistant","session_id":id,"message":{"model":model}}),
            );
        }
        let code = fake_claude_code(text);
        let result = mcp.call(
            "tools/call",
            &json!({"name":"exec","arguments":{"code":code}}),
        );
        let text = if result["isError"] == true {
            "exec denied"
        } else {
            "claude answer"
        };
        fake_claude_stream(&mut output, &id, text);
        turn += 1;
        if report_usage {
            fake_claude_usage(&mut output, &id, model.as_deref(), text, turn);
        } else {
            send(
                &mut output,
                &json!({"type":"result","subtype":"success","is_error":false,"session_id":id,"result":text}),
            );
        }
    }
}

#[test]
fn claude_print_uses_real_exec_and_does_not_duplicate_text() {
    let fixture = claude_fixture();
    let output = fixture.output(&["--print", "write"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout)
            .matches("claude answer")
            .count(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("backend-result")).unwrap(),
        "real exec"
    );
}

#[test]
fn claude_default_model_uses_reports_and_survives_resume() {
    for prompt in ["init model", "stream model", "assistant model", "no model"] {
        let fixture = claude_fixture();
        let config = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            config.replace("model = \"test-model\"\n", ""),
        )
        .unwrap();
        let mut tui = fixture.spawn(&[]);
        tui.wait_for_screen("claude/default", WAIT);
        tui.submit(prompt);
        fixture.wait_for_event_count("turn_end", 1);
        let expected = if prompt == "no model" {
            "default"
        } else {
            "claude-sonnet-test"
        };
        tui.wait_for_screen(&format!("claude/{expected}"), WAIT);
        assert!(!tui.screen_text().contains("backend/claude"));
        let events = fixture.events();
        let terminal = events
            .iter()
            .find(|event| event["type"] == "turn_end")
            .unwrap();
        assert_eq!(terminal["model"]["provider"], "claude");
        assert_eq!(terminal["model"]["id"], expected);
        tui.submit("/quit");
        tui.wait_exit();
        let mut tui = fixture.spawn(&["--continue"]);
        tui.wait_for_screen(&format!("claude/{expected}"), WAIT);
        tui.submit("write");
        fixture.wait_for_event_count("turn_end", 2);
        tui.wait_for_screen("claude/claude-sonnet-test", WAIT);
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
            "default"
        );
        tui.submit("/quit");
        tui.wait_exit();
    }
}

#[test]
fn claude_initialization_model_stays_visible_and_survives_resume() {
    let fixture = claude_fixture();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("model = \"test-model\"\n", ""),
    )
    .unwrap();
    std::fs::write(fixture.workspace.join("init-model"), "claude-opus-5-5[1m]").unwrap();
    std::fs::write(fixture.workspace.join("response-model"), "claude-opus-5-5").unwrap();
    let mut tui = fixture.spawn(&[]);
    tui.wait_for_screen("claude/default", WAIT);
    for (count, prompt) in [(1, "stream model"), (2, "write")] {
        tui.submit(prompt);
        fixture.wait_for_event_count("turn_end", count);
        tui.wait_for("Done", WAIT);
        tui.submit("/session");
        tui.wait_for_screen("Workspace", WAIT);
        assert!(tui.screen_text().contains("claude/claude-opus-5-5[1m]"));
        let events = fixture.events();
        let terminal = events
            .iter()
            .rfind(|event| event["type"] == "turn_end")
            .unwrap();
        assert_eq!(terminal["model"]["id"], "claude-opus-5-5");
        tui.send(b"\x1b");
        tui.wait_gone("Workspace", WAIT);
        tui.clear_output();
    }
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.wait_for_screen("claude/claude-opus-5-5[1m]", WAIT);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 3);
    tui.submit("/session");
    tui.wait_for_screen("Workspace", WAIT);
    assert!(tui.screen_text().contains("claude/claude-opus-5-5[1m]"));
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "default"
    );
    tui.send(b"\x1b");
    tui.wait_gone("Workspace", WAIT);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_model_alias_stays_selected_while_reports_use_the_actual_model() {
    let fixture = claude_fixture();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            "{}models = [\"other-model\", \"sonnet\"]\n",
            config.replace("model = \"test-model\"", "model = \"sonnet\"")
        ),
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);
    tui.wait_for_screen("claude/sonnet", WAIT);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.wait_for_screen("claude/claude-sonnet-test", WAIT);
    let events = fixture.events();
    let terminal = events
        .iter()
        .find(|event| event["type"] == "turn_end")
        .unwrap();
    assert_eq!(terminal["model"]["id"], "claude-sonnet-test");
    tui.submit("/model");
    tui.wait_for_screen("Switch model", WAIT);
    tui.send(b"\r");
    tui.wait_for("switched to claude/sonnet", WAIT);
    tui.submit("again");
    fixture.wait_for_event_count("turn_end", 2);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "sonnet"
    );
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.wait_for_screen("claude/claude-sonnet-test", WAIT);
    tui.submit("resume");
    fixture.wait_for_event_count("turn_end", 3);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "sonnet"
    );
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&["--continue", "--model", "claude/other-model"]);
    tui.wait_for_screen("claude/other-model", WAIT);
    tui.submit("override");
    fixture.wait_for_event_count("turn_end", 4);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "other-model"
    );
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_reported_model_is_saved_for_failed_and_cancelled_turns() {
    for (prompt, terminal) in [("error", "turn_failed"), ("cancel", "turn_cancelled")] {
        let fixture = claude_fixture();
        let config = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            config.replace("model = \"test-model\"\n", ""),
        )
        .unwrap();
        let mut tui = fixture.spawn(&[]);
        tui.submit(prompt);
        tui.wait_for_screen("claude/claude-sonnet-test", WAIT);
        if prompt == "cancel" {
            tui.wait_for("Exec", WAIT);
            tui.send(b"\x1b");
        }
        fixture.wait_for_event_count(terminal, 1);
        let events = fixture.events();
        let event = events
            .iter()
            .find(|event| event["type"] == terminal)
            .unwrap();
        assert_eq!(event["model"]["provider"], "claude");
        assert_eq!(event["model"]["id"], "claude-sonnet-test");
        tui.submit("/quit");
        tui.wait_exit();
    }
}

#[test]
fn claude_rejects_extra_capabilities_and_protocol_failures() {
    for prompt in [
        "extra tool",
        "extra server",
        "disconnect",
        "wrong session",
        "error",
        "login error",
    ] {
        let fixture = claude_fixture();
        let output = fixture.output(&["--print", prompt]);
        assert!(!output.status.success(), "{prompt}");
        assert!(!fixture.workspace.join("backend-result").exists());
    }
}

fn configure_auto_mode(fixture: &Fixture, server: &MockServer) {
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"default_model = "claude/test-model"
{config}
[providers.evaluator]
api_type = "openai-completions"
base_url = "{}"
no_auth = true
[providers.evaluator.models.judge]

[providers.unused]
api_type = "openai-completions"
api_key = "!touch unused-credential; exit 1"
"#,
            server.url()
        ),
    )
    .unwrap();
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        r#"mode = "confirm"
[auto_mode]
enable = true
model = "evaluator/judge"
max_tokens = 128
"#,
    )
    .unwrap();
}

#[test]
fn external_auto_mode_approves_with_only_the_evaluator_provider() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![text_response(
        r#"{"risk":"low","reason":"fixture approval"}"#,
    )]);
    configure_auto_mode(&fixture, &server);
    let output = fixture.output(&["--print", "shell"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.workspace.join("denied-marker").exists());
    assert!(!fixture.workspace.join("unused-credential").exists());
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("shell-command risk evaluator"));
    assert!(requests[0].body.contains(r#""model":"judge""#));
}

#[test]
fn external_auto_mode_keeps_the_evaluator_credentials_for_a_subagent() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![
        text_response(r#"{"risk":"low","reason":"parent approval"}"#),
        text_response(r#"{"risk":"low","reason":"child approval"}"#),
    ]);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"default_model = "claude/test-model"
{config}
[providers.evaluator]
api_type = "openai-completions"
base_url = "{}"
api_key = "$EVAL_KEY"
[providers.evaluator.models.judge]
"#,
            server.url()
        ),
    )
    .unwrap();
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        r#"mode = "confirm"
[auto_mode]
enable = true
model = "evaluator/judge"
max_tokens = 128
"#,
    )
    .unwrap();

    let output = fixture.output(&["-e", "EVAL_KEY=eval-secret", "--print", "subagent shell"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.workspace.join("denied-marker").exists());
    let requests = server.requests();
    assert_eq!(requests.len(), 1, "the child judges the command once");
    assert!(
        requests[0]
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer eval-secret"),
        "{}",
        requests[0].headers
    );
}

#[test]
fn external_auto_mode_runs_the_evaluator_on_the_cli_backend() {
    let fixture = claude_fixture();
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        r#"mode = "confirm"
[auto_mode]
enable = true
model = "claude/test-model"
max_tokens = 128
"#,
    )
    .unwrap();
    let output = fixture.output(&["--print", "shell"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.workspace.join("denied-marker").exists());
    let prompt =
        std::fs::read_to_string(fixture.workspace.join("claude-evaluator-prompt")).unwrap();
    assert!(
        prompt.contains("shell-command risk evaluator"),
        "the child must run under the caller's prompt: {prompt}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-evaluator-max-tokens")).unwrap(),
        "128",
        "the caller's output budget must reach the child"
    );
}

#[test]
fn external_auto_mode_does_not_approve_ask_or_invalid_results() {
    for response in [r#"{"risk":"confirm","reason":"needs review"}"#, "invalid"] {
        let fixture = claude_fixture();
        let server = MockServer::start(vec![text_response(response)]);
        configure_auto_mode(&fixture, &server);
        let output = fixture.output(&["--print", "shell"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("exec denied"));
        assert!(!fixture.workspace.join("denied-marker").exists());
        assert_eq!(server.requests().len(), 1);
    }
}

#[test]
fn external_auto_mode_tui_requests_confirmation_when_evaluator_asks() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![text_response(
        r#"{"risk":"confirm","reason":"needs review"}"#,
    )]);
    configure_auto_mode(&fixture, &server);
    let mut tui = fixture.spawn(&[]);
    tui.submit("shell");
    tui.wait_for("Permission Required", WAIT);
    assert!(!fixture.workspace.join("denied-marker").exists());
    tui.send(b"a");
    tui.wait_for("claude answer", WAIT);
    assert!(fixture.workspace.join("denied-marker").exists());
    assert_eq!(server.requests().len(), 1);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_preserves_shell_approval() {
    let fixture = claude_fixture();
    let output = fixture.output(&["--print", "shell"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("exec denied"));
    assert!(!fixture.workspace.join("denied-marker").exists());
}

#[test]
fn claude_tui_keeps_context_and_resumes_by_id() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.wait_for("context 0/200k", WAIT);
    tui.submit("write");
    tui.wait_for("context 6k/128k", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("again");
    fixture.wait_for_event_count("turn_end", 2);
    tui.submit("/quit");
    tui.wait_exit();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let id = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.wait_for("context 6k/128k", WAIT);
    tui.submit("resume");
    fixture.wait_for_event_count("turn_end", 3);
    // The CLI reopened the session it saved, not a fresh one.
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-resumed")).unwrap(),
        id
    );
    tui.submit("/quit");
    tui.wait_exit();
    let events = fixture.events();
    let assistant_responses: Vec<_> = events
        .iter()
        .filter(|event| {
            event["type"] == "message"
                && event["role"] == "assistant"
                && event["blocks"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "thinking"))
        })
        .collect();
    assert_eq!(assistant_responses.len(), 3);
    for response in assistant_responses {
        assert_eq!(
            response["blocks"],
            json!([
                {"type": "thinking", "text": "considering", "signature": null},
                {"type": "text", "text": "claude answer"}
            ])
        );
    }
    let transcript = serde_json::to_string(&events).unwrap();
    assert!(transcript.contains("real exec"));
}

#[test]
fn claude_cancellation_reopens_the_session_on_the_next_turn() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("cancel");
    tui.wait_for("Exec", WAIT);
    tui.send(b"\x1b");
    fixture.wait_for_event_count("turn_cancelled", 1);
    let id = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    // Cancelling closes the child, so the next turn resumes the CLI session
    // from the checkpoint instead of failing.
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    assert!(fixture.workspace.join("backend-result").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--resume"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-resumed")).unwrap(),
        id
    );
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_turn_cost_is_the_per_turn_delta() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("again");
    fixture.wait_for_event_count("turn_end", 2);
    tui.submit("/quit");
    tui.wait_exit();
    // The CLI reports cumulative spend per child process, so each turn keeps
    // only its own delta.
    let costs: Vec<_> = fixture
        .events()
        .iter()
        .filter(|event| event["type"] == "turn_end")
        .map(|event| event["cost"].as_f64().unwrap())
        .collect();
    assert_eq!(costs.len(), 2);
    for cost in costs {
        assert!((cost - 0.02).abs() < 1e-9, "{cost}");
    }
}

#[test]
fn claude_records_the_context_it_reported() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/quit");
    tui.wait_exit();

    // The same record a provider turn writes, so replay restores the gauge
    // whichever transport ran the turn.
    let reported: Vec<_> = fixture
        .events()
        .into_iter()
        .filter(|event| event["type"] == "context_reported")
        .collect();
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert_eq!(reported[0]["context"]["size"], 128_000);
    assert!(
        reported[0]["context"]["used"].as_u64().unwrap() > 0,
        "{reported:?}"
    );
}

#[test]
fn claude_records_how_long_the_child_thought() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/quit");
    tui.wait_exit();
    // The provider path closes its thinking block when the first non-thinking
    // delta arrives; the CLI path closes it on the block's own stop frame.
    let timing: Vec<_> = fixture
        .events()
        .into_iter()
        .filter(|event| event["type"] == "thinking_timing")
        .collect();
    assert_eq!(timing.len(), 1, "{timing:?}");
}

#[test]
fn claude_effort_accepts_the_cli_maximum() {
    let fixture = claude_fixture();
    let output = fixture.output(&["--model", "claude/test-model:max", "--print", "write"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-effort")).unwrap(),
        "max"
    );
}

#[test]
fn claude_effort_flag_follows_the_thinking_level() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-effort")).unwrap(),
        "default"
    );
    tui.submit("/thinking");
    tui.wait_for("Thinking level", WAIT);
    // Rows are off / low / medium / high / xhigh, pre-selected at off.
    tui.send(b"\x1b[B\x1b[B\r");
    tui.submit("again");
    fixture.wait_for_event_count("turn_end", 2);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-effort")).unwrap(),
        "medium"
    );
    // A new effort restarts the child, and the CLI keeps the conversation.
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-resumed")).unwrap(),
        std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap()
    );
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_system_prompt_file_carries_a_large_prompt() {
    let fixture = claude_fixture();
    let rules = "x".repeat(200_000);
    std::fs::write(fixture.workspace.join("AGENTS.md"), &rules).unwrap();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/quit");
    tui.wait_exit();
    let bytes: usize =
        std::fs::read_to_string(fixture.workspace.join("claude-system-prompt-bytes"))
            .unwrap()
            .parse()
            .unwrap();
    assert!(bytes > 200_000, "system prompt was {bytes} bytes");
    // The prompt goes to a private state file, never into the workspace.
    let path =
        std::fs::read_to_string(fixture.workspace.join("claude-system-prompt-path")).unwrap();
    assert!(
        std::path::Path::new(&path).starts_with(&fixture.state),
        "{path}"
    );
    assert!(!std::path::Path::new(&path).starts_with(&fixture.workspace));
}

#[test]
fn claude_compaction_starts_a_new_backend_session() {
    let server = MockServer::start(vec![
        text_response("summary of the first turns"),
        text_response("summary of the first turns"),
    ]);
    let fixture = Fixture::new(&server);
    install_fake_claude(&fixture);
    let mut tui = fixture.spawn(&["--model", "claude/test-model"]);
    for turn in 1..=4 {
        tui.submit(&format!("turn {turn} marker"));
        fixture.wait_for_event_count("turn_end", turn);
    }
    let first = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    // The journal records turn_end before the app clears its run state.
    tui.wait_idle(WAIT);
    tui.submit("/compact");
    tui.wait_for("Compacted", WAIT);
    fixture.wait_for_event_count("compaction", 1);
    tui.submit("after compact");
    fixture.wait_for_event_count("turn_end", 5);
    let second = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("<conversation_history>"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_auto_compaction_starts_a_new_backend_session() {
    let server = MockServer::start(vec![
        text_response("summary of the first turn"),
        text_response("summary of the first turn"),
    ]);
    let fixture = Fixture::new(&server);
    install_fake_claude(&fixture);
    // The fake child reports 6k of context per turn, so this cap fires after
    // the first turn without a manual /compact.
    fixture.enable_auto_compaction(1000);
    let mut tui = fixture.spawn(&["--model", "claude/test-model"]);
    for turn in 1..=3 {
        tui.submit(&format!("turn {turn} marker"));
        fixture.wait_for_event_count("turn_end", turn);
    }
    tui.wait_for("Compacted", WAIT);
    fixture.wait_for_event_count("compaction", 1);
    let first = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    tui.submit("after auto compact");
    fixture.wait_for_event_count("turn_end", 4);
    let second = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("<conversation_history>"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_hard_cap_is_lofis_compaction() {
    let fixture = claude_fixture();
    // The fake child reports a 6k fill against a 100 token window. Reserving
    // 20 leaves an 80 token hard cap, so the turn ends at Lofi's cap: the
    // child is never the one deciding that the context is full.
    let current = std::fs::read_to_string(&fixture.config).unwrap();
    let current = current.replacen(
        "model = \"test-model\"",
        "model = \"test-model\"\ncontext_window = 100",
        1,
    );
    std::fs::write(
        &fixture.config,
        format!("[compaction]\nreserved_context_tokens = 20\n\n{current}"),
    )
    .unwrap();
    let mut tui = fixture.spawn(&["--model", "claude/test-model"]);
    tui.submit("hard cap prompt");
    // The turn stops at the cap and Lofi itself decides that one exchange is
    // not enough to compact.
    tui.wait_for("could not compact at the hard cap", WAIT);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_resumes_after_a_user_shell() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    let id = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    tui.submit("!printf shell-marker");
    fixture.wait_for_event_count("user_shell", 1);
    tui.submit("after shell");
    fixture.wait_for_event_count("turn_end", 2);
    // The child stayed alive, so the shell output reaches it as a delta.
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap(),
        id
    );
    // The shell output is the only missing context, and the CLI still owns the
    // turns before it.
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("shell-marker"), "{prompt}");
    assert!(prompt.contains("<conversation_update>"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_new_session_starts_a_new_backend_session() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    let first = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    // The journal records turn_end before the app clears its run state.
    tui.wait_idle(WAIT);
    tui.submit("/new");
    tui.submit("fresh");
    fixture.wait_for_event_count("turn_end", 2);
    let second = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    // A new session has no history to transfer.
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(!prompt.contains("<conversation_history>"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn switching_to_a_backend_keeps_cost_tracking() {
    let server = MockServer::start(vec![text_response_with_usage("native answer marker", 5)]);
    let fixture = claude_fixture();
    configure_native_model(&fixture, &server);
    // A fixed price makes the native turn's cost deterministic.
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!("{config}per_request_price = 0.01\n"),
    )
    .unwrap();
    let mut tui = fixture.spawn(&["--model", "mock/chat"]);
    tui.submit("native marker");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/model");
    tui.wait_for("claude/test-model", WAIT);
    tui.send(b"\x1b[B\r");
    tui.wait_for("for the next turn", WAIT);
    tui.submit("external marker");
    fixture.wait_for_event_count("turn_end", 2);
    tui.submit("/quit");
    tui.wait_exit();
    let costs: Vec<_> = fixture
        .events()
        .iter()
        .filter(|event| event["type"] == "turn_end")
        .map(|event| event["cost"].as_f64().unwrap())
        .collect();
    assert_eq!(costs.len(), 2);
    assert!((costs[0] - 0.01).abs() < 1e-9, "{costs:?}");
    assert!((costs[1] - 0.02).abs() < 1e-9, "{costs:?}");
}

#[test]
fn claude_tree_rollback_starts_a_new_backend_session() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("tree first prompt");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("tree second prompt");
    fixture.wait_for_event_count("turn_end", 2);
    let first = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    // The journal records turn_end before the app clears its run state.
    tui.wait_idle(WAIT);
    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    // The tree hydrates asynchronously.
    tui.wait_for("tree first prompt", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(500));
    tui.send(b"\x1b[A\x1b[A\r");
    std::thread::sleep(std::time::Duration::from_millis(300));
    // A rollback restores the prompt it dropped; start a new one.
    tui.send(b"\x15");
    tui.submit("after rollback");
    fixture.wait_for_event_count("turn_end", 3);
    // A rollback rewrites the lineage, so the child context is stale.
    let second = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("<conversation_history>"), "{prompt}");
    assert!(!prompt.contains("tree second prompt"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_resume_picker_switches_backend_sessions() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("first session marker");
    fixture.wait_for_event_count("turn_end", 1);
    let first = std::fs::read_to_string(fixture.workspace.join("claude-session")).unwrap();
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&[]);
    tui.submit("second session marker");
    fixture.wait_for_event_count("turn_end", 2);
    // The journal records turn_end before the app clears its run state.
    tui.wait_idle(WAIT);
    tui.submit("/resume");
    tui.wait_for("Resume a session", WAIT);
    tui.send(b"\x1b[B\r");
    std::thread::sleep(std::time::Duration::from_millis(300));
    tui.submit("resumed marker");
    std::thread::sleep(std::time::Duration::from_millis(1500));
    fixture.wait_for_event_count("turn_end", 3);
    // The picker moved to the session that owns the first backend session.
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-resumed")).unwrap(),
        first
    );
    // The CLI already holds the first session's context, so nothing is
    // transferred and the second session's history stays out.
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(!prompt.contains("<conversation_history>"), "{prompt}");
    assert!(!prompt.contains("second session marker"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

fn configure_model_choices(fixture: &Fixture) {
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"{config}models = ["test-model", "other-model", "rejected"]
"#
        ),
    )
    .unwrap();
}

#[test]
fn external_models_switch_in_place_and_survive_resume() {
    let fixture = claude_fixture();
    configure_model_choices(&fixture);
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/model");
    tui.wait_for("other-model", WAIT);
    tui.send(b"\x1b[B\r");
    tui.wait_for("for the next turn", WAIT);
    tui.submit("again");
    fixture.wait_for_event_count("turn_end", 2);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "other-model"
    );
    let method = "claude-method";
    let initial = std::fs::read_to_string(fixture.workspace.join(method)).unwrap();
    assert_eq!(initial, "--session-id");
    tui.submit("/quit");
    tui.wait_exit();
    std::fs::remove_file(fixture.workspace.join("selected-model")).unwrap();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.submit("resume");
    fixture.wait_for_event_count("turn_end", 3);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "other-model"
    );
    let resumed = std::fs::read_to_string(fixture.workspace.join(method)).unwrap();
    assert_eq!(resumed, "--resume");
    let events = fixture.events();
    let models: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "turn_end")
        .map(|event| event["model"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(models, ["test-model", "other-model", "other-model"]);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn configured_backend_models_are_listed_without_native_credentials() {
    let fixture = claude_fixture();
    configure_model_choices(&fixture);
    let server = MockServer::start(vec![]);
    configure_native_model(&fixture, &server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        config.replace("no_auth = true", "no_auth = false"),
    )
    .unwrap();
    let output = fixture.output(&["--list-models"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let models = String::from_utf8(output.stdout).unwrap();
    for model in ["claude/test-model", "claude/other-model", "mock/chat"] {
        assert!(models.contains(model), "{models}");
    }
    let mut tui = fixture.spawn(&[]);
    tui.wait_for_screen("claude/test-model", WAIT);
    tui.submit("/model");
    tui.wait_for_screen("Switch model", WAIT);
    assert!(tui.screen_text().contains("claude/test-model"));
    assert!(tui.screen_text().contains("claude/other-model"));
    assert!(!tui.screen_text().contains("mock/chat"));
    tui.send(b"\x1b");
    tui.wait_gone("Switch model", WAIT);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn backend_model_can_be_selected_when_native_default_has_no_credentials() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![]);
    configure_native_model(&fixture, &server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            "default_model = \"mock/chat\"\n{}",
            config.replace("no_auth = true", "no_auth = false")
        ),
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);
    tui.wait_for_screen("(no model)", WAIT);
    tui.submit("/model");
    tui.wait_for_screen("claude/test-model", WAIT);
    tui.send(b"\r");
    tui.wait_for("switched to claude/test-model", WAIT);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("backend-result")).unwrap(),
        "real exec"
    );
    assert!(server.requests().is_empty());
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn backend_startup_uses_normal_model_defaults() {
    for default in [
        "default_model = \"claude/other-model\"\n",
        "default_provider = \"claude\"\n",
    ] {
        let fixture = claude_fixture();
        configure_model_choices(&fixture);
        let server = MockServer::start(vec![]);
        configure_native_model(&fixture, &server);
        let config = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(&fixture.config, format!("{default}{config}")).unwrap();
        let output = fixture.output(&["--print", "write"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = if default.starts_with("default_model") {
            "other-model"
        } else {
            "test-model"
        };
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
            expected
        );
        assert!(server.requests().is_empty());
    }
}

#[test]
fn backend_startup_honors_environment_model_override() {
    let fixture = claude_fixture();
    configure_model_choices(&fixture);
    let output = fixture.output(&[
        "-e",
        "LOFI__DEFAULT_MODEL=claude/other-model",
        "--print",
        "write",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "other-model"
    );
}

#[test]
fn backend_without_initial_model_selects_first_configured_model() {
    let fixture = claude_fixture();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            "{}models = [\"sonnet\", \"opus\"]\n",
            config.replace("model = \"test-model\"\n", "")
        ),
    )
    .unwrap();
    let output = fixture.output(&["--print", "write"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "sonnet"
    );
}

#[test]
fn external_model_flag_selects_configured_model_and_rejection_stops_prompt() {
    let fixture = claude_fixture();
    configure_model_choices(&fixture);
    let output = fixture.output(&["--model", "claude/rejected", "--print", "write"]);
    assert!(!output.status.success());
    assert!(!fixture.workspace.join("backend-result").exists());
    let output = fixture.output(&["--model", "claude/other-model", "--print", "write"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "other-model"
    );
    assert!(fixture.workspace.join("backend-result").exists());
}

#[test]
fn external_model_rejection_does_not_send_the_next_prompt() {
    let fixture = claude_fixture();
    configure_model_choices(&fixture);
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    std::fs::remove_file(fixture.workspace.join("backend-result")).unwrap();
    tui.submit("/model");
    tui.wait_for("rejected", WAIT);
    tui.send(b"\x1b[B\x1b[B\r");
    tui.wait_for("for the next turn", WAIT);
    tui.submit("write");
    fixture.wait_for_event_count("turn_failed", 1);
    assert!(!fixture.workspace.join("backend-result").exists());
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_failed_init_rebuilds_the_prompt_for_a_later_shell() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    // The CLI reports another tool, so Lofi rejects init and the child never
    // takes a prompt. The next turn must rebuild the prompt from the lineage
    // instead of resuming a session the CLI never created.
    tui.submit("extra tool");
    fixture.wait_for_event_count("turn_failed", 1);
    tui.submit("!printf first-shell-marker");
    fixture.wait_for_event_count("user_shell", 1);
    tui.submit("!printf second-shell-marker");
    fixture.wait_for_event_count("user_shell", 2);
    tui.submit("after failure");
    fixture.wait_for_event_count("turn_end", 1);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("<conversation_history>"), "{prompt}");
    assert!(prompt.contains("first-shell-marker"), "{prompt}");
    assert!(prompt.contains("second-shell-marker"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_failed_turn_rebuilds_the_prompt_for_a_later_shell() {
    let fixture = claude_fixture();
    configure_model_choices(&fixture);
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    // The shell output travels as a delta. A failed turn hides the events
    // older than itself from the projection, so the delta cannot be trusted
    // and the next turn rebuilds the prompt from the lineage.
    tui.submit("!printf first-shell-marker");
    fixture.wait_for_event_count("user_shell", 1);
    tui.submit("/model");
    tui.wait_for("Switch model", WAIT);
    // The picker starts on the current model, so two rows down select the
    // model the CLI rejects.
    tui.send(b"\x1b[B\x1b[B\r");
    tui.submit("write");
    fixture.wait_for_event_count("turn_failed", 1);
    tui.submit("!printf second-shell-marker");
    fixture.wait_for_event_count("user_shell", 2);
    tui.submit("/model");
    tui.wait_for("Switch model", WAIT);
    tui.send(b"\x1b[A\r");
    tui.submit("after failure");
    fixture.wait_for_event_count("turn_end", 2);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("<conversation_history>"), "{prompt}");
    assert!(prompt.contains("first-shell-marker"), "{prompt}");
    assert!(prompt.contains("second-shell-marker"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_failed_turn_keeps_its_completed_tool_cycle_on_resume() {
    let fixture = claude_fixture();
    let mut first = fixture.spawn(&[]);
    first.submit("fail after tool");
    fixture.wait_for_event_count("turn_failed", 1);
    first.submit("/quit");
    first.wait_exit();
    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("after failure");
    fixture.wait_for_event_count("turn_end", 1);
    // Replay keeps the failed turn's completed tool cycle, so the child the
    // resumed process opens receives it in the transfer.
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("<conversation_history>"), "{prompt}");
    assert!(prompt.contains("fail after tool"), "{prompt}");
    assert!(prompt.contains("real exec"), "{prompt}");
    resumed.submit("/quit");
    resumed.wait_exit();
}

#[test]
fn external_context_without_backend_reports_uses_the_model_window() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.wait_for("context 0/200k", WAIT);
    tui.submit("no usage");
    fixture.wait_for_event_count("turn_end", 1);
    tui.wait_for("context 0/200k", WAIT);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_jobs_use_the_session_registry() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("job");
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"a");
    tui.wait_for("claude answer", WAIT);
    fixture.wait_for_event_count("job_started", 1);
    fixture.wait_for_event_count("job_finished", 1);
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn claude_sessions_keep_workspaces_separate() {
    let first = claude_fixture();
    let second = claude_fixture();
    assert!(first.output(&["--print", "write"]).status.success());
    assert!(!second.workspace.join("backend-result").exists());
    assert!(second.output(&["--print", "write"]).status.success());
    assert!(first.workspace.join("backend-result").exists());
    assert!(second.workspace.join("backend-result").exists());
}

#[test]
fn acp_configuration_is_rejected_before_starting_a_process() {
    let fixture = Fixture::without_models();
    std::fs::write(
        &fixture.config,
        r#"default_model = "external/default"
[backends.external]
type = "acp"
command = "sh"
args = ["-c", "touch adapter-started"]
"#,
    )
    .unwrap();
    let output = fixture.output(&["--print", "write"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown variant `acp`"));
    assert!(!fixture.workspace.join("adapter-started").exists());
    assert!(!fixture.workspace.join("backend-result").exists());
}

fn configure_native_model(fixture: &Fixture, server: &MockServer) {
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"{config}
[providers.mock]
api_type = "openai-completions"
base_url = "{}"
no_auth = true
[providers.mock.models.chat]
"#,
            server.url()
        ),
    )
    .unwrap();
}

#[test]
fn model_picker_switches_backends_with_shared_history() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![
        text_response("native answer marker"),
        text_response("native second marker"),
    ]);
    configure_native_model(&fixture, &server);
    let mut tui = fixture.spawn(&["--model", "mock/chat"]);
    tui.submit("native prompt marker");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/model");
    tui.wait_for("claude/test-model", WAIT);
    tui.send(b"\x1b[B\r");
    tui.wait_for("for the next turn", WAIT);
    tui.submit("external prompt marker");
    fixture.wait_for_event_count("turn_end", 2);
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("native prompt marker"));
    assert!(prompt.contains("native answer marker"));
    assert!(prompt.contains("external prompt marker"));
    tui.submit("/model");
    tui.wait_for("mock/chat", WAIT);
    tui.send(b"\x1b[A\r");
    tui.wait_for("switched to mock/chat", WAIT);
    tui.submit("native followup marker");
    fixture.wait_for_event_count("turn_end", 3);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("native prompt marker"));
    assert!(requests[1].body.contains("external prompt marker"));
    assert!(requests[1].body.contains("claude answer"));
    assert!(requests[1].body.contains("real exec"));
    let request: Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = request["messages"].as_array().unwrap();
    let call_id = messages
        .iter()
        .find_map(|message| message["tool_calls"][0]["id"].as_str())
        .unwrap();
    assert!(
        messages.iter().any(|message| {
            message["role"] == "tool"
                && message["tool_call_id"] == call_id
                && message["content"]
                    .as_str()
                    .is_some_and(|text| text.contains("real exec"))
        }),
        "missing tool result: {request}"
    );
    tui.submit("/model");
    tui.wait_for("claude/test-model", WAIT);
    tui.send(b"\x1b[B\r");
    tui.wait_for("for the next turn", WAIT);
    tui.submit("external followup marker");
    fixture.wait_for_event_count("turn_end", 4);
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("native followup marker"));
    assert!(prompt.contains("native second marker"));
    assert!(!prompt.contains("claude answer"));
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.submit("resumed external marker");
    fixture.wait_for_event_count("turn_end", 5);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--resume"
    );
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn external_start_can_switch_to_native_and_resume_native() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![
        text_response("native answer marker"),
        text_response("native resumed marker"),
    ]);
    configure_native_model(&fixture, &server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!("default_model = \"claude/test-model\"\n{config}"),
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);
    tui.submit("external first marker");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/model");
    tui.wait_for("mock/chat", WAIT);
    tui.send(b"\x1b[A\r");
    tui.wait_for("switched to mock/chat", WAIT);
    tui.submit("native prompt marker");
    fixture.wait_for_event_count("turn_end", 2);
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.wait_for_screen("mock/chat", WAIT);
    tui.submit("native resumed prompt");
    fixture.wait_for_event_count("turn_end", 3);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body.contains("external first marker"));
    assert!(requests[0].body.contains("claude answer"));
    assert!(requests[1].body.contains("external first marker"));
    assert!(requests[1].body.contains("native answer marker"));
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn external_default_model_resets_in_place_and_survives_resume() {
    let fixture = claude_fixture();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!("{config}models = [\"test-model\", \"default\"]\n"),
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/model");
    tui.wait_for("claude/default", WAIT);
    tui.send(b"\x1b[B\r");
    tui.wait_for("for the next turn", WAIT);
    tui.submit("again");
    fixture.wait_for_event_count("turn_end", 2);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "default"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    tui.submit("/quit");
    tui.wait_exit();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.submit("resumed default");
    fixture.wait_for_event_count("turn_end", 3);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("selected-model")).unwrap(),
        "default"
    );
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn external_shell_results_reach_the_next_prompt() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("write");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("!printf shell-history-marker");
    fixture.wait_for_event_count("user_shell", 1);
    tui.submit("after shell");
    fixture.wait_for_event_count("turn_end", 2);
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("shell-history-marker"), "{prompt}");
    assert!(!prompt.contains("claude answer"), "{prompt}");
    tui.submit("/quit");
    tui.wait_exit();
}

#[test]
fn external_backend_rejects_unknown_models() {
    let fixture = claude_fixture();
    let output = fixture.output(&["--model", "claude/unused-model", "--print", "write"]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("no model \"unused-model\" for backend")
    );
}

#[test]
fn corrupt_backend_binding_falls_back_to_history_transfer() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("first prompt marker");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/quit");
    tui.wait_exit();
    let binding = fixture.session_files()[0].with_extension("backend.json");
    std::fs::write(&binding, "{truncated").unwrap();
    let mut tui = fixture.spawn(&["--continue"]);
    tui.submit("second prompt marker");
    fixture.wait_for_event_count("turn_end", 2);
    tui.submit("/quit");
    tui.wait_exit();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("claude-method")).unwrap(),
        "--session-id"
    );
    let prompt = std::fs::read_to_string(fixture.workspace.join("claude-prompt")).unwrap();
    assert!(prompt.contains("first prompt marker"), "{prompt}");
    let rewritten: Value = serde_json::from_slice(&std::fs::read(&binding).unwrap()).unwrap();
    assert_eq!(rewritten["backend"], "claude");
}
#[test]
fn claude_backend_runs_a_subagent() {
    let fixture = claude_fixture();
    let output = fixture.output(&["--print", "subagent"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = std::fs::read_to_string(fixture.workspace.join("subagent-result")).unwrap();
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["status"], "completed", "{body}");
    assert_eq!(body["result"], "claude answer", "{body}");
}

#[test]
fn claude_records_the_spend_its_child_used() {
    let fixture = claude_fixture();
    let mut tui = fixture.spawn(&[]);
    tui.submit("subagent");
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/quit");
    tui.wait_exit();
    // The provider path reports child spend per round; the CLI path has no
    // rounds, so it reports it when the turn settles. Either way the session
    // records it, and the gauge folds it.
    let recorded: Vec<_> = fixture
        .events()
        .into_iter()
        .filter(|event| event["type"] == "subagent_usage")
        .collect();
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert!(
        recorded[0]["usage"]["output_tokens"].as_u64().unwrap() > 0,
        "{recorded:?}"
    );
}

#[test]
fn claude_backend_spawns_an_http_child() {
    let fixture = claude_fixture();
    let server = MockServer::start(vec![text_response("http child answer")]);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"default_model = "claude/test-model"
{config}
[providers.mock]
api_type = "openai-completions"
base_url = "{}"
no_auth = true

[providers.mock.models.child]
"#,
            server.url()
        ),
    )
    .unwrap();
    let output = fixture.output(&["--print", "subagent override"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body = std::fs::read_to_string(fixture.workspace.join("subagent-result")).unwrap();
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["status"], "completed", "{body}");
    assert_eq!(body["result"], "http child answer", "{body}");
    assert_eq!(server.requests().len(), 1, "the child must run on the mock");
}
