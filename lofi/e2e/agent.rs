use crate::support::{
    event_types, responses_response, text_response, tool_response, transcript_text,
    wait_for_process_exit, Fixture, MockResponse, MockServer, ProcessGuard, WAIT,
};

#[test]
fn openai_responses_streams_reasoning_text_and_usage_through_the_tui() {
    let server = MockServer::start(vec![responses_response(
        "responses thinking marker",
        "responses answer marker",
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning:high"]);

    tui.submit("responses prompt marker");
    tui.wait_for("responses answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].path.ends_with("/responses"));
    assert!(requests[0].body.contains("responses prompt marker"));
    assert!(requests[0].body.contains("high"));

    let transcript = transcript_text(&fixture.events());
    assert!(transcript.contains("responses thinking marker"));
    assert!(transcript.contains("responses answer marker"));
    assert!(transcript.contains("cache_read_tokens"));
}

#[test]
fn transient_provider_failure_retries_then_completes() {
    let server = MockServer::start(vec![
        MockResponse::error(503, "overloaded retry marker"),
        text_response("retry answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("retry prompt marker");
    tui.wait_for("retry answer marker", WAIT);

    assert_eq!(server.request_count(), 2);
    let events = fixture.events();
    assert!(event_types(&events).contains(&"turn_end"));
    assert!(!event_types(&events).contains(&"turn_failed"));
}

#[test]
fn tool_failure_is_returned_to_the_model_and_the_turn_recovers() {
    let server = MockServer::start(vec![
        tool_response("failure-tool", "throw new Error(\"tool failure marker\");"),
        text_response("tool recovery answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("tool failure prompt marker");
    tui.wait_for("tool recovery answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("tool failure marker"));
    assert!(requests[1].body.contains("error"));
    assert!(transcript_text(&fixture.events()).contains("tool failure marker"));
}

#[test]
fn permission_allow_executes_the_requested_shell_command() {
    let server = MockServer::start(vec![
        tool_response(
            "allow-tool",
            "return await lofi.bash({ cmd: \"printf permission-allowed > permission.txt\" });",
        ),
        text_response("permission allow answer marker"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("permission allow prompt marker");
    tui.wait_for("Allow", WAIT);
    tui.send(b"a");
    tui.wait_for("permission allow answer marker", WAIT);

    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("permission.txt")).unwrap(),
        "permission-allowed"
    );
    assert_eq!(server.request_count(), 2);
}

#[test]
fn permission_deny_returns_an_error_without_running_the_command() {
    let server = MockServer::start(vec![
        tool_response(
            "deny-tool",
            "return await lofi.bash({ cmd: \"printf should-not-exist > denied.txt\" });",
        ),
        text_response("permission deny answer marker"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("permission deny prompt marker");
    tui.wait_for("Deny", WAIT);
    tui.send(b"d");
    tui.wait_for("permission deny answer marker", WAIT);

    assert!(!fixture.workspace.join("denied.txt").exists());
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("denied"));
}

#[test]
fn cancelling_a_running_tool_kills_its_process_and_records_cancellation() {
    let server = MockServer::start(vec![tool_response(
        "cancel-tool",
        "return await lofi.bash({ cmd: \"echo $$ > foreground.pid; exec sleep 60\" });",
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("cancel tool prompt marker");
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"a");
    let pid_path = fixture.workspace.join("foreground.pid");
    let start = std::time::Instant::now();
    while !pid_path.exists() && start.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        pid_path.exists(),
        "foreground command did not start; terminal output: {}",
        tui.output()
    );
    let pid = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let mut process = ProcessGuard::new(pid);
    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
    wait_for_process_exit(pid);
    process.disarm();

    assert_eq!(server.request_count(), 1);
    let events = fixture.events();
    assert!(event_types(&events).contains(&"turn_cancelled"));
}
