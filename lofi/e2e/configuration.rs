use crate::support::{
    stop_text_response, text_response, tool_response, Fixture, MockResponse, MockServer, WAIT,
};

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
    let models = serde_json::json!({
        "data": [{
            "id": "remote-thinking-model",
            "name": "Remote Thinking Model",
            "context_length": 64000,
            "preferred_api": "responses"
        }]
    });
    let server = MockServer::start(vec![MockResponse::json(&models)]);
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
    assert!(!requests[0]
        .headers
        .to_ascii_lowercase()
        .contains("authorization:"));
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

#[test]
fn discovered_model_inherits_intent_recovery_from_auto_models_config() {
    let models = serde_json::json!({ "data": [{ "id": "broken-template" }] });
    let server = MockServer::start(vec![
        MockResponse::json(&models),
        stop_text_response("I will run the tests next."),
        text_response("auto model continuation answer"),
    ]);
    let fixture = Fixture::new(&server);
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            r#"{config}
[providers.auto-recovery]
base_url = "{}"
api_type = "openai-completions"
no_auth = true

[providers.auto-recovery.auto_models]
enabled = true
models_url = "{}/models"
auth = false
ttl_seconds = 3600

[providers.auto-recovery.auto_models.auto_continue]
intent = true
"#,
            server.url(),
            server.url()
        ),
    )
    .unwrap();

    let output = fixture.output(&[
        "--model",
        "auto-recovery/broken-template",
        "--print",
        "exercise discovered model recovery",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("auto model continuation answer"));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].path, "/models");
    assert!(requests[2].body.contains("Continue the task now"));
}

#[test]
fn policy_explain_covers_custom_rules_wrappers_redirects_and_heredocs() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        r#"mode = "workspace_write"

[[allow]]
match = "fixture-tool"
mode = "prefix"

[[ask]]
match = "fixture-ask --confirm"
mode = "substring"

[[deny]]
match = "fixture-deny: --never"
mode = "args"

[[wrappers]]
name = "fixture-wrap"
kind = "shell_c"

[redirects]
action = "deny"
safe_targets = ["/dev/null"]

[heredocs]
action = "deny"
"#,
    )
    .unwrap();

    for (command, action) in [
        ("fixture-tool read value", "action: allow"),
        ("printf before; fixture-ask --confirm now", "action: ask"),
        ("fixture-deny one --never", "action: deny"),
        ("fixture-wrap -c 'fixture-tool wrapped'", "action: allow"),
        ("fixture-tool write > /dev/null", "action: allow"),
        ("fixture-tool write > output.txt", "action: deny"),
        ("fixture-tool read <<EOF", "action: deny"),
        ("fixture-tool read &", "action: deny"),
    ] {
        let output = fixture.output(&["--policy-explain", command]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(action),
            "command={command} stdout={}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    assert_eq!(server.request_count(), 0);
}

#[test]
fn policy_modes_and_yolo_have_process_level_wire_behavior() {
    let server = MockServer::start(vec![
        tool_response(
            "readonly-call",
            r#"return await lofi.bash({ cmd: "cargo build" });"#,
        ),
        text_response("readonly policy answer"),
        tool_response(
            "yolo-call",
            r#"return await lofi.bash({ cmd: "printf yolo-ran > yolo.txt" });"#,
        ),
        text_response("yolo policy answer"),
    ]);
    let fixture = Fixture::new(&server);

    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        "mode = \"read_only\"\n",
    )
    .unwrap();
    let mut readonly = fixture.spawn(&[]);
    readonly.submit("run a read-only policy command");
    readonly.wait_for("readonly policy answer", WAIT);
    assert!(!fixture.workspace.join("target").exists());
    let requests = server.requests();
    assert!(requests[1].body.contains("denied"));

    std::fs::write(
        fixture.config.parent().unwrap().join("policy.toml"),
        "mode = \"workspace_write\"\nyolo = true\n",
    )
    .unwrap();
    let mut yolo = fixture.spawn(&[]);
    yolo.submit("run a yolo policy command");
    yolo.wait_for("yolo policy answer", WAIT);
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("yolo.txt")).unwrap(),
        "yolo-ran"
    );
    assert_eq!(server.request_count(), 4);
}

#[test]
fn filesystem_secret_boundaries_hold_across_symlinks_and_chunked_output() {
    let server = MockServer::start(vec![
        tool_response(
            "boundary-call",
            r#"const results = {};
for (const [name, run] of Object.entries({
  outside: () => lofi.read("../config.toml"),
  linked: () => lofi.read("linked-secret.txt"),
  huge: () => lofi.bash({
    cmd: "printenv FIXTURE_SECRET; i=0; while [ $i -lt 1200 ]; do echo chunk-$i; i=$((i+1)); done; printenv FIXTURE_SECRET",
  }),
})) {
  results[name] = await run().then(
    (value) => ({ ok: true, value }),
    (error) => ({ ok: false, error: String(error) }),
  );
}
return results;"#,
        ),
        text_response("boundary final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let env_file = fixture.config.parent().unwrap().join("boundary.env");
    std::fs::write(&env_file, "FIXTURE_SECRET=fixture-secret-value\n").unwrap();
    std::os::unix::fs::symlink(&env_file, fixture.workspace.join("linked-secret.txt")).unwrap();
    let config = std::fs::read_to_string(&fixture.config).unwrap();
    std::fs::write(
        &fixture.config,
        format!(
            "[bash]\nstrip_env = true\nenv_file = {:?}\n\n{config}",
            env_file.display().to_string()
        ),
    )
    .unwrap();
    fixture.set_truncation(1, 64);

    let mut tui = fixture.spawn(&[]);
    tui.submit("exercise filesystem secret boundaries");
    tui.wait_for("boundary final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    assert!(
        body.matches("path escapes workspace").count() >= 2,
        "{body}"
    );
    assert!(body.contains("[redacted]"), "{body}");
    assert!(!requests[1].body.contains("fixture-secret-value"));
    assert!(!fixture
        .events()
        .iter()
        .any(|event| event.to_string().contains("fixture-secret-value")));
}
