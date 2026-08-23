use crate::support::{
    anthropic_usage_response, eof_cut_responses_response, event_types,
    google_thinking_usage_response, lost_tool_response, responses_response, text_response,
    thinking_response, tool_response, transcript_text, wait_for_process_exit, Fixture,
    MockResponse, MockServer, ProcessGuard, WAIT,
};

#[test]
fn global_and_local_agents_prompts_are_ordered_once_and_survive_resume() {
    let server = MockServer::start(vec![
        text_response("system prompt first answer"),
        text_response("system prompt resumed answer"),
    ]);
    let fixture = Fixture::new(&server);
    std::fs::write(
        fixture.config.parent().unwrap().join("AGENTS.md"),
        "global system prompt marker",
    )
    .unwrap();
    std::fs::write(
        fixture.workspace.join("AGENTS.md"),
        "workspace system prompt marker",
    )
    .unwrap();
    let mut first = fixture.spawn(&[]);

    first.submit("system prompt first request");
    first.wait_for("system prompt first answer", WAIT);
    first.submit("/quit");
    first.wait_exit();
    std::fs::write(
        fixture.config.parent().unwrap().join("AGENTS.md"),
        "changed global system prompt marker",
    )
    .unwrap();
    std::fs::write(
        fixture.workspace.join("AGENTS.md"),
        "changed workspace system prompt marker",
    )
    .unwrap();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("system prompt resumed request");
    resumed.wait_for("system prompt resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "system")
                .count(),
            1
        );
        let system = messages[0]["content"].as_str().unwrap();
        let global = system.find("global system prompt marker").unwrap();
        let workspace = system.find("workspace system prompt marker").unwrap();
        assert!(global < workspace);
        assert_eq!(system.matches("global system prompt marker").count(), 1);
        assert_eq!(system.matches("workspace system prompt marker").count(), 1);
    }
    assert!(requests[1].body.contains("system prompt first request"));
    assert!(requests[1].body.contains("system prompt first answer"));
    assert!(requests[1].body.contains("system prompt resumed request"));
    assert!(!requests[1]
        .body
        .contains("changed global system prompt marker"));
    assert!(!requests[1]
        .body
        .contains("changed workspace system prompt marker"));
}

