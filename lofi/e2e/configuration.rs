use crate::support::{text_response, tool_response, Fixture, MockResponse, MockServer, WAIT};

#[test]
fn automatic_policy_approval_uses_the_configured_model_and_skips_the_modal() {
    let server = MockServer::start(vec![
        tool_response(
            "auto-policy-call",
            r#"return await lofi.bash({ cmd: "printf auto-policy-file > auto-approved.txt" });"#,
        ),
        text_response(r#"{"decision":"allow","reason":"fixture approval"}"#),
        text_response("auto policy final answer"),
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

    tui.submit("use automatic policy approval");
    tui.wait_for("auto policy final answer", WAIT);

    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("auto-approved.txt")).unwrap(),
        "auto-policy-file"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].body.contains("shell-command safety evaluator"));
    assert!(requests[1].body.contains("auto-approved.txt"));
    assert!(requests[1].body.contains(r#""model":"alt""#));
    assert!(requests[1].body.contains(r#""max_completion_tokens":128"#));
}

#[test]
fn bash_env_file_values_are_available_to_commands_and_redacted_from_context() {
    let server = MockServer::start(vec![
        tool_response(
            "bash-env-call",
            r#"return await lofi.bash({ cmd: "printf $FIXTURE_SECRET" });"#,
        ),
        text_response("bash environment final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let env_file = fixture.config.parent().unwrap().join("fixture.env");
    std::fs::write(&env_file, "FIXTURE_SECRET=fixture-secret-value\n").unwrap();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            "[bash]\nstrip_env = true\nenv_file = {:?}\n\n{config}",
            env_file.display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        "mode = \"unrestricted\"\n",
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise configured bash environment");
    tui.wait_for("bash environment final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("[redacted]"));
    assert!(!requests[1].body.contains("fixture-secret-value"));
    assert!(!fixture
        .events()
        .iter()
        .any(|event| event.to_string().contains("fixture-secret-value")));
}

#[test]
fn remote_model_discovery_maps_fields_and_uses_the_cache_when_offline() {
    let server = MockServer::start(vec![MockResponse::json(serde_json::json!({
        "data": [{
            "id": "remote-thinking-model",
            "name": "Remote Thinking Model",
            "context_length": 64000,
            "preferred_api": "responses"
        }]
    }))]);
    let fixture = Fixture::new(&server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"{config}
[providers.discovery]
base_url = "{}"
api_type = "openai-completions"
no_auth = true

[providers.discovery.auto_models]
enabled = true
models_url = "{}/models"
auth = false
ttl_seconds = 3600
api_type_field = "preferred_api"
thinking_level = "high"
thinking_levels = ["low", "high"]

[providers.discovery.auto_models.api_type_mappings]
responses = "openai-responses"

[providers.discovery.auto_models.field_mappings]
name = "name"
context_window = "context_length"
"#,
            server.url(),
            server.url()
        ),
    )
    .unwrap();

    let online = fixture.output(&["--list-models"]);
    assert!(
        online.status.success(),
        "{}",
        String::from_utf8_lossy(&online.stderr)
    );
    let stdout = String::from_utf8_lossy(&online.stdout);
    assert!(stdout.contains("discovery/remote-thinking-model — Remote Thinking Model"));
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].path, "/models");
    drop(server);

    let cached = fixture.output(&["--list-models"]);
    assert!(
        cached.status.success(),
        "{}",
        String::from_utf8_lossy(&cached.stderr)
    );
    assert!(String::from_utf8_lossy(&cached.stdout)
        .contains("discovery/remote-thinking-model — Remote Thinking Model"));
}
