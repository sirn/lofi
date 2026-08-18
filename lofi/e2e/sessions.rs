use std::io::Write;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::support::{
    delayed_text_response, event_types, job_events, parallel_responses_tool_response,
    parallel_tool_response, process_is_alive, responses_response, spawned_pid, text_response,
    text_response_with_usage, tool_response, tool_response_with_usage, transcript_text,
    wait_for_process_exit, Fixture, MockResponse, MockServer, ProcessGuard, WAIT,
};

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
        .position(|event| event["type"] == "job_started")
        .unwrap();
    let finish_index = events
        .iter()
        .position(|event| event["type"] == "job_finished")
        .unwrap();
    assert!(start_index < finish_index);
}

#[test]
fn rollback_away_then_back_does_not_report_completed_job_as_stale() {
    let server = MockServer::start(vec![
        tool_response(
            "spawn",
            "return await lofi.jobSpawn({ cmd: \"sleep 0.5\", notify: false });",
        ),
        text_response("first answer settled"),
        text_response("second answer after job"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("spawn a short job");
    tui.wait_for("first answer settled", WAIT);
    let mut job = ProcessGuard::new(spawned_pid(&fixture));
    wait_for_process_exit(job.pid());
    job.disarm();
    // The driver records JobFinished asynchronously after the process exits.
    fixture.wait_for_event_count("job_finished", 1);

    tui.clear_output();
    tui.submit("second prompt after job");
    tui.wait_for("second answer after job", WAIT);

    // First rollback: drop to before JobStarted. The completed job leaves the
    // registry even though kill() sees a terminal state and reports no kill.
    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    // The tree hydrates asynchronously; give the visible window time to
    // resolve labels before navigating.
    std::thread::sleep(std::time::Duration::from_millis(500));
    tui.send(b"\x1b[A\x1b[A\x1b[A\x1b[A\r");
    std::thread::sleep(std::time::Duration::from_millis(300));

    // Second rollback: forward to "first answer settled", between JobStarted
    // and JobFinished. The finish marker is now on the abandoned branch, but
    // the process already exited, so this must not queue a stale notice.
    tui.send(b"\x15");
    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(500));
    // The JobFinished marker between turns breaks the tree's forward chain,
    // so after the first rollback only "spawn a short job" and "first answer
    // settled" remain. "first answer settled" is the last row and already
    // selected; Enter restores the point between JobStarted and JobFinished.
    tui.send(b"\r");
    std::thread::sleep(std::time::Duration::from_millis(300));

    // A queued stale notice is drained by the next input event while at rest.
    // Send a neutral keystroke and give any injected notice time to become an
    // agent request before asserting the request log stayed at three turns.
    tui.send(b" ");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline && server.request_count() < 4 {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let requests = server.requests();
    assert_eq!(
        requests.len(),
        3,
        "completed job must not queue a stale notice; requests: {:?}",
        requests.iter().map(|r| r.body.clone()).collect::<Vec<_>>()
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.body.contains("their ids are stale")),
        "completed job must not be reported as stale"
    );
}

#[test]
fn rollback_without_jobs_produces_no_stale_notification() {
    let server = MockServer::start(vec![
        text_response("first answer done"),
        text_response("second answer done"),
        text_response("post-rollback answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("first prompt");
    tui.wait_for("first answer done", WAIT);
    tui.clear_output();
    tui.submit("second prompt");
    tui.wait_for("second answer done", WAIT);

    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    tui.wait_for("first prompt", WAIT);
    tui.send(b"\x1b[A\r");
    std::thread::sleep(std::time::Duration::from_millis(300));

    tui.submit("post rollback prompt");
    tui.wait_for("post-rollback answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let post_rollback = &requests[2].body;
    assert!(
        !post_rollback.contains("their ids are stale"),
        "no jobs running, must not report stale: {post_rollback}"
    );
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
    tui.wait_for("Roll back to a turn", WAIT);
    tui.wait_for("sleep 60", WAIT);
    tui.send(b"\x1b[A\r");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(process_is_alive(pid));

    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    tui.wait_for("sleep 60", WAIT);
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
    assert_eq!(server.request_count(), 3);
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
    assert_eq!(server.request_count(), 3);
    let requests = server.requests();
    let resume_request = &requests.last().unwrap().body;
    assert!(resume_request.contains("session resumed: jobs ["));
    assert!(resume_request.contains("their ids are stale"));

    let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
    wait_for_process_exit(pid);
    job.disarm();
}

#[test]
fn tree_rollback_excludes_future_turns_and_preserves_the_old_branch() {
    let server = MockServer::start(vec![
        text_response("rollback first answer"),
        text_response("rollback second answer"),
        text_response("rollback branch answer"),
        text_response("rollback restarted answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("rollback first prompt");
    tui.wait_for("rollback first answer", WAIT);
    tui.clear_output();
    tui.submit("rollback second prompt");
    tui.wait_for("rollback second answer", WAIT);

    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(300));
    tui.send(b"\x1b[A\r");
    std::thread::sleep(std::time::Duration::from_millis(100));
    tui.send(b"\x15");
    tui.submit("rollback replacement prompt");
    tui.wait_for("rollback branch answer", WAIT);

    tui.submit("/quit");
    tui.wait_exit();

    let mut restarted = fixture.spawn(&["--continue"]);
    restarted.submit("rollback restarted prompt");
    restarted.wait_for("rollback restarted answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for branch in [&requests[2].body, &requests[3].body] {
        assert!(branch.contains("rollback first prompt"));
        assert!(branch.contains("rollback first answer"));
        assert!(branch.contains("rollback replacement prompt"));
        assert!(!branch.contains("rollback second prompt"));
        assert!(!branch.contains("rollback second answer"));
    }
    assert!(requests[3].body.contains("rollback branch answer"));
    assert!(requests[3].body.contains("rollback restarted prompt"));

    let transcript = transcript_text(&fixture.events());
    assert!(transcript.contains("rollback second prompt"));
    assert!(transcript.contains("rollback second answer"));
    assert!(transcript.contains("rollback replacement prompt"));
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[test]
fn closing_large_tree_picker_releases_transient_memory() {
    let server = MockServer::start(vec![text_response("memory seed answer")]);
    let fixture = Fixture::new(&server);
    let mut seed = fixture.spawn(&[]);

    seed.submit("memory seed prompt");
    seed.wait_for("memory seed answer", WAIT);
    seed.submit("/quit");
    seed.wait_exit();
    fixture.append_tree_siblings(75_000);

    let mut tui = fixture.spawn(&["--continue"]);
    let baseline = tui.resident_kib();
    let mut closed_samples = Vec::new();
    for _ in 0..3 {
        tui.clear_output();
        tui.submit("/tree");
        tui.wait_for("074999", WAIT);
        tui.send(b"\x1b");
        std::thread::sleep(std::time::Duration::from_millis(250));
        closed_samples.push(tui.resident_kib());
    }

    let highest_closed = closed_samples.iter().copied().max().unwrap();
    assert!(
        highest_closed <= baseline + 16 * 1024,
        "tree picker retained too much memory: baseline={baseline} KiB, closed={closed_samples:?} KiB"
    );
}

#[test]
fn fresh_and_empty_continue_sessions_are_created_lazily() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);

    let mut fresh = fixture.spawn(&[]);
    fresh.submit("/quit");
    fresh.wait_exit();
    assert!(fixture.session_files().is_empty());

    let mut continued = fixture.spawn(&["--continue"]);
    continued.submit("/quit");
    continued.wait_exit();
    assert!(fixture.session_files().is_empty());
    assert_eq!(server.request_count(), 0);
}

#[test]
fn continue_is_scoped_to_the_exact_workspace() {
    let server = MockServer::start(vec![
        text_response("workspace a answer"),
        text_response("workspace b answer"),
        text_response("workspace a resumed answer"),
        text_response("workspace b resumed answer"),
    ]);
    let fixture = Fixture::new(&server);
    let workspace_b = fixture.workspace.join("other-workspace");
    std::fs::create_dir_all(&workspace_b).unwrap();

    let mut a = fixture.spawn(&[]);
    a.submit("workspace a prompt");
    a.wait_for("workspace a answer", WAIT);
    a.submit("/quit");
    a.wait_exit();

    let mut b = fixture.spawn_in(&workspace_b, &[]);
    b.submit("workspace b prompt");
    b.wait_for("workspace b answer", WAIT);
    b.submit("/quit");
    b.wait_exit();

    let mut resumed_a = fixture.spawn(&["--continue"]);
    resumed_a.submit("workspace a resumed prompt");
    resumed_a.wait_for("workspace a resumed answer", WAIT);
    resumed_a.submit("/quit");
    resumed_a.wait_exit();

    let mut resumed_b = fixture.spawn_in(&workspace_b, &["--continue"]);
    resumed_b.submit("workspace b resumed prompt");
    resumed_b.wait_for("workspace b resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[2].body.contains("workspace a prompt"));
    assert!(!requests[2].body.contains("workspace b prompt"));
    assert!(requests[3].body.contains("workspace b prompt"));
    assert!(!requests[3].body.contains("workspace a prompt"));
    assert_eq!(fixture.session_files().len(), 2);
}

#[test]
fn failed_turn_is_visible_but_excluded_from_history_after_restart() {
    let server = MockServer::start(vec![
        text_response("successful baseline answer"),
        MockResponse::error(401, "failed lifecycle marker"),
        text_response("failed lifecycle recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("successful baseline prompt");
    first.wait_for("successful baseline answer", WAIT);
    first.submit("failed lifecycle prompt");
    first.wait_for("failed lifecycle marker", WAIT);
    fixture.wait_for_event_count("turn_failed", 1);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("failed lifecycle recovery prompt");
    resumed.wait_for("failed lifecycle recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let recovered = &requests[2].body;
    assert!(recovered.contains("successful baseline prompt"));
    assert!(recovered.contains("successful baseline answer"));
    assert!(recovered.contains("failed lifecycle recovery prompt"));
    assert!(!recovered.contains("failed lifecycle prompt"));
    assert!(!recovered.contains("failed lifecycle marker"));

    let events = fixture.events();
    assert!(event_types(&events).contains(&"turn_failed"));
    assert!(transcript_text(&events).contains("failed lifecycle prompt"));
}

#[test]
fn prompt_is_durable_when_the_process_stops_during_the_provider_request() {
    let server = MockServer::start(vec![
        delayed_text_response(
            "response that must not become durable",
            std::time::Duration::from_millis(500),
        ),
        text_response("interrupted lifecycle recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut interrupted = fixture.spawn(&[]);

    interrupted.submit("interrupted lifecycle prompt");
    let started = std::time::Instant::now();
    while server.request_count() < 1 && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(server.request_count(), 1);
    fixture.wait_for_event_count("message", 1);
    interrupted.kill_now();
    drop(interrupted);
    std::thread::sleep(std::time::Duration::from_millis(550));

    let before_resume = fixture.events();
    assert!(transcript_text(&before_resume).contains("interrupted lifecycle prompt"));
    assert!(!event_types(&before_resume).contains(&"turn_end"));

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("interrupted lifecycle recovery prompt");
    resumed.wait_for("interrupted lifecycle recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("interrupted lifecycle prompt"));
    assert!(requests[1]
        .body
        .contains("interrupted lifecycle recovery prompt"));
    assert!(!requests[1]
        .body
        .contains("response that must not become durable"));
}

#[test]
fn user_shell_output_creates_a_session_and_survives_restart() {
    let server = MockServer::start(vec![text_response("shell resume answer")]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("!printf durable-shell-output-marker");
    first.wait_for("durable-shell-output-marker", WAIT);
    fixture.wait_for_event_count("user_shell", 1);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("shell resume prompt");
    resumed.wait_for("shell resume answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("durable-shell-output-marker"));
    assert!(requests[0].body.contains("shell resume prompt"));
    assert!(event_types(&fixture.events()).contains(&"user_shell"));
}

#[test]
fn cancelled_tool_turn_replays_as_a_closed_cycle_after_restart() {
    let server = MockServer::start(vec![
        tool_response(
            "cancelled-lifecycle-tool",
            "return await lofi.bash({ cmd: \"echo $$ > cancelled-lifecycle.pid; exec sleep 60\" });",
        ),
        text_response("cancelled lifecycle recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("cancelled lifecycle prompt");
    first.wait_for("Permission Required", WAIT);
    first.send(b"a");
    let pid_path = fixture.workspace.join("cancelled-lifecycle.pid");
    let started = std::time::Instant::now();
    while !pid_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let mut process = ProcessGuard::new(pid);
    first.send(b"\x03");
    first.wait_for("Cancelled", WAIT);
    wait_for_process_exit(pid);
    process.disarm();
    fixture.wait_for_event_count("turn_cancelled", 1);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("cancelled lifecycle recovery prompt");
    resumed.wait_for("cancelled lifecycle recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let recovered: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = recovered["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .expect("cancelled tool call after resume");
    assert_eq!(call["tool_calls"][0]["id"], "cancelled-lifecycle-tool");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("cancelled tool result after resume");
    assert_eq!(result["tool_call_id"], "cancelled-lifecycle-tool");
    assert!(result["content"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .contains("cancel"));
    assert!(event_types(&fixture.events()).contains(&"turn_cancelled"));
}

#[test]
fn cancelled_parallel_tool_calls_replay_as_closed_cycles_after_restart() {
    let server = MockServer::start(vec![
        parallel_tool_response(&[
            (
                "cancelled-parallel-a",
                r#"return { marker: "cancelled parallel first completed" };"#,
            ),
            (
                "cancelled-parallel-b",
                r#"return await lofi.bash({ cmd: "echo $$ > cancelled-parallel-b.pid; exec sleep 60" });"#,
            ),
        ]),
        text_response("cancelled parallel recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("cancelled parallel prompt");
    first.wait_for("Permission Required", WAIT);
    first.send(b"a");
    let mut guards = Vec::new();
    for name in ["cancelled-parallel-b.pid"] {
        let path = fixture.workspace.join(name);
        let started = std::time::Instant::now();
        while !path.exists() && started.elapsed() < WAIT {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            path.exists(),
            "tool process did not create {name}; terminal output:
{}",
            first.output()
        );
        let pid = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        guards.push(ProcessGuard::new(pid));
    }
    first.send(b"\x03");
    first.wait_for("Cancelled", WAIT);
    for guard in &mut guards {
        wait_for_process_exit(guard.pid());
        guard.disarm();
    }
    fixture.wait_for_event_count("turn_cancelled", 1);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("cancelled parallel recovery prompt");
    resumed.wait_for("cancelled parallel recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let recovered: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = recovered["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .expect("cancelled assistant tool calls after resume");
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
    for call_id in ["cancelled-parallel-a", "cancelled-parallel-b"] {
        assert!(assistant["tool_calls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|call| call["id"] == call_id));
        let result = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .find(|message| message["tool_call_id"] == call_id)
            .expect("tool result after resume");
        let content = result["content"].as_str().unwrap();
        if call_id == "cancelled-parallel-a" {
            assert!(content.contains("cancelled parallel first completed"));
        } else {
            assert!(content.to_ascii_lowercase().contains("cancel"));
        }
    }
    assert!(event_types(&fixture.events()).contains(&"turn_cancelled"));
}

#[test]
fn cancelled_parallel_responses_calls_replay_as_closed_cycles_after_restart() {
    let server = MockServer::start(vec![
        parallel_responses_tool_response(&[
            (
                "cancelled-responses-a",
                r#"return { marker: "cancelled responses first completed" };"#,
            ),
            (
                "cancelled-responses-b",
                r#"return await lofi.bash({ cmd: "echo $$ > cancelled-responses-b.pid; exec sleep 60" });"#,
            ),
        ]),
        responses_response(
            "cancelled responses recovery thinking",
            "cancelled responses recovery answer",
        ),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&["--model", "responses/reasoning"]);

    first.submit("cancelled responses prompt");
    first.wait_for("Permission Required", WAIT);
    first.send(b"a");
    let mut guards = Vec::new();
    for name in ["cancelled-responses-b.pid"] {
        let path = fixture.workspace.join(name);
        let started = std::time::Instant::now();
        while !path.exists() && started.elapsed() < WAIT {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let pid = std::fs::read_to_string(&path)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        guards.push(ProcessGuard::new(pid));
    }
    first.send(b"\x03");
    first.wait_for("Cancelled", WAIT);
    for guard in &mut guards {
        wait_for_process_exit(guard.pid());
        guard.disarm();
    }
    fixture.wait_for_event_count("turn_cancelled", 1);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("cancelled responses recovery prompt");
    resumed.wait_for("cancelled responses recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let recovered: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let input = recovered["input"].as_array().unwrap();
    for call_id in ["cancelled-responses-a", "cancelled-responses-b"] {
        assert!(input
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == call_id));
        let result = input
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .find(|item| item["call_id"] == call_id)
            .expect("responses output after resume");
        let output = result["output"].as_str().unwrap();
        if call_id == "cancelled-responses-a" {
            assert!(output.contains("cancelled responses first completed"));
        } else {
            assert!(output.to_ascii_lowercase().contains("cancel"));
        }
    }
    assert!(event_types(&fixture.events()).contains(&"turn_cancelled"));
}

#[test]
fn resume_restores_prior_conversation_context() {
    let server = MockServer::start(vec![
        text_response("first answer marker"),
        text_response("second answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("first prompt marker");
    first.wait_for("first answer marker", WAIT);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("second prompt marker");
    resumed.wait_for("second answer marker", WAIT);

    let requests = server.requests();
    let resumed_body = &requests.last().unwrap().body;
    assert!(resumed_body.contains("first prompt marker"));
    assert!(resumed_body.contains("first answer marker"));
    assert!(resumed_body.contains("second prompt marker"));
}

#[test]
fn resume_replays_encrypted_responses_reasoning() {
    let server = MockServer::start(vec![
        responses_response("saved reasoning marker", "saved responses answer"),
        responses_response("resumed reasoning marker", "resumed responses answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&["--model", "responses/reasoning:high"]);

    first.submit("saved responses prompt");
    first.wait_for("saved responses answer", WAIT);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("resumed responses prompt");
    resumed.wait_for("resumed responses answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let request: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let reasoning = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("reasoning item after resume");
    assert_eq!(reasoning["encrypted_content"], "encrypted-reasoning-marker");
    assert_eq!(reasoning["summary"][0]["text"], "saved reasoning marker");
}

#[test]
fn automatic_compaction_crossing_shapes_the_next_request() {
    let server = MockServer::start(vec![
        text_response_with_usage("auto compact answer one", 10),
        text_response_with_usage("auto compact answer two", 20),
        text_response_with_usage("auto compact answer three", 60),
        text_response("auto compact final answer"),
    ]);
    let fixture = Fixture::new(&server);
    fixture.enable_auto_compaction(50);
    let mut tui = fixture.spawn(&[]);

    for (index, (prompt, answer)) in [
        ("auto compact prompt one", "auto compact answer one"),
        ("auto compact prompt two", "auto compact answer two"),
        ("auto compact prompt three", "auto compact answer three"),
    ]
    .into_iter()
    .enumerate()
    {
        tui.submit(prompt);
        tui.wait_for(answer, WAIT);
        if index < 2 {
            tui.clear_output();
        }
    }
    tui.wait_for("Compacted", WAIT);
    tui.submit("auto compact final prompt");
    tui.wait_for("auto compact final answer", WAIT);

    let events = fixture.events();
    assert!(events.iter().any(|event| event["type"] == "compaction"));
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let request: serde_json::Value = serde_json::from_str(&requests.last().unwrap().body).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("This summary captures work done"))
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "user" && message["content"] == "auto compact prompt three"
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "assistant" && message["content"] == "auto compact answer three"
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "user" && message["content"] == "auto compact final prompt"
    }));
}

#[test]
fn manual_compaction_survives_resume_and_shapes_the_next_request() {
    let server = MockServer::start(vec![
        text_response("answer one marker"),
        text_response("answer two marker"),
        text_response("answer three marker"),
        text_response("answer four marker"),
        text_response("post compaction answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    for (prompt, answer) in [
        ("prompt one marker", "answer one marker"),
        ("prompt two marker", "answer two marker"),
        ("prompt three marker", "answer three marker"),
        ("prompt four marker", "answer four marker"),
    ] {
        tui.submit(prompt);
        tui.wait_for(answer, WAIT);
        tui.clear_output();
    }
    tui.submit("/compact");
    tui.wait_for("Compacted", WAIT);
    tui.submit("/quit");
    tui.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("post compaction prompt marker");
    resumed.wait_for("post compaction answer marker", WAIT);

    let events = fixture.events();
    assert!(events.iter().any(|event| event["type"] == "compaction"));
    let requests = server.requests();
    assert_eq!(requests.len(), 5);
    let request: serde_json::Value = serde_json::from_str(&requests.last().unwrap().body).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "system")
            .count(),
        1
    );
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("This summary captures work done"))
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "user" && message["content"] == "prompt four marker"
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "assistant" && message["content"] == "answer four marker"
    }));
    assert!(messages.iter().any(|message| {
        message["role"] == "user" && message["content"] == "post compaction prompt marker"
    }));
    assert!(!messages.iter().any(|message| {
        message["role"] == "assistant"
            && matches!(
                message["content"].as_str(),
                Some("answer one marker" | "answer two marker" | "answer three marker")
            )
    }));
}

#[test]
fn new_continue_and_explicit_resume_select_the_requested_history() {
    let server = MockServer::start(vec![
        text_response("explicit first answer"),
        text_response("explicit second answer"),
        text_response("continued second answer"),
        text_response("explicit resumed answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("explicit first prompt");
    tui.wait_for("explicit first answer", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    let first_path = fixture.session_files().into_iter().next().unwrap();
    let first_id = first_path
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    tui.submit("/new");
    tui.submit("explicit second prompt");
    tui.wait_for("explicit second answer", WAIT);
    fixture.wait_for_event_count("turn_end", 2);
    assert_eq!(fixture.session_files().len(), 2);
    tui.submit("/quit");
    tui.wait_exit();

    let mut continued = fixture.spawn(&["--continue"]);
    continued.submit("continued second prompt");
    continued.wait_for("continued second answer", WAIT);
    continued.submit("/quit");
    continued.wait_exit();

    let mut resumed = fixture.spawn(&["--resume", &first_id]);
    resumed.submit("explicit resumed prompt");
    resumed.wait_for("explicit resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let continued = &requests[2].body;
    assert!(continued.contains("explicit second prompt"));
    assert!(continued.contains("explicit second answer"));
    assert!(continued.contains("continued second prompt"));
    assert!(!continued.contains("explicit first prompt"));

    let explicit = &requests[3].body;
    assert!(explicit.contains("explicit first prompt"));
    assert!(explicit.contains("explicit first answer"));
    assert!(explicit.contains("explicit resumed prompt"));
    assert!(!explicit.contains("explicit second prompt"));
    assert!(!explicit.contains("explicit second answer"));
}

#[test]
fn new_session_kills_jobs_and_drops_old_job_notices() {
    let server = MockServer::start(vec![
        tool_response(
            "new-session-job",
            r#"return await lofi.jobSpawn({ cmd: "sleep 60" });"#,
        ),
        text_response("new session job started"),
        text_response("new session clean answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("start the old session job");
    tui.wait_for("new session job started", WAIT);
    let pid = spawned_pid(&fixture);
    let mut job = ProcessGuard::new(pid);
    assert!(process_is_alive(pid));

    tui.submit("/new");
    wait_for_process_exit(pid);
    job.disarm();
    tui.submit("new session prompt");
    tui.wait_for("new session clean answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let new_session = &requests[2].body;
    assert!(new_session.contains("new session prompt"));
    assert!(!new_session.contains("start the old session job"));
    assert!(!new_session.contains("sleep 60"));
    assert!(!new_session.contains("cancelled:"));
}

#[test]
fn resume_picker_kills_current_session_jobs_and_drops_their_notices() {
    let server = MockServer::start(vec![
        text_response("resume job saved answer"),
        tool_response(
            "resume-current-job",
            r#"return await lofi.jobSpawn({ cmd: "echo $$ > resume-job.pid; exec sleep 60" });"#,
        ),
        text_response("resume current job started"),
        text_response("resume job clean answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("resume job saved prompt");
    tui.wait_for("resume job saved answer", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/new");
    tui.submit("start the resume current job");
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"a");
    tui.wait_for("resume current job started", WAIT);
    fixture.wait_for_event_count("turn_end", 2);
    let pid: i32 = std::fs::read_to_string(fixture.workspace.join("resume-job.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut job = ProcessGuard::new(pid);
    assert!(process_is_alive(pid));

    tui.submit("/resume");
    tui.wait_for("Resume a session", WAIT);
    tui.send(b"\x1b[B\r");
    wait_for_process_exit(pid);
    job.disarm();
    tui.submit("resume job final prompt");
    tui.wait_for("resume job clean answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let resumed = &requests[3].body;
    assert!(resumed.contains("resume job saved prompt"));
    assert!(resumed.contains("resume job final prompt"));
    assert!(!resumed.contains("start the resume current job"));
    assert!(!resumed.contains("sleep 60"));
    assert!(!resumed.contains("cancelled:"));
}

#[test]
fn explicit_model_overrides_the_model_restored_from_the_session() {
    let server = MockServer::start(vec![
        text_response("model override saved answer"),
        text_response("model override resumed answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&["--model", "mock/alt:high"]);

    first.submit("model override saved prompt");
    first.wait_for("model override saved answer", WAIT);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue", "--model", "mock/chat:low"]);
    resumed.submit("model override resumed prompt");
    resumed.wait_for("model override resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let request: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(request["model"], "chat");
    assert_eq!(request["reasoning_effort"], "low");
    assert!(requests[1].body.contains("model override saved prompt"));
    assert!(requests[1].body.contains("model override resumed prompt"));
}

#[test]
fn saved_transcript_remains_available_when_no_model_is_configured() {
    let server = MockServer::start(vec![text_response("no model saved answer")]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("no model saved prompt");
    first.wait_for("no model saved answer", WAIT);
    first.submit("/quit");
    first.wait_exit();

    std::fs::write(&fixture.config, "[providers]\n").unwrap();
    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.wait_for_scrollback("No models configured.", WAIT);
    resumed.wait_for("no model saved answer", WAIT);
    resumed.clear_output();
    resumed.submit("prompt while resumed without a model");
    resumed.wait_for("No models configured.", WAIT);

    assert_eq!(server.request_count(), 1);
    assert_eq!(fixture.session_files().len(), 1);
}

#[test]
fn resume_picker_switches_to_a_saved_session_and_restores_its_model() {
    let server = MockServer::start(vec![
        text_response("picker saved answer"),
        text_response("picker current answer"),
        text_response("picker resumed answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "mock/alt:high"]);

    tui.submit("picker saved prompt");
    tui.wait_for("picker saved answer", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    tui.submit("/new");
    tui.submit("/model");
    tui.wait_for("Switch model", WAIT);
    tui.send(b"\x1b[B\r");
    tui.wait_for("switched to mock/chat", WAIT);
    tui.submit("picker current prompt");
    tui.wait_for("picker current answer", WAIT);
    fixture.wait_for_event_count("turn_end", 2);

    tui.clear_output();
    tui.submit("/resume");
    tui.wait_for("Resume a session", WAIT);
    tui.send(b"\x1b[B\r");
    std::thread::sleep(std::time::Duration::from_millis(150));
    tui.submit("picker resumed prompt");
    tui.wait_for("picker resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let body: serde_json::Value = serde_json::from_str(&requests[2].body).unwrap();
    assert_eq!(body["model"], "alt");
    let raw = &requests[2].body;
    assert!(raw.contains("picker saved prompt"));
    assert!(!raw.contains("picker current prompt"));
}

#[test]
fn hard_context_pressure_compacts_and_silently_continues_the_tool_cycle() {
    let server = MockServer::start(vec![
        text_response("hard setup answer one"),
        text_response("hard setup answer two"),
        text_response("hard setup answer three"),
        tool_response_with_usage(
            "hard-pressure-call",
            "return { marker: \"hard pressure tool result\" };",
            90,
        ),
        text_response("hard pressure final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let current = std::fs::read_to_string(&fixture.config).unwrap();
    let current = current.replacen("context_window = 100000", "context_window = 100", 1);
    std::fs::write(
        &fixture.config,
        format!("[compaction]\nreserved_context_tokens = 20\n\n{current}"),
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    for (prompt, answer) in [
        ("hard setup prompt one", "hard setup answer one"),
        ("hard setup prompt two", "hard setup answer two"),
        ("hard setup prompt three", "hard setup answer three"),
    ] {
        tui.submit(prompt);
        tui.wait_for(answer, WAIT);
        tui.clear_output();
    }
    tui.submit("hard pressure prompt");
    tui.wait_for("hard pressure final answer", WAIT);

    assert_eq!(server.request_count(), 5);
    assert!(fixture
        .events()
        .iter()
        .any(|event| event["type"] == "compaction"));
    let request: serde_json::Value =
        serde_json::from_str(&server.requests().last().unwrap().body).unwrap();
    let messages = request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("This summary captures work done"))
    }));
    assert!(messages.iter().any(|message| message["role"] == "tool"));
}

#[test]
fn queued_job_completion_survives_escape_cancellation_of_a_foreground_tool() {
    let server = MockServer::start(vec![
        tool_response(
            "race-tools",
            r#"const job = await lofi.jobSpawn({ cmd: "while [ ! -f foreground-tool.pid ]; do sleep 0.02; done; sleep 0.2; printf queued-job-log-marker" });
const foreground = await lofi.bash({ cmd: "echo $$ > foreground-tool.pid; exec sleep 3600" });
return { job, foreground };"#,
        ),
        text_response("queued job notice handled after cancellation"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("start a background job and then run a long foreground command");
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"a");
    let pid_path = fixture.workspace.join("foreground-tool.pid");
    let started = std::time::Instant::now();
    while !pid_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        pid_path.exists(),
        "foreground tool did not start; requests: {}; output: {:?}",
        server.request_count(),
        tui.output()
    );
    let pid: i32 = std::fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut foreground = ProcessGuard::new(pid);
    assert!(process_is_alive(pid));

    // Wait until the background completion is visibly queued behind the
    // foreground tool. Escape must cancel only the active run, not this
    // already queued system notice.
    tui.wait_for("completed:", WAIT);
    tui.send(b"");
    wait_for_process_exit(pid);
    foreground.disarm();
    tui.wait_for("queued job notice handled after cancellation", WAIT);
    fixture.wait_for_event_count("turn_cancelled", 1);
    fixture.wait_for_event_count("turn_end", 1);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains(" completed:"));
    assert!(requests[1].body.contains("queued-job-log-marker"));
    let events = fixture.events();
    assert!(events.iter().any(|event| event["type"] == "turn_cancelled"));
    assert_eq!(job_events(&events, "job_started").len(), 1);
    assert_eq!(job_events(&events, "job_finished").len(), 1);
}

#[test]
fn background_job_completion_is_injected_as_a_notice_prompt() {
    let server = MockServer::start(vec![
        tool_response(
            "notified-job-call",
            r#"return await lofi.jobSpawn({ cmd: "sleep 0.3; printf notification-log-marker" });"#,
        ),
        text_response("notified job initial answer"),
        text_response("notified job follow-up answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("start a job with completion notification");
    tui.wait_for("notified job initial answer", WAIT);
    tui.wait_for("notified job follow-up answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let notice = &requests[2].body;
    assert!(notice.contains(" completed: sleep 0.3"), "{notice}");
    assert!(notice.contains("notification-log-marker"));
    let request: serde_json::Value = serde_json::from_str(notice).unwrap();
    let latest = request["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(latest["role"], "user");
    let notice_text = latest["content"].as_str().unwrap();
    assert!(notice_text.starts_with("job "));
    assert!(notice_text.contains(" completed: sleep 0.3"));
    let events = fixture.events();
    assert_eq!(job_events(&events, "job_started").len(), 1);
    assert_eq!(job_events(&events, "job_finished").len(), 1);
}

#[test]
fn malformed_final_event_is_ignored_and_the_prior_turn_still_resumes() {
    let server = MockServer::start(vec![
        text_response("corruption baseline answer"),
        text_response("corruption recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("corruption baseline prompt");
    first.wait_for("corruption baseline answer", WAIT);
    first.submit("/quit");
    first.wait_exit();
    let path = fixture.session_files().into_iter().next().unwrap();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, r#"{{"id":"corrupt-tail","parent_id":"missing","type":"message","role":"user","blocks":[{{"type":"text","text":"corrupt final prompt"}}]"#)
        .unwrap();
    file.sync_all().unwrap();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("corruption recovery prompt");
    resumed.wait_for("corruption recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("corruption baseline prompt"));
    assert!(requests[1].body.contains("corruption baseline answer"));
    assert!(requests[1].body.contains("corruption recovery prompt"));
    assert!(!requests[1].body.contains("corrupt final prompt"));
}

#[test]
fn transcript_without_cursor_falls_back_to_the_latest_complete_turn() {
    let server = MockServer::start(vec![
        text_response("cursor baseline answer"),
        text_response("cursor recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("cursor baseline prompt");
    first.wait_for("cursor baseline answer", WAIT);
    first.submit("/quit");
    first.wait_exit();
    let path = fixture.session_files().into_iter().next().unwrap();
    let without_cursor = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .filter(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .is_none_or(|event| event["type"] != "cursor")
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&path, format!("{without_cursor}\n")).unwrap();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("cursor recovery prompt");
    resumed.wait_for("cursor recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("cursor baseline prompt"));
    assert!(requests[1].body.contains("cursor baseline answer"));
    assert!(requests[1].body.contains("cursor recovery prompt"));
}

#[test]
fn malformed_and_unsupported_transcripts_do_not_appear_in_session_listings() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let sessions = fixture.state.join("lofi").join("sessions");
    let workspace_dir = sessions.join("malformed-fixture");
    std::fs::create_dir_all(&workspace_dir).unwrap();
    std::fs::write(workspace_dir.join("malformed.jsonl"), "{not-json}\n").unwrap();
    std::fs::write(
        workspace_dir.join("unsupported.jsonl"),
        format!(
            r#"{{"type":"meta","version":999,"created":0,"cwd":"{}","model":"mock/chat:medium"}}
"#,
            fixture.workspace.display()
        ),
    )
    .unwrap();

    let output = fixture.output(&["--list-sessions"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "(no sessions)\n");
    assert_eq!(server.request_count(), 0);
}

#[test]
fn session_state_permissions_are_private_after_process_startup() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start(vec![text_response("permissions answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("permissions prompt");
    tui.wait_for("permissions answer", WAIT);
    tui.submit("/quit");
    tui.wait_exit();

    let path = fixture.session_files().into_iter().next().unwrap();
    let workspace_dir = path.parent().unwrap();
    let sessions = workspace_dir.parent().unwrap();
    let state = sessions.parent().unwrap();
    for dir in [state, sessions, workspace_dir] {
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "{}",