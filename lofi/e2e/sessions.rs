use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::support::{
    job_events, process_is_alive, responses_response, spawned_pid, text_response,
    text_response_with_usage, tool_response, tool_response_with_usage, wait_for_process_exit,
    Fixture, MockServer, ProcessGuard, WAIT,
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

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let branch = &requests[2].body;
    assert!(branch.contains("rollback first prompt"));
    assert!(branch.contains("rollback first answer"));
    assert!(branch.contains("rollback replacement prompt"));
    assert!(!branch.contains("rollback second prompt"));
    assert!(!branch.contains("rollback second answer"));

    let transcript = crate::support::transcript_text(&fixture.events());
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
fn new_session_and_explicit_resume_select_the_requested_history() {
    let server = MockServer::start(vec![
        text_response("explicit first answer"),
        text_response("explicit second answer"),
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

    let mut resumed = fixture.spawn(&["--resume", &first_id]);
    resumed.submit("explicit resumed prompt");
    resumed.wait_for("explicit resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    let body = &requests[2].body;
    assert!(body.contains("explicit first prompt"));
    assert!(body.contains("explicit first answer"));
    assert!(body.contains("explicit resumed prompt"));
    assert!(!body.contains("explicit second prompt"));
    assert!(!body.contains("explicit second answer"));
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
