use serde_json::Value;

use crate::support::{text_response, transcript_text, Fixture, MockServer, WAIT};

#[test]
fn direct_shell_context_marker_controls_the_next_model_request() {
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