#[test]
fn local_agents_prompts_follow_the_project_tree_outermost_first() {
    let server = MockServer::start(vec![text_response("local agents answer marker")]);
    let fixture = Fixture::new(&server);
    let nested = fixture.workspace.join("crates/nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(fixture.workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
    std::fs::write(
        fixture.workspace.join("AGENTS.md"),
        "project root agents marker",
    )
    .unwrap();
    std::fs::write(nested.join("AGENTS.md"), "nested local agents marker").unwrap();
    let mut tui = fixture.spawn_in(&nested, &[]);

    tui.submit("local agents prompt marker");
    tui.wait_for("local agents answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    let system = body["messages"][0]["content"].as_str().unwrap();
    let root = system.find("project root agents marker").unwrap();
    let local = system.find("nested local agents marker").unwrap();
    assert!(root < local);
    assert!(system.contains(&format!(
        "<agents_md source=\"{}\">",
        fixture.workspace.display()
    )));
    assert!(system.contains(&format!("<agents_md source=\"{}\">", nested.display())));
    assert_eq!(system.matches("project root agents marker").count(), 1);
    assert_eq!(system.matches("nested local agents marker").count(), 1);
}

#[test]
fn system_prompt_includes_the_skills_index_without_loading_skill_bodies() {
    let server = MockServer::start(vec![text_response("skills prompt answer marker")]);
    let fixture = Fixture::new(&server);
    let skill_dir = fixture
        .config
        .parent()
        .unwrap()
        .join("skills/system-prompt-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: system-prompt-skill\ndescription: system prompt skill description marker\n---\n\nsystem prompt skill body must stay lazy\n",
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("skills prompt request marker");
    tui.wait_for("skills prompt answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("<skills>"));
    assert!(system.contains("Load a skill with `lofi.skill(name)` before following it."));
    assert!(system.contains("<name>system-prompt-skill</name>"));
    assert!(
        system.contains("<description>system prompt skill description marker</description>"),
        "system prompt: {system}"
    );
    assert!(!system.contains("<location>"), "system prompt: {system}");
    assert!(!system.contains("system prompt skill body must stay lazy"));
}

#[test]
fn every_supported_thinking_level_reaches_the_provider_request() {
    let server = MockServer::start(vec![
        text_response("thinking off level answer"),
        text_response("thinking low level answer"),
        text_response("thinking medium level answer"),
        text_response("thinking high level answer"),
        text_response("thinking xhigh level answer"),
    ]);
    let fixture = Fixture::new(&server);

    for level in ["off", "low", "medium", "high", "xhigh"] {
        let model = format!("mock/chat:{level}");
        let prompt = format!("thinking {level} level prompt");
        let output = fixture.output(&["--model", &model, "--print", &prompt]);
        assert!(
            output.status.success(),
            "thinking level {level}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    for (request, level) in requests
        .iter()
        .zip(["off", "low", "medium", "high", "xhigh"])
    {
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        if level == "off" {
            assert!(body.get("reasoning_effort").is_none());
        } else {
            assert_eq!(body["reasoning_effort"], level);
        }
        assert!(request
            .body
            .contains(&format!("thinking {level} level prompt")));
    }
}

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
    let request: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(request["reasoning"]["effort"], "high");
    assert_eq!(request["reasoning"]["summary"], "auto");
    assert_eq!(request["store"], false);
    assert_eq!(request["include"][0], "reasoning.encrypted_content");
    assert!(requests[0].body.contains("responses prompt marker"));

    let transcript = transcript_text(&fixture.events());
    assert!(transcript.contains("responses thinking marker"));
    assert!(transcript.contains("responses answer marker"));
    assert!(transcript.contains("cache_read_tokens"));
}

type ThinkingResponse = fn(&str, &str) -> MockResponse;

fn loop_api_cases() -> [(&'static str, &'static str, ThinkingResponse); 4] {
    [
        ("OpenAI Completions", "mock/chat:high", thinking_response),
        (
            "OpenAI Responses",
            "responses/reasoning:high",
            responses_response,
        ),
        (
            "Anthropic Messages",
            "anthropic/tools:high",
            |thinking, text| anthropic_usage_response(thinking, "loop-signature", text),
        ),
        (
            "Google Generative AI",
            "google/tools:high",
            google_thinking_usage_response,
        ),
    ]
}

#[test]
fn repeated_thinking_notifies_and_recovers_once_for_all_api_types() {
    let pattern = "abcdefghij".repeat(10);
    for (api, model, response) in loop_api_cases() {
        let server = MockServer::start(vec![
            response(&pattern.repeat(3), "discarded loop answer"),
            response("different reasoning", "loop recovery answer marker")
                .with_delay(std::time::Duration::from_millis(500)),
        ]);
        let fixture = Fixture::new(&server);
        let mut tui = fixture.spawn(&["--model", model]);

        tui.submit("loop recovery prompt marker");
        tui.wait_for("potential agent loop detected", WAIT);
        tui.wait_for("loop recovery answer marker", WAIT);

        let requests = server.requests();
        assert_eq!(requests.len(), 2, "{api}");
        assert!(
            requests[1].body.contains("A potential loop was detected"),
            "{api}"
        );
        assert!(
            requests[1]
                .body
                .contains("repeated thinking pattern detected"),
            "{api}"
        );
        let transcript = transcript_text(&fixture.events());
        assert!(
            transcript.contains("A potential loop was detected"),
            "{api}"
        );
        assert!(transcript.contains("loop recovery answer marker"), "{api}");
        assert!(!transcript.contains("discarded loop answer"), "{api}");
    }
}

#[test]
fn repeated_thinking_stops_after_failed_recovery_for_all_api_types() {
    let pattern = "abcdefghij".repeat(10).repeat(3);
    for (api, model, response) in loop_api_cases() {
        let server = MockServer::start(vec![
            response(&pattern, "discarded first loop answer"),
            response(&pattern, "discarded second loop answer")
                .with_delay(std::time::Duration::from_millis(500)),
        ]);
        let fixture = Fixture::new(&server);
        let mut tui = fixture.spawn(&["--model", model]);

        tui.submit("failed loop recovery prompt marker");
        tui.wait_for("potential agent loop detected", WAIT);
        tui.wait_for("agent stopped after loop recovery failed", WAIT);

        assert_eq!(server.request_count(), 2, "{api}");
        assert!(
            event_types(&fixture.events()).contains(&"turn_end"),
            "{api}"
        );
        let transcript = transcript_text(&fixture.events());
        assert_eq!(
            transcript.matches("A potential loop was detected").count(),
            1,
            "{api}"
        );
        assert!(!transcript.contains("discarded first loop answer"), "{api}");
        assert!(
            !transcript.contains("discarded second loop answer"),
            "{api}"
        );
    }
}

#[test]
fn openai_responses_replays_encrypted_reasoning_on_the_next_turn() {
    let server = MockServer::start(vec![
        responses_response("first reasoning marker", "first responses answer"),
        responses_response("second reasoning marker", "second responses answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning:high"]);

    tui.submit("first responses prompt");
    tui.wait_for("first responses answer", WAIT);
    tui.clear_output();
    tui.submit("second responses prompt");
    tui.wait_for("second responses answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let input = second["input"].as_array().unwrap();
    let reasoning = input
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning item in the next request");
    assert_eq!(reasoning["encrypted_content"], "encrypted-reasoning-marker");
    assert_eq!(reasoning["summary"][0]["text"], "first reasoning marker");
}

#[test]
fn openai_responses_omits_reasoning_request_when_thinking_is_off() {
    let server = MockServer::start(vec![responses_response(
        "provider reasoning marker",
        "thinking off answer marker",
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning:off"]);

    tui.submit("thinking off prompt marker");
    tui.wait_for("thinking off answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert!(request.get("reasoning").is_none());
    assert_eq!(request["store"], false);
    assert_eq!(request["include"][0], "reasoning.encrypted_content");
}

#[test]
fn truncated_responses_stream_retries_without_persisting_partial_output() {
    let server = MockServer::start(vec![
        eof_cut_responses_response("discarded partial marker"),
        responses_response("retry reasoning marker", "complete response marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning:high"]);

    tui.submit("truncated response prompt marker");
    tui.wait_for("complete response marker", WAIT);

    assert_eq!(server.request_count(), 2);
    let transcript = transcript_text(&fixture.events());
    assert!(!transcript.contains("discarded partial marker"));
    assert!(transcript.contains("complete response marker"));
    assert!(event_types(&fixture.events()).contains(&"turn_end"));
}

#[test]
fn missing_tool_call_is_reissued_after_provider_tool_stop() {
    let server = MockServer::start(vec![
        lost_tool_response("I will inspect the files now."),
        tool_response(
            "reissued-tool",
            r#"return { marker: "reissued tool marker" };"#,
        ),
        text_response("reissued tool final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let output = fixture.output(&["--print", "recover the missing tool call"]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("reissued tool final answer"));
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].body.contains("Your tool call was not received"));
    assert!(requests[2].body.contains("reissued tool marker"));
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
fn authentication_failure_is_not_retried_and_records_a_failed_turn() {
    let server = MockServer::start(vec![MockResponse::error(
        401,
        "invalid authentication edge marker",
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("authentication failure prompt marker");
    tui.wait_for("invalid authentication", WAIT);

    assert_eq!(server.request_count(), 1);
    let events = fixture.events();
    assert!(event_types(&events).contains(&"turn_failed"));
    assert!(!event_types(&events).contains(&"turn_end"));
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

#[test]
fn workspace_and_namespaced_skills_override_global_metadata_without_eager_loading() {
    let server = MockServer::start(vec![text_response("workspace skill answer")]);
    let fixture = Fixture::new(&server);
    let global = fixture.config.parent().unwrap().join("skills/review");
    let workspace = fixture.workspace.join(".lofi/skills/review");
    let namespaced = fixture.workspace.join(".lofi/skills/git/rebase");
    std::fs::create_dir_all(&global).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&namespaced).unwrap();
    std::fs::write(
        global.join("SKILL.md"),
        "---\ndescription: global review marker\n---\nglobal body marker\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join("SKILL.md"),
        "---\ndescription: workspace review marker\n---\nworkspace body marker\n",
    )
    .unwrap();
    std::fs::write(
        namespaced.join("SKILL.md"),
        "---\ndescription: namespaced rebase marker\n---\nnamespaced body marker\n",
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("inspect workspace skills");
    tui.wait_for("workspace skill answer", WAIT);

    let request: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
    let system = request["messages"][0]["content"].as_str().unwrap();
    assert!(system.contains("<name>review</name>"));
    assert!(system.contains("workspace review marker"));
    assert!(!system.contains("global review marker"));
    assert!(system.contains("<name>git/rebase</name>"));
    assert!(system.contains("namespaced rebase marker"));
    assert!(!system.contains("workspace body marker"));
    assert!(!system.contains("namespaced body marker"));
}

#[test]
fn retry_exhaustion_after_a_tool_round_records_one_failed_turn() {
    let server = MockServer::start(vec![
        tool_response(
            "retry-after-tool",
            "return { marker: \"completed tool round\" };",
        ),
        MockResponse::error(503, "retry exhaustion one"),
        MockResponse::error(503, "retry exhaustion two"),
        MockResponse::error(503, "retry exhaustion final"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exhaust retry after tool");
    tui.wait_for("retry exhaustion final", WAIT);

    assert_eq!(server.request_count(), 4);
    let events = fixture.events();
    assert_eq!(
        event_types(&events)
            .into_iter()
            .filter(|kind| *kind == "turn_failed")
            .count(),
        1
    );
    let transcript = transcript_text(&events);
    assert!(transcript.contains("completed tool round"));
    assert!(!event_types(&events).contains(&"turn_end"));
}

#[test]
fn cancellation_interrupts_a_configured_retry_delay() {
    let server = MockServer::start(vec![MockResponse::error(503, "delayed retry marker")]);
    let fixture = Fixture::new(&server);
    let config = std::fs::read_to_string(&fixture.config).unwrap().replace(
        "base_delay_ms = 1\nmax_delay_ms = 1",
        "base_delay_ms = 5000\nmax_delay_ms = 5000",
    );
    std::fs::write(&fixture.config, config).unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("cancel during retry delay");
    let start = std::time::Instant::now();
    while server.request_count() == 0 && start.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    std::thread::sleep(std::time::Duration::from_millis(100));
    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);

    assert_eq!(server.request_count(), 1);
    assert!(event_types(&fixture.events()).contains(&"turn_cancelled"));
}

#[test]
fn cancelling_a_job_wait_returns_control_without_killing_the_job() {
    let server = MockServer::start(vec![tool_response(
        "cancel-job-wait",
        r#"const s = await lofi.jobSpawn({ cmd: "echo $$ > jobwait.pid; exec sleep 60" }); const w = await lofi.jobWait({ id: s.id }); return w;"#,
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("cancel job wait prompt marker");
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"a");
    let pid_path = fixture.workspace.join("jobwait.pid");
    let start = std::time::Instant::now();
    while !pid_path.exists() && start.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        pid_path.exists(),
        "job did not start; terminal output: {}",
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

    // The wait was interrupted, not the job: the process must still be alive.
    assert!(
        crate::support::process_is_alive(pid),
        "job process died after jobWait cancel"
    );
    process.disarm();

    assert_eq!(server.request_count(), 1);
    let events = fixture.events();
    assert!(event_types(&events).contains(&"turn_cancelled"));
}
