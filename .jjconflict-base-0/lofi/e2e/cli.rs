use std::os::unix::process::ExitStatusExt;
use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::support::{
    delayed_text_response, text_response, thinking_response, thinking_tool_response, tool_response,
    Fixture, MockServer, WAIT,
};

#[test]
fn help_version_and_dispatch_precedence_are_stable() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);

    let help = fixture.output(&["--help"]);
    assert!(help.status.success());
    let help = String::from_utf8_lossy(&help.stdout);
    for flag in [
        "--print",
        "--env",
        "--list-models",
        "--continue",
        "--resume",
        "--no-session",
    ] {
        assert!(help.contains(flag), "help omitted {flag}: {help}");
    }

    let version = fixture.output(&["--version"]);
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        format!("lofi {}\n", env!("CARGO_PKG_VERSION"))
    );

    let precedence = fixture.output(&[
        "--list-models",
        "--list-sessions",
        "--docs",
        "--print",
        "must not reach the provider",
    ]);
    assert!(precedence.status.success());
    assert!(String::from_utf8_lossy(&precedence.stdout).contains("mock/chat"));
    assert_eq!(server.request_count(), 0);
}

#[test]
fn malformed_configuration_fails_before_network_or_tui_startup() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    std::fs::write(&fixture.config, "this is not = valid toml [").unwrap();

    let output = fixture.output(&["--list-models"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    assert!(
        stderr.contains("config") || stderr.contains("toml"),
        "{stderr}"
    );
    assert_eq!(server.request_count(), 0);
}

#[test]
fn bootstrap_preserves_malloc_setting_and_accepts_rust_log() {
    let server = MockServer::start(vec![
        tool_response(
            "bootstrap-env",
            r#"const value = await lofi.bash({ cmd: "printf %s \"$MALLOC_ARENA_MAX\"" });
await lofi.write({ path: "malloc-arena-value", text: value.output });
return value;"#,
        ),
        text_response("bootstrap environment answer"),
    ]);
    let fixture = Fixture::new(&server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!("[bash]\nstrip_env = false\n\n{config}"),
    )
    .unwrap();

    let output = fixture.output_with_env(
        &["--print", "inspect bootstrap environment"],
        &[("MALLOC_ARENA_MAX", "7"), ("RUST_LOG", "lofi=debug")],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let path = fixture.workspace.join("malloc-arena-value");
    assert!(
        path.exists(),
        "stdout={} stderr={} requests={:?}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        server.requests(),
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "7");
}

#[test]
fn signal_during_print_mode_returns_the_signal_status() {
    let server = MockServer::start(vec![delayed_text_response(
        "signal response must not finish",
        Duration::from_secs(5),
    )]);
    let fixture = Fixture::new(&server);
    let mut command = fixture.command(&["--print", "wait for signal"]);
    command.stdout(Stdio::null()).stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let start = std::time::Instant::now();
    while server.request_count() == 0 && start.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = i32::try_from(child.id()).unwrap();
    kill(Pid::from_raw(pid), Signal::SIGTERM).unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
}

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

#[test]
fn env_flag_injects_into_config_resolution_and_bash() {
    let server = MockServer::start(vec![
        tool_response(
            "env-bash",
            r#"return await lofi.bash({ cmd: "printf %s \"$EXAMPLE_API_KEY\"" });"#,
        ),
        text_response("env flag answer"),
    ]);
    let fixture = Fixture::new(&server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    // Add a provider whose base_url and key come only from $VAR, so an
    // --env injection is required to reach the mock server.
    std::fs::write(
        &fixture.config,
        format!(
            r#"{config}
[providers.envtest]
base_url = "$EXAMPLE_BASE_URL"
api_key = "$EXAMPLE_API_KEY"

[providers.envtest.models.env]
name = "Env Model"
context_window = 100000
thinking_level = "medium"
thinking_levels = ["low", "medium", "high"]
"#
        ),
    )
    .unwrap();
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        "mode = \"unrestricted\"\n",
    )
    .unwrap();

    let output = fixture.output(&[
        "-e",
        "EXAMPLE_API_KEY=env-secret-key",
        "-e",
        &format!("EXAMPLE_BASE_URL={}", server.url()),
        "--model",
        "envtest/env",
        "--print",
        "use injected env",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("env flag answer"));

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    // The base_url resolved from --env pointed at the mock server, and the
    // key was sent as a bearer token.
    assert!(requests[0]
        .headers
        .to_ascii_lowercase()
        .contains("authorization: bearer env-secret-key"));
    // The bash child saw the injected key, redacted from the transcript.
    let body = &requests[1].body;
    assert!(body.contains("[redacted]"));
    assert!(!body.contains("env-secret-key"));
}

#[test]
fn env_flag_bare_name_forwards_parent_and_missing_errors() {
    let server = MockServer::start(vec![text_response("bare env answer")]);
    let fixture = Fixture::new(&server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"{config}
[providers.envbare]
base_url = "$EXAMPLE_BASE_URL"
api_key = "$EXAMPLE_API_KEY"
no_auth = false

[providers.envbare.models.env]
name = "Env Bare"
context_window = 100000
"#
        ),
    )
    .unwrap();

    // A bare NAME forwards the parent shell's value.
    let output = fixture.output_with_env(
        &[
            "-e",
            "EXAMPLE_API_KEY",
            "-e",
            "EXAMPLE_BASE_URL",
            "--model",
            "envbare/env",
            "--print",
            "bare env",
        ],
        &[
            ("EXAMPLE_API_KEY", "bare-secret"),
            ("EXAMPLE_BASE_URL", &server.url()),
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("bare env answer"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .headers
        .to_ascii_lowercase()
        .contains("authorization: bearer bare-secret"));

    // A bare NAME that is unset in the parent is a config error before any
    // provider request.
    let missing = fixture.output(&["-e", "LOFI_E2E_DEFINITELY_UNSET", "--list-models"]);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("LOFI_E2E_DEFINITELY_UNSET"));
}
#[test]
fn reasoning_content_is_replayed_to_chat_completions_providers() {
    // Round 1 streams thinking + tool call. Round 2 ends with text. The
    // DeepSeek-style thinking must replay as assistant["reasoning_content"]
    // on the round 2 request.
    let server = MockServer::start(vec![
        thinking_tool_response("private rope", "rt", "return 1;"),
        text_response("done"),
    ]);
    let fixture = Fixture::new(&server);

    let output = fixture.output(&["--print", "trigger tool"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[0].body.contains("reasoning_content"),
        "first turn must not invent reasoning_content, body: {}",
        requests[0].body
    );
    let second = &requests[1].body;
    assert!(
        second.contains("reasoning_content"),
        "second turn must replay reasoning_content, body: {second}"
    );
    assert!(second.contains("private rope"));
}

#[test]
fn unresolved_explicit_values_fail_naming_the_provider_and_field() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let base = std::fs::read_to_string(&fixture.config).unwrap();

    let cases = [
        (
            format!(
                "[providers.broken_key]\napi_type = \"openai-completions\"\nbase_url = \"{}\"\napi_key = \"$LOFI_E2E_UNSET_API_KEY\"\n\n[providers.broken_key.models.chat]\n",
                server.url()
            ),
            "broken_key",
            "api_key",
        ),
        (
            "[providers.broken_url]\napi_type = \"openai-completions\"\nno_auth = true\nbase_url = \"$LOFI_E2E_UNSET_BASE_URL\"\n\n[providers.broken_url.models.chat]\n"
                .to_string(),
            "broken_url",
            "base_url",
        ),
        (
            format!(
                "[providers.broken_header]\napi_type = \"openai-completions\"\nno_auth = true\nbase_url = \"{}\"\n\n[providers.broken_header.models.chat]\n\n[providers.broken_header.headers]\nx-organization = \"$LOFI_E2E_UNSET_HEADER\"\n",
                server.url()
            ),
            "broken_header",
            "x-organization",
        ),
    ];
    for (section, provider, field) in cases {
        std::fs::write(&fixture.config, format!("{base}\n{section}")).unwrap();
        let output = fixture.output(&["--list-models"]);
        assert!(
            !output.status.success(),
            "unresolved {field} on {provider} must fail startup"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(provider),
            "{provider} missing from: {stderr}"
        );
        assert!(stderr.contains(field), "{field} missing from: {stderr}");
    }
    assert_eq!(server.request_count(), 0);
}
