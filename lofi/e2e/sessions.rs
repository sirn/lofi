use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

use crate::support::{
    delayed_text_response, event_types, job_events, process_is_alive, responses_response,
    spawned_pid, text_response, text_response_with_usage, tool_response, tool_response_with_usage,
    transcript_text, wait_for_process_exit, Fixture, MockResponse, MockServer, ProcessGuard, WAIT,
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
    use std::io::Write;
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
fn unsupported_and_malformed_transcripts_fail_without_starting_the_tui() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let sessions = fixture.state.join("lofi").join("sessions");
    let workspace_dir = sessions.join(
        fixture
            .session_files()
            .into_iter()
            .next()
            .and_then(|path| {
                path.parent()
                    .and_then(|parent| parent.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| "workspace".to_string()),
    );
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let malformed = workspace_dir.join("malformed.jsonl");
    std::fs::write(&malformed, "{not-json}\n").unwrap();
    let output = fixture.output(&["--list-sessions"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "(no sessions)\n");

    let unsupported = workspace_dir.join("unsupported.jsonl");
    std::fs::write(
        &unsupported,
        format!(
            r#"{{"type":"meta","version":999,"created":0,"cwd":"{}","model":"mock/chat:medium"}}
"#,
            fixture.workspace.display()
        ),
    )
    .unwrap();
    let output = fixture.output(&["--list-sessions"]);
    assert!(output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("unsupported"));
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
            dir.display()
        );
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn startup_repairs_broad_state_permissions_and_keeps_session_content() {
    use std::os::unix::fs::PermissionsExt;

    let server = MockServer::start(vec![
        text_response("permissions baseline answer"),
        text_response("permissions recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    first.submit("permissions baseline prompt");
    first.wait_for("permissions baseline answer", WAIT);
    first.submit("/quit");
    first.wait_exit();

    let path = fixture.session_files().into_iter().next().unwrap();
    let workspace_dir = path.parent().unwrap();
    let sessions = workspace_dir.parent().unwrap();
    std::fs::set_permissions(state_dir(&fixture), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(sessions, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(workspace_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("permissions recovery prompt");
    resumed.wait_for("permissions recovery answer", WAIT);

    for dir in [state_dir(&fixture), sessions, workspace_dir] {
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "{}",
            dir.display()
        );
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("permissions baseline prompt"));
    assert!(requests[1].body.contains("permissions recovery prompt"));
}

#[test]
fn startup_collects_an_abandoned_temp_dir_and_preserves_a_locked_one() {
    use std::os::unix::fs::OpenOptionsExt;

    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let tmp = fixture.state.join("lofi").join("tmp");
    let workspace = tmp.join(
        fixture
            .workspace
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
    );
    let abandoned = workspace.join("abandoned-session-output");
    std::fs::create_dir_all(&abandoned).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(abandoned.join(".lease"))
        .unwrap();
    let live = workspace.join("live-session-output");
    std::fs::create_dir_all(&live).unwrap();
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(live.join(".lease"))
        .unwrap();
    use std::os::fd::AsRawFd;
    let result = unsafe { nix::libc::flock(lease.as_raw_fd(), nix::libc::LOCK_EX) };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());

    let mut tui = fixture.spawn(&[]);
    tui.submit("/quit");
    tui.wait_exit();

    assert!(!abandoned.exists());
    assert!(live.exists());
    drop(lease);
}

fn state_dir(fixture: &Fixture) -> &std::path::Path {
    fixture.state.join("lofi").leak()
}
