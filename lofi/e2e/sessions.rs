use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::support::{
    job_events, process_is_alive, spawned_pid, text_response, tool_response, wait_for_process_exit,
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
    tui.wait_for("user: loading", WAIT);
    tui.wait_for("exec: jobSpawn sleep 60", WAIT);
    tui.send(b"\x1b[A\r");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(process_is_alive(pid));

    tui.clear_output();
    tui.submit("/tree");
    tui.wait_for("user: loading", WAIT);
    tui.wait_for("exec: jobSpawn sleep 60", WAIT);
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
fn manual_compaction_and_recall_survive_in_the_session() {
    let server = MockServer::start(vec![
        text_response("answer one marker"),
        text_response("answer two marker"),
        text_response("answer three marker"),
        text_response("answer four marker"),
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
    tui.clear_output();
    tui.submit("/recall prompt one marker");
    tui.wait_for("prompt one marker", WAIT);

    let events = fixture.events();
    assert!(events.iter().any(|event| event["type"] == "compaction"));
}
