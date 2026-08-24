use nix::sys::signal::Signal;
use serde_json::Value;

use crate::support::{
    delayed_text_response, delayed_tool_response, process_is_alive, spawned_pid, text_response,
    tool_response, transcript_text, wait_for_process_exit, Fixture, MockServer, ProcessGuard, WAIT,
};

#[test]
fn user_shell_context_marker_controls_the_next_model_request() {
    let server = MockServer::start(vec![text_response("shell context answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!!printf omitted-shell-marker");
    tui.wait_for("omitted-shell-marker", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(100));
    tui.submit("!printf included-shell-marker");
    tui.wait_for("included-shell-marker", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(100));
    tui.submit("shell context prompt marker");
    tui.wait_for("shell context answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("included-shell-marker"));
    assert!(!requests[0].body.contains("omitted-shell-marker"));
    let transcript = transcript_text(&fixture.events());
    assert!(transcript.contains("included-shell-marker"));
    assert!(transcript.contains("omitted-shell-marker"));
}

#[test]
fn slash_commands_autocomplete_and_information_modals_work() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.send(b"/he\t\r");
    tui.wait_for("Keys", WAIT);
    tui.send(b"\x1b");
    std::thread::sleep(std::time::Duration::from_millis(100));

    tui.clear_output();
    tui.submit("/session");
    tui.wait_for("Workspace", WAIT);
    tui.send(b"\x1b");
    std::thread::sleep(std::time::Duration::from_millis(100));

    tui.clear_output();
    tui.submit("/theme");
    tui.wait_for("Color scheme", WAIT);
    tui.send(b"\x1b[B\r");

    tui.clear_output();
    tui.submit("/unknown-e2e-command");
    tui.wait_for("unknown command", WAIT);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn topmost_policy_confirmation_handles_input_before_tree_picker() {
    let server = MockServer::start(vec![
        text_response("overlap seed answer"),
        delayed_tool_response(
            "overlap-policy-call",
            "return await lofi.bash({ cmd: \"touch should-not-run\" });",
            std::time::Duration::from_millis(500),
        ),
        text_response("overlap policy denied answer"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("overlap seed prompt");
    tui.wait_for("overlap seed answer", WAIT);
    tui.submit("request a command that needs confirmation");
    let started = std::time::Instant::now();
    while server.request_count() < 2 && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(server.request_count(), 2);

    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"da");
    let started = std::time::Instant::now();
    while server.request_count() < 3 && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(server.request_count(), 3);
    tui.send(b"\x1b");
    tui.wait_for("overlap policy denied answer", WAIT);
    assert!(!fixture.workspace.join("should-not-run").exists());
}

#[test]
fn policy_dialog_deny_all_blocks_exec_without_prompting() {
    let server = MockServer::start(vec![
        tool_response(
            "policy-call",
            "return await lofi.bash({ cmd: \"touch should-not-run\" });",
        ),
        text_response("policy deny all answer"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("/policy");
    tui.wait_for("Bash policy", WAIT);
    // Without auto mode the rows are allow all / ask (manual) / deny all.
    tui.send(b"\x1b[B\x1b[B\r");
    tui.wait_for("policy: deny all", WAIT);

    tui.submit("run the touch please");
    tui.wait_for("policy deny all answer", WAIT);

    // No prompt appeared: the override blocked the exec outright and the
    // denial result went back to the model.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].body.contains("deny all"),
        "{}",
        requests[1].body
    );
    assert!(!fixture.workspace.join("should-not-run").exists());
}

#[test]
fn policy_dialog_allow_all_runs_exec_without_prompting() {
    let server = MockServer::start(vec![
        tool_response(
            "policy-call",
            "return await lofi.bash({ cmd: \"touch should-run\" });",
        ),
        text_response("policy allow all answer"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("/policy");
    tui.wait_for("Bash policy", WAIT);
    // The dialog opens pre-selected on ask (manual); one up selects allow all.
    tui.send(b"\x1b[A\r");
    tui.wait_for("policy: allow all", WAIT);

    tui.submit("run the touch please");
    tui.wait_for("policy allow all answer", WAIT);

    // Under confirm policy this exec would have prompted; allow-all skipped
    // the prompt and ran it.
    assert_eq!(server.requests().len(), 2);
    assert!(fixture.workspace.join("should-run").exists());
}

#[test]
fn model_and_thinking_pickers_change_the_next_request() {
    let server = MockServer::start(vec![text_response("picker answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("/model");
    tui.wait_for("Switch model", WAIT);
    tui.send(b"\x1b[A\r");
    tui.wait_for("switched to mock/alt", WAIT);

    tui.submit("/thinking");
    tui.wait_for("Thinking level", WAIT);
    tui.send(b"\x1b[B\r");
    tui.submit("picker prompt marker");
    tui.wait_for("picker answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(request["model"], "alt");
    assert_eq!(request["reasoning_effort"], "high");
}

#[test]
fn service_picker_changes_the_next_request() {
    let server = MockServer::start(vec![text_response("service answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("/service");
    tui.wait_for("Service tier", WAIT);
    // Rows are auto / flex / priority, pre-selected at the model's flex.
    tui.send(b"\x1b[B\r");
    tui.wait_for("switched to mock/chat:medium@priority", WAIT);

    tui.submit("service prompt marker");
    tui.wait_for("service answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(request["service_tier"], "priority");
}

#[test]
fn mid_run_model_switch_keeps_the_working_line_model() {
    let server = MockServer::start(vec![delayed_text_response(
        "mid-run answer marker",
        std::time::Duration::from_secs(2),
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("mid-run switch marker");
    tui.wait_for("Working for", WAIT);
    tui.submit("/model");
    tui.wait_for("Switch model", WAIT);
    tui.send(b"\x1b[A\r");
    tui.wait_for("switched to mock/alt", WAIT);

    let working = tui.screen_row("Working for").expect("working row");
    assert!(
        working.contains("mock/chat"),
        "working line must keep the run model: {working}"
    );
    assert!(
        !working.contains("alt"),
        "working line must not show the newly set model: {working}"
    );

    tui.wait_for("mid-run answer marker", WAIT);
}

#[test]
fn no_model_mode_launches_and_rejects_prompts_without_a_request() {
    let fixture = Fixture::without_models();
    let mut tui = fixture.spawn(&[]);

    tui.wait_for("No models configured", WAIT);
    tui.clear_output();
    tui.submit("prompt with no model");
    tui.wait_for("No models configured", WAIT);
    assert!(fixture.session_files().is_empty());
}

#[test]
fn no_session_mode_runs_without_writing_a_transcript() {
    let server = MockServer::start(vec![text_response("ephemeral answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--no-session"]);

    tui.submit("ephemeral prompt marker");
    tui.wait_for("ephemeral answer marker", WAIT);
    tui.submit("/session");
    tui.wait_for("No session file", WAIT);
    assert!(fixture.session_files().is_empty());
}

#[test]
fn diagnostics_verbose_recall_clear_and_exit_commands_work() {
    let server = MockServer::start(vec![
        text_response("command history answer marker"),
        text_response("command history follow-up answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("command history prompt marker");
    tui.wait_for("command history answer marker", WAIT);
    tui.clear_output();
    tui.submit("/debug");
    tui.wait_for("Debug mode activated", WAIT);
    // The debug line and its status column contribution.
    tui.wait_for("Debug", WAIT);
    tui.wait_for("Peak", WAIT);
    tui.wait_for("Measured", WAIT);
    tui.submit("/verbose");
    tui.wait_for(" verbose ", WAIT);

    tui.clear_output();
    tui.submit("/recall command history prompt marker");
    tui.wait_for("matches", WAIT);
    tui.wait_for("command history prompt marker", WAIT);

    tui.submit("/clear");
    tui.submit("command history follow-up prompt");
    tui.wait_for("command history follow-up answer", WAIT);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("command history prompt marker"));
    assert!(requests[1].body.contains("command history answer marker"));
    assert!(requests[1]
        .body
        .contains("command history follow-up prompt"));

    tui.submit("/exit");
    tui.wait_exit();
}

#[test]
fn jobs_modal_lists_opens_output_and_stops_a_running_job() {
    let server = MockServer::start(vec![
        tool_response(
            "modal-job-call",
            r#"return await lofi.jobSpawn({ cmd: "printf 'raw-marker\\033[2K\\rmodal-job-screen-marker'; sleep 60", tty: true, cols: 36, rows: 8, notify: false });"#,
        ),
        text_response("modal job answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("start modal job");
    tui.wait_for("modal job answer marker", WAIT);
    let mut job = ProcessGuard::new(spawned_pid(&fixture));
    assert!(process_is_alive(job.pid()));

    tui.clear_output();
    tui.submit("/job");
    tui.wait_for("background jobs", WAIT);
    tui.send(b"\r");
    tui.wait_for("modal-job-screen-marker", WAIT);
    tui.clear_output();
    tui.send(b"\x1b");
    tui.wait_for("x", WAIT);
    tui.clear_output();
    tui.send(b"x");
    tui.wait_for("this job?", WAIT);
    tui.send(b"y");
    wait_for_process_exit(job.pid());
    job.disarm();
}

#[test]
fn prompts_submitted_during_a_run_are_queued_and_sent_in_order() {
    let server = MockServer::start(vec![
        delayed_text_response(
            "queued first answer marker",
            std::time::Duration::from_millis(250),
        ),
        text_response("queued second answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("queued first prompt marker");
    std::thread::sleep(std::time::Duration::from_millis(50));
    tui.submit("queued second prompt marker");
    tui.wait_for("queued second answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body.contains("queued first prompt marker"));
    assert!(!requests[0].body.contains("queued second prompt marker"));
    assert!(requests[1].body.contains("queued first answer marker"));
    assert!(requests[1].body.contains("queued second prompt marker"));
}

#[test]
fn bracketed_multiline_paste_is_submitted_as_one_prompt() {
    let server = MockServer::start(vec![text_response("paste answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.send(b"\x1b[200~paste line one\npaste line two\x1b[201~\r");
    tui.wait_for("paste answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("paste line one\\npaste line two"));
}

#[test]
fn selecting_rendered_text_copies_the_original_markdown_over_osc52() {
    let server = MockServer::start(vec![text_response("**copy-markdown-marker**")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("render markdown for copy");
    tui.wait_for("copy-markdown-marker", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    tui.clear_output();

    tui.send(b"\tkk0v$y");
    tui.wait_for("Copied to clipboard", WAIT);

    let output = tui.output();
    assert!(
        output.contains("\x1b]52;c;Kipjb3B5LW1hcmtkb3duLW1hcmtlcioq\x07"),
        "terminal output: {output:?}"
    );
}

#[test]
fn escape_clears_input_and_ctrl_d_exits() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.send(b"input that escape must clear");
    std::thread::sleep(std::time::Duration::from_millis(100));
    tui.send(b"\x1b");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(server.request_count(), 0);
    tui.send(b"\x04");
    tui.wait_exit();
}

#[test]
fn ctrl_c_cancels_user_shell_and_kills_its_process_group() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!echo $$ > user-shell.pid; exec sleep 60");
    let pid_path = fixture.workspace.join("user-shell.pid");
    let started = std::time::Instant::now();
    while !pid_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut guard = ProcessGuard::new(pid);
    assert!(process_is_alive(pid));

    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
    wait_for_process_exit(pid);
    guard.disarm();

    let events = fixture.events();
    let bash = events
        .iter()
        .find(|event| event["type"] == "user_shell")
        .unwrap();
    assert_eq!(bash["cancelled"], true);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn user_shell_records_exit_signal_and_large_output_without_blocking_shutdown() {
    let server = MockServer::start(vec![text_response("shell edge context answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!printf nonzero-shell-marker; exit 23");
    tui.wait_for("Exit 23", WAIT);
    tui.submit("!kill -TERM $$");
    tui.wait_for("Signal 15", WAIT);
    tui.submit("!i=0; while [ $i -lt 8000 ]; do printf 'large-shell-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' $i; i=$((i+1)); done");
    tui.wait_for("· truncated", WAIT);
    tui.submit("shell edge context prompt");
    tui.wait_for("shell edge context answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("nonzero-shell-marker"));
    assert!(requests[0].body.contains("Command exited with code 23"));
    assert!(requests[0].body.contains("Command terminated by signal 15"));
    assert!(requests[0].body.contains("Output truncated"));
    assert!(!requests[0].body.contains("large-shell-0000"));

    tui.submit("!echo $$ > shutdown-shell.pid; printf shutdown-stream-start; sleep 60");
    let pid_path = fixture.workspace.join("shutdown-shell.pid");
    let started = std::time::Instant::now();
    while !pid_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid: i32 = std::fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut guard = ProcessGuard::new(pid);
    tui.send(b"\x04");
    tui.wait_exit();
    wait_for_process_exit(pid);
    guard.disarm();
}

#[test]
fn accepting_auto_mode_dialog_keeps_the_draft_in_the_input_box() {
    let server = MockServer::start(vec![
        tool_response(
            "auto-draft-call",
            r#"return await lofi.bash({ cmd: "printf auto-draft-ran" });"#,
        ),
        delayed_text_response(
            r#"{"decision":"ask","reason":"fixture asks"}"#,
            std::time::Duration::from_millis(400),
        ),
        text_response("auto draft final answer"),
    ]);
    let fixture = Fixture::new(&server);
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        r#"mode = "confirm"

[auto_mode]
enable = true
provider = "mock"
model = "alt"
max_tokens = 128
"#,
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("run the auto draft tool");
    tui.wait_for("Working for", WAIT);
    // Type a draft while the run and the auto-mode evaluation are in flight.
    tui.send(b"DRAFT-NOT-A-PROMPT");
    tui.wait_for("Permission Required", WAIT);
    tui.wait_for("Auto evaluation asks", WAIT);

    tui.send(b"\r");
    tui.wait_for("auto draft final answer", WAIT);

    let row = tui
        .screen_row("DRAFT-NOT-A-PROMPT")
        .expect("draft must remain in the input box");
    assert!(row.contains('▌'), "draft row is not the input area: {row}");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body.contains("run the auto draft tool"));
    assert!(!requests[2].body.contains("DRAFT-NOT-A-PROMPT"));
    assert!(fixture
        .events()
        .iter()
        .all(|event| !event.to_string().contains("DRAFT-NOT-A-PROMPT")));
}

#[test]
fn user_shell_reading_stdin_does_not_swallow_typed_keys() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    let script = fixture.workspace.join("probe.sh");
    std::fs::write(
        &script,
        "i=0\nwhile [ $i -lt 120 ] && [ ! -e probe-got-ab ]; do sleep 0.1; read -r -t 0.05 line && printf '%s' \"$line\" > probe-got-ab; i=$((i+1)); done\nexit 0\n",
    )
    .unwrap();
    tui.submit("!sh probe.sh");
    tui.wait_for("Working for", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(700));

    tui.send(b"ZZPROBE-MARKER-ZZ\r");
    tui.send(b"ab\r");
    let got_path = fixture.workspace.join("probe-got-ab");
    let started = std::time::Instant::now();
    while !got_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !got_path.exists(),
        "the !-shell must not read the TUI's terminal stdin"
    );
    tui.wait_for("Exit 0", WAIT);
    tui.wait_for("ZZPROBE-MARKER-ZZ", WAIT);
}

#[test]
fn no_session_recall_and_result_report_that_persistence_is_unavailable() {
    let server = MockServer::start(vec![
        tool_response(
            "ephemeral-recovery",
            r#"const recall = await lofi.recall({ query: "anything", scope: "all" });
const result = await lofi.result("missing-event");
return { recall, result };"#,
        ),
        text_response("ephemeral recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--no-session"]);

    tui.submit("try recovery without a session");
    tui.wait_for("ephemeral recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("recall unavailable"));
    assert!(requests[1].body.contains("result unavailable"));
    assert!(fixture.session_files().is_empty());
}

#[test]
fn sigterm_leaves_the_terminal_ready_for_the_next_process() {
    let server = MockServer::start(vec![text_response("signal terminal answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("signal terminal prompt");
    tui.wait_for("signal terminal answer", WAIT);
    tui.signal(Signal::SIGTERM);
    tui.wait_exit();

    let next = fixture.output(&["--list-models"]);
    assert!(next.status.success());
    assert!(String::from_utf8_lossy(&next.stdout).contains("mock/chat"));
}

#[test]
fn resize_reflows_the_transcript_without_losing_the_latest_answer() {
    let server = MockServer::start(vec![text_response("resize reflow answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("resize reflow prompt");
    tui.wait_for("resize reflow answer", WAIT);
    tui.resize(24, 80);
    tui.wait_for("resize reflow answer", WAIT);
    tui.send(b"\x04");
    tui.wait_exit();
}
