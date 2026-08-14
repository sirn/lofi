use crate::support::{text_response, thinking_response, tool_response, Fixture, MockServer};

#[test]
fn informational_cli_commands_run_without_a_tty() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);

    let docs = fixture.output(&["--docs"]);
    assert!(docs.status.success());
    assert!(String::from_utf8_lossy(&docs.stdout).contains("lofi.read"));

    let search = fixture.output(&["--docs-search", "background job"]);
    assert!(search.status.success());
    assert!(String::from_utf8_lossy(&search.stdout).contains("lofi.jobSpawn"));

    let no_matches = fixture.output(&["--docs-search", "definitely-no-such-api-entry"]);
    assert!(no_matches.status.success());
    assert_eq!(
        String::from_utf8_lossy(&no_matches.stdout),
        "(no matches)\n"
    );

    let policy = fixture.output(&["--policy-explain", "printf safe"]);
    assert!(policy.status.success());
    assert!(String::from_utf8_lossy(&policy.stdout).contains("action: allow"));

    let models = fixture.output(&["--list-models"]);
    assert!(models.status.success());
    let models = String::from_utf8_lossy(&models.stdout);
    assert!(models.contains("mock/chat"));
    assert!(models.contains("responses/reasoning"));

    let sessions = fixture.output(&["--list-sessions"]);
    assert!(sessions.status.success());
    assert_eq!(
        String::from_utf8_lossy(&sessions.stdout),
        "(no sessions)
"
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn print_mode_streams_text_and_reports_tool_calls_on_stderr() {
    let server = MockServer::start(vec![
        tool_response("print-tool", "return { answer: 42 };"),
        thinking_response("private reasoning", "print answer marker"),
    ]);
    let fixture = Fixture::new(&server);

    let output = fixture.output(&["--print", "print prompt marker"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "print answer marker
"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("[exec]"));
    assert!(stderr.contains("return { answer: 42 };"));
    assert!(!stderr.contains("private reasoning"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0].body.contains("print prompt marker"),
        "captured request: {:?}",
        requests[0]
    );
    assert!(requests[1].body.contains("answer"));
    assert!(requests[1].body.contains("42"));
    assert!(fixture.session_files().is_empty());
}

#[test]
fn print_mode_returns_nonzero_for_provider_and_model_errors() {
    let server = MockServer::start(vec![crate::support::MockResponse::error(
        401,
        "invalid test credential",
    )]);
    let fixture = Fixture::new(&server);

    let provider = fixture.output(&["--print", "provider error prompt"]);
    assert!(!provider.status.success());
    assert!(String::from_utf8_lossy(&provider.stderr).contains("invalid test credential"));

    let model = fixture.output(&["--model", "mock/missing", "--print", "model error prompt"]);
    assert!(!model.status.success());
    assert!(String::from_utf8_lossy(&model.stderr).contains("missing"));
}

#[test]
fn list_sessions_reports_a_completed_tui_session() {
    let server = MockServer::start(vec![text_response("listed session answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);
    tui.submit("listed session prompt");
    tui.wait_for("listed session answer", crate::support::WAIT);
    tui.submit("/quit");
    tui.wait_exit();

    let sessions = fixture.output(&["--list-sessions"]);
    assert!(sessions.status.success());
    let stdout = String::from_utf8_lossy(&sessions.stdout);
    assert!(stdout.contains("mock/chat"));
    assert!(stdout.contains("msgs"));
}

#[test]
fn invalid_resume_id_fails_before_starting_the_tui() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);

    let output = fixture.output(&["--resume", "missing-session-id"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing-session-id"));
    assert_eq!(server.request_count(), 0);
}
