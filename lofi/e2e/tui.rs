use nix::sys::signal::Signal;
use serde_json::Value;

use crate::support::{
    delayed_text_response, delayed_tool_response, process_is_alive, spawned_pid, text_response,
    thinking_tool_response, tool_response, transcript_text, wait_for_process_exit, Fixture,
    MockServer, ProcessGuard, Tui, WAIT,
};

fn enter_navigation(tui: &mut Tui) {
    tui.send(b"\t");
    std::thread::sleep(std::time::Duration::from_millis(50));
}

fn wait_tinted_row(tui: &Tui, needle: &str) {
    let start = std::time::Instant::now();
    while start.elapsed() < WAIT {
        if tui
            .tinted_row_text()
            .as_deref()
            .is_some_and(|row| row.contains(needle))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "could not focus {needle:?}; current: {:?}; screen:\n{}",
        tui.tinted_row_text(),
        tui.screen_text()
    );
}

fn wait_select_cursor_row(tui: &Tui, needle: &str) {
    let start = std::time::Instant::now();
    while start.elapsed() < WAIT {
        if tui
            .select_cursor_row_text()
            .as_deref()
            .is_some_and(|row| row.contains(needle))
        {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "could not focus select cursor on {needle:?}; current: {:?}; screen:\n{}",
        tui.select_cursor_row_text(),
        tui.screen_text()
    );
}

fn select_transcript_row(tui: &mut Tui, needle: &str) {
    for _ in 0..40 {
        let current = tui.tinted_row_text();
        if current.as_deref().is_some_and(|row| row.contains(needle)) {
            return;
        }
        let rows = tui.screen_text();
        let rows = rows.lines().collect::<Vec<_>>();
        let target = rows.iter().position(|row| row.contains(needle));
        if let Some((target, current)) = target.zip(tui.tinted_row_index()) {
            tui.send(if target < current { b"k" } else { b"j" });
        } else {
            tui.send(b"k");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "could not select transcript row {needle:?}; current: {:?}; screen:\n{}",
        tui.tinted_row_text(),
        tui.screen_text()
    );
}

#[test]
fn expanded_transcript_details_are_independent_adaptive_and_styled() {
    let server = MockServer::start(vec![
        thinking_tool_response(
            "thinking-detail-one\nthinking-detail-two",
            "detail-e2e-call",
            r#"await lofi.bash({ cmd: "printf 'left-%s\\nright-%s\\n' 1 2" });
return "detail-executive-one\\ndetail-executive-two";"#,
        ),
        text_response("detail first answer marker"),
        text_response("detail freezing answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("detail e2e prompt");
    tui.wait_for("detail first answer marker", WAIT);
    tui.submit("freeze the detail e2e turn");
    tui.wait_for("detail freezing answer marker", WAIT);

    for row in ["Exec", "Tool bash", "Succeed"] {
        tui.screen_row(row)
            .unwrap_or_else(|| panic!("missing {row} row; screen:\n{}", tui.screen_text()));
    }

    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Exec");
    tui.send(b"\r");
    tui.wait_for("await lofi.bash", WAIT);
    tui.send(b"\r");
    std::thread::sleep(std::time::Duration::from_millis(50));

    select_transcript_row(&mut tui, "Tool bash");
    tui.send(b"\r");
    tui.wait_for_screen("left-1", WAIT);
    tui.wait_for_screen("right-2", WAIT);

    let screen = tui.screen_text();
    let rows = screen.lines().collect::<Vec<_>>();
    let bash = rows
        .iter()
        .position(|row| row.contains("Tool bash"))
        .unwrap();
    let result = rows.iter().position(|row| row.contains("Succeed")).unwrap();
    assert_eq!(
        result - bash,
        3,
        "a two-line native detail must render exactly two rows:\n{screen}"
    );
    assert!(tui.row_has_raised_background("left-1"));
    let detail_rows = rows
        .iter()
        .filter(|row| row.contains("left-1") || row.contains("right-2"))
        .collect::<Vec<_>>();
    assert_eq!(detail_rows.len(), 2);
    assert!(detail_rows.iter().all(|row| !row.contains('┌')));
    assert!(detail_rows.iter().all(|row| !row.contains('└')));
    // The box fits on screen, so no scrollbar line is drawn: no track (│)
    // and no border; the box look comes from the raised background alone.
    assert!(detail_rows.iter().all(|row| {
        let start = row.find("left-").or_else(|| row.find("right-")).unwrap();
        !row[start..].contains('│') && !row[start..].contains('┃')
    }));
    tui.send(b"\r");
    std::thread::sleep(std::time::Duration::from_millis(50));

    tui.wait_for_screen("thinking-detail-one", WAIT);
    assert!(
        tui.screen_row("thinking-detail-one")
            .is_some_and(|row| !row.contains('▸')),
        "thinking must stay inline without expansion; screen:\n{}",
        tui.screen_text()
    );
    assert!(
        !tui.row_has_raised_background("thinking-detail-one"),
        "thinking must stay on the transcript background; screen:\n{}",
        tui.screen_text()
    );
    assert!(tui.row_has_italic_text("thinking-detail-one"));
}

#[test]
fn expanded_detail_is_modal_and_bounded_to_ten_rows() {
    let detail_code = r#"
// mini-exec-00
// mini-exec-01
// mini-exec-02
// mini-exec-03
// mini-exec-04
// mini-exec-05
// mini-exec-06
// mini-exec-07
// mini-exec-08
// mini-exec-09
// mini-exec-10
// mini-exec-11
// mini-exec-12
// mini-exec-13
// mini-exec-14
return "mini-done";
    "#
    .trim();
    let server = MockServer::start(vec![
        tool_response("mini-detail-call", detail_code),
        text_response("mini detail answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("expand a mini detail");
    tui.wait_for("mini detail answer marker", WAIT);
    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Exec");
    tui.send(b"\r");
    wait_tinted_row(&tui, "mini-exec-00");
    assert!(
        !tui.screen_text().contains("mini-exec-10"),
        "screen:\n{}",
        tui.screen_text()
    );
    assert!(
        tui.select_cursor_row_text()
            .as_deref()
            .is_some_and(|row| row.contains("mini-exec-00")),
        "a head detail marks its cursor cell on the first row: {:?}",
        tui.select_cursor_row_text()
    );

    tui.send(b"j");
    wait_tinted_row(&tui, "mini-exec-01");
    tui.send(b"\x1b[6~");
    wait_tinted_row(&tui, "mini-exec-11");
    assert!(tui.screen_text().contains("mini-exec-10"));
    assert!(
        !tui.screen_text().contains("mini-exec-00"),
        "screen:\n{}",
        tui.screen_text()
    );
    tui.send(b"\x1b[1;2B");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(tui
        .tinted_row_text()
        .as_deref()
        .is_some_and(|row| row.contains("mini-exec-11")));
    tui.send(b"v");
    tui.send(b"j");
    wait_select_cursor_row(&tui, "mini-exec-12");
    tui.send(b"\r");
    let deadline = std::time::Instant::now() + WAIT;
    while tui.screen_text().contains("mini-exec-12") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    wait_tinted_row(&tui, "Exec");
    assert!(!tui.screen_text().contains("mini-exec-12"));
}

#[test]
fn tail_detail_expansion_focuses_its_last_row_and_esc_collapses_it() {
    let lines = (0..20)
        .map(|n| format!("tail-res-{n:02}"))
        .collect::<Vec<_>>()
        .join("\\n");
    let server = MockServer::start(vec![
        tool_response("tail-detail-call", &format!("return \"{lines}\";")),
        text_response("tail detail answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("expand a tail detail");
    tui.wait_for("tail detail answer marker", WAIT);
    assert!(
        !tui.screen_text().contains("\u{25b8}"),
        "screen:\n{}",
        tui.screen_text()
    );
    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Succeed");
    tui.send(b"\r");
    wait_tinted_row(&tui, "tail-res-19");
    assert!(
        tui.select_cursor_row_text()
            .as_deref()
            .is_some_and(|row| row.contains("tail-res-19")),
        "a tail detail marks its cursor cell on the last row: {:?}",
        tui.select_cursor_row_text()
    );
    assert!(!tui.screen_text().contains("tail-res-00"));

    tui.send(b"i");
    let deadline = std::time::Instant::now() + WAIT;
    while tui.screen_text().contains("tail-res-19") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !tui.screen_text().contains("tail-res-19"),
        "leaving for the input area must collapse the detail:\n{}",
        tui.screen_text()
    );
}

#[test]
fn yanking_a_detail_selection_collapses_the_expansion() {
    let server = MockServer::start(vec![
        tool_response("yank-detail-call", "// yank-detail-one\n// yank-detail-two"),
        text_response("yank detail answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("expand a yank detail");
    tui.wait_for("yank detail answer marker", WAIT);
    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Exec");
    tui.send(b"\r");
    wait_tinted_row(&tui, "yank-detail-one");
    tui.send(b"vj");
    wait_select_cursor_row(&tui, "yank-detail-two");
    tui.send(b"y");
    tui.wait_for("Copied to clipboard", WAIT);
    assert!(
        !tui.screen_text().contains("yank-detail-one"),
        "copy-to-input must collapse the detail:\n{}",
        tui.screen_text()
    );
}

#[test]
fn expanded_detail_copy_uses_transcript_selection_content() {
    let server = MockServer::start(vec![
        tool_response(
            "copy-detail-call",
            "// copy-detail-one
// copy-detail-two",
        ),
        text_response("copy detail answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("copy an expanded detail");
    tui.wait_for("copy detail answer marker", WAIT);
    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Exec");
    tui.send(b"\r");
    wait_tinted_row(&tui, "copy-detail-one");
    tui.clear_output();

    tui.send(b"vj$y");
    tui.wait_for("Copied to clipboard", WAIT);

    let output = tui.output();
    assert!(
        output.contains("\x1b]52;c;Ly8gY29weS1kZXRhaWwtb25lCi8vIGNvcHktZGV0YWlsLXR3bw==\x07"),
        "terminal output: {output:?}"
    );
}

#[test]
fn esc_collapses_an_expanded_detail_without_cancelling_the_turn() {
    // The second response arrives late so the turn is still running while the
    // first tool result is expanded; Escape must peel the detail, not abort.
    let server = MockServer::start(vec![
        tool_response("esc-detail-call", "return \"esc-detail-payload\";"),
        delayed_text_response("esc detail survived", std::time::Duration::from_secs(3)),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("expand a detail while the turn runs");
    tui.wait_for("Succeed", WAIT);
    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Exec");
    tui.send(b"\r");
    wait_tinted_row(&tui, "esc-detail-payload");

    tui.send(b"\x1b");
    let deadline = std::time::Instant::now() + WAIT;
    while tui.screen_text().contains("esc-detail-payload") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !tui.screen_text().contains("esc-detail-payload"),
        "Escape must collapse the focused detail first:\n{}",
        tui.screen_text()
    );
    tui.wait_for("esc detail survived", WAIT);
}

#[test]
fn tab_collapses_an_expanded_detail_without_leaving_navigation() {
    let server = MockServer::start(vec![
        tool_response("tab-detail-call", "return \"tab-detail-payload\";"),
        text_response("tab detail answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("expand a detail");
    tui.wait_for("tab detail answer marker", WAIT);
    enter_navigation(&mut tui);
    select_transcript_row(&mut tui, "Exec");
    tui.send(b"\r");
    wait_tinted_row(&tui, "tab-detail-payload");

    tui.send(b"\t");
    let deadline = std::time::Instant::now() + WAIT;
    while tui.screen_text().contains("tab-detail-payload") && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !tui.screen_text().contains("tab-detail-payload"),
        "Tab must collapse the focused detail:\n{}",
        tui.screen_text()
    );
    // Nav keys still work, so the session stayed in Navigate mode.
    tui.send(b"\r");
    wait_tinted_row(&tui, "tab-detail-payload");
}

#[test]
fn quit_persists_queued_prompts_instead_of_dropping_them() {
    // The queue is memory-only until a run drains it; quitting with a
    // queued prompt must persist it as an un-run turn, not drop it.
    let server =
        MockServer::start(vec![text_response("slow first answer marker")
            .with_delay(std::time::Duration::from_millis(800))]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("first prompt marker");
    tui.submit("queued while running marker");
    tui.wait_for_screen("Queue: queued while running marker", WAIT);
    tui.submit("/exit");
    tui.wait_exit();

    let reloaded = fixture.spawn(&["--continue"]);
    reloaded.wait_for_screen("first prompt marker", WAIT);
    reloaded.wait_for_screen("queued while running marker", WAIT);
}

#[test]
fn reloaded_details_stay_collapsed_before_any_expansion() {
    let server = MockServer::start(vec![
        tool_response(
            "reloaded-detail-call",
            r#"await lofi.read("reloaded-hidden-detail.txt");
return "exec-result-reload-one\nexec-result-reload-two";"#,
        ),
        text_response("reloaded detail answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    std::fs::write(
        fixture.workspace.join("reloaded-hidden-detail.txt"),
        "reloaded-hidden-first\nreloaded-hidden-second\n",
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("persist a reloaded detail prompt");
    tui.wait_for("reloaded detail answer marker", WAIT);
    tui.submit("/exit");
    tui.wait_exit();

    let mut reloaded = fixture.spawn(&["--continue"]);
    reloaded.wait_for_screen("persist a reloaded detail prompt", WAIT);
    reloaded.wait_for_screen("Tool read", WAIT);
    for row in ["Tool read", "Succeed"] {
        reloaded
            .screen_row(row)
            .unwrap_or_else(|| panic!("missing {row} row; screen:\n{}", reloaded.screen_text()));
    }
    assert!(!reloaded.screen_text().contains("exec-result-reload-one"));
    assert!(!reloaded.screen_text().contains("reloaded-hidden-first"));

    enter_navigation(&mut reloaded);
    select_transcript_row(&mut reloaded, "Succeed");
    reloaded.send(b"\r");
    reloaded.wait_for_screen("exec-result-reload-one", WAIT);
    reloaded.wait_for_screen("exec-result-reload-two", WAIT);
    assert!(
        !reloaded.screen_text().contains("reloaded-hidden-first"),
        "expanding Succeed must not reveal the read detail"
    );
}

#[test]
fn markdown_strikethrough_uses_double_tildes_only() {
    let server = MockServer::start(vec![
        // Single tildes are literal text (~$250), not strikethrough. The
        // closing tilde sits after ** so GFM flanking accepts the pair —
        // pre-fix this whole span rendered struck.
        text_response("So your ~$250 ballpark is right — call it **~$240–250/mo**."),
        // Separate lines: the row-level style probe cannot split one row.
        text_response("plain-marker tail\n~~double-tilde-marker~~"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("single tilde prompt marker");
    tui.wait_for("$250 ballpark", WAIT);
    assert!(
        !tui.row_has_crossed_out_text("$250 ballpark"),
        "a wide single-tilde pair must not strike the text between; screen:\n{}",
        tui.screen_text()
    );
    assert!(
        !tui.row_has_crossed_out_text("$240"),
        "the closing single tilde must not strike; screen:\n{}",
        tui.screen_text()
    );

    tui.submit("double tilde prompt marker");
    tui.wait_for("double-tilde-marker", WAIT);
    assert!(
        tui.row_has_crossed_out_text("double-tilde-marker"),
        "~~ must still strike through; screen:\n{}",
        tui.screen_text()
    );
    assert!(
        !tui.row_has_crossed_out_text("plain-marker"),
        "text outside the ~~ span must not strike; screen:\n{}",
        tui.screen_text()
    );
}

#[test]
fn user_shell_context_marker_controls_the_next_model_request() {
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
fn user_shell_output_is_expanded_by_default() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    // Joined at runtime, so the marker only matches rendered command output,
    // never the echoed `$ ` command line itself.
    tui.submit("!printf 'shell-out''put-expanded-marker'");
    tui.wait_for("shell-output-expanded-marker", WAIT);
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
fn topmost_policy_confirmation_handles_input_before_tree_picker() {
    let server = MockServer::start(vec![
        text_response("overlap seed answer"),
        delayed_tool_response(
            "overlap-policy-call",
            "return await lofi.bash({ cmd: \"touch should-not-run\" });",
            std::time::Duration::from_millis(500),
        ),
        text_response("overlap policy denied answer"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("overlap seed prompt");
    tui.wait_for("overlap seed answer", WAIT);
    tui.submit("request a command that needs confirmation");
    let started = std::time::Instant::now();
    while server.request_count() < 2 && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(server.request_count(), 2);

    tui.submit("/tree");
    tui.wait_for("Roll back to a turn", WAIT);
    tui.wait_for("Permission Required", WAIT);
    tui.send(b"da");
    let started = std::time::Instant::now();
    while server.request_count() < 3 && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(server.request_count(), 3);
    tui.send(b"\x1b");
    tui.wait_for("overlap policy denied answer", WAIT);
    assert!(!fixture.workspace.join("should-not-run").exists());
}

#[test]
fn policy_dialog_deny_all_blocks_exec_without_prompting() {
    let server = MockServer::start(vec![
        tool_response(
            "policy-call",
            "return await lofi.bash({ cmd: \"touch should-not-run\" });",
        ),
        text_response("policy deny all answer"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("/policy");
    tui.wait_for("Bash policy", WAIT);
    // Without auto mode the rows are allow all / ask (manual) / deny all.
    tui.send(b"\x1b[B\x1b[B\r");
    tui.wait_for("policy: deny all", WAIT);

    tui.submit("run the touch please");
    tui.wait_for("policy deny all answer", WAIT);

    // No prompt appeared: the override blocked the exec outright and the
    // denial result went back to the model.
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests[1].body.contains("deny all"),
        "{}",
        requests[1].body
    );
    assert!(!fixture.workspace.join("should-not-run").exists());
}

#[test]
fn policy_dialog_allow_all_runs_exec_without_prompting() {
    let server = MockServer::start(vec![
        tool_response(
            "policy-call",
            "return await lofi.bash({ cmd: \"touch should-run\" });",
        ),
        text_response("policy allow all answer"),
    ]);
    let fixture = Fixture::with_policy(&server, "confirm");
    let mut tui = fixture.spawn(&[]);

    tui.submit("/policy");
    tui.wait_for("Bash policy", WAIT);
    // The dialog opens pre-selected on ask (manual); one up selects allow all.
    tui.send(b"\x1b[A\r");
    tui.wait_for("policy: allow all", WAIT);

    tui.submit("run the touch please");
    tui.wait_for("policy allow all answer", WAIT);

    // Under confirm policy this exec would have prompted; allow-all skipped
    // the prompt and ran it.
    assert_eq!(server.requests().len(), 2);
    assert!(fixture.workspace.join("should-run").exists());
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
fn service_picker_changes_the_next_request() {
    let server = MockServer::start(vec![text_response("service answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("/service");
    tui.wait_for("Service tier", WAIT);
    // Rows are auto / flex / priority, pre-selected at the model's flex.
    tui.send(b"\x1b[B\r");
    tui.wait_for("switched to mock/chat:medium@priority", WAIT);

    tui.submit("service prompt marker");
    tui.wait_for("service answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    let request: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(request["service_tier"], "priority");
}

#[test]
fn mid_run_model_switch_keeps_the_working_line_model() {
    let server = MockServer::start(vec![delayed_text_response(
        "mid-run answer marker",
        std::time::Duration::from_secs(2),
    )]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("mid-run switch marker");
    tui.wait_for("Working for", WAIT);
    tui.submit("/model");
    tui.wait_for("Switch model", WAIT);
    tui.send(b"\x1b[A\r");
    tui.wait_for("switched to mock/alt", WAIT);

    let working = tui.screen_row("Working for").expect("working row");
    assert!(
        working.contains("mock/chat"),
        "working line must keep the run model: {working}"
    );
    assert!(
        !working.contains("alt"),
        "working line must not show the newly set model: {working}"
    );

    tui.wait_for("mid-run answer marker", WAIT);
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

#[test]
fn diagnostics_recall_clear_and_exit_commands_work() {
    let server = MockServer::start(vec![
        text_response("command history answer marker"),
        text_response("command history follow-up answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("command history prompt marker");
    tui.wait_for("command history answer marker", WAIT);
    tui.clear_output();
    tui.submit("/debug");
    tui.wait_for("Debug mode activated", WAIT);
    // The debug line and its status column contribution.
    tui.wait_for("Debug", WAIT);
    tui.wait_for("Peak", WAIT);
    tui.wait_for("Measured", WAIT);

    tui.clear_output();
    tui.submit("/recall command history prompt marker");
    tui.wait_for("matches", WAIT);
    tui.wait_for("command history prompt marker", WAIT);

    tui.submit("/clear");
    tui.submit("command history follow-up prompt");
    tui.wait_for("command history follow-up answer", WAIT);
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("command history prompt marker"));
    assert!(requests[1].body.contains("command history answer marker"));
    assert!(requests[1]
        .body
        .contains("command history follow-up prompt"));

    tui.submit("/exit");
    tui.wait_exit();
}

#[test]
fn jobs_modal_lists_opens_output_and_stops_a_running_job() {
    let server = MockServer::start(vec![
        tool_response(
            "modal-job-call",
            r#"return await lofi.jobSpawn({ cmd: "printf 'raw-marker\\033[2K\\rmodal-job-screen-marker'; sleep 60", tty: true, cols: 36, rows: 8, notify: false });"#,
        ),
        text_response("modal job answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("start modal job");
    tui.wait_for("modal job answer marker", WAIT);
    let mut job = ProcessGuard::new(spawned_pid(&fixture));
    assert!(process_is_alive(job.pid()));

    tui.clear_output();
    tui.submit("/job");
    tui.wait_for("background jobs", WAIT);
    tui.send(b"\r");
    tui.wait_for("modal-job-screen-marker", WAIT);
    tui.clear_output();
    tui.send(b"\x1b");
    tui.wait_for("x", WAIT);
    tui.clear_output();
    tui.send(b"x");
    tui.wait_for("this job?", WAIT);
    tui.send(b"y");
    wait_for_process_exit(job.pid());
    job.disarm();
}

#[test]
fn prompts_submitted_during_a_run_are_queued_and_sent_in_order() {
    let server = MockServer::start(vec![
        delayed_text_response(
            "queued first answer marker",
            std::time::Duration::from_millis(250),
        ),
        text_response("queued second answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("queued first prompt marker");
    std::thread::sleep(std::time::Duration::from_millis(50));
    tui.submit("queued second prompt marker");
    tui.wait_for("queued second answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body.contains("queued first prompt marker"));
    assert!(!requests[0].body.contains("queued second prompt marker"));
    assert!(requests[1].body.contains("queued first answer marker"));
    assert!(requests[1].body.contains("queued second prompt marker"));
}

#[test]
fn bracketed_multiline_paste_is_submitted_as_one_prompt() {
    let server = MockServer::start(vec![text_response("paste answer marker")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.send(b"\x1b[200~paste line one\npaste line two\x1b[201~\r");
    tui.wait_for("paste answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("paste line one\\npaste line two"));
}

#[test]
fn selecting_rendered_text_copies_the_original_markdown_over_osc52() {
    let server = MockServer::start(vec![text_response("**copy-markdown-marker**")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("render markdown for copy");
    tui.wait_for("copy-markdown-marker", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    tui.clear_output();

    tui.send(b"\tkk0v$y");
    tui.wait_for("Copied to clipboard", WAIT);

    let output = tui.output();
    assert!(
        output.contains("\x1b]52;c;Kipjb3B5LW1hcmtkb3duLW1hcmtlcioq\x07"),
        "terminal output: {output:?}"
    );
}

#[test]
fn escape_clears_input_and_ctrl_d_exits() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.send(b"input that escape must clear");
    std::thread::sleep(std::time::Duration::from_millis(100));
    tui.send(b"\x1b");
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(server.request_count(), 0);
    tui.send(b"\x04");
    tui.wait_exit();
}

#[test]
fn user_shell_output_streams_while_running() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    // The marker must render long before the command exits; a flush-on-done
    // pipeline would only show it after the 30s sleep, past WAIT.
    tui.submit("!printf 'stream-live-marker'; sleep 30");
    tui.wait_for("stream-live-marker", WAIT);
    tui.wait_for("Running", WAIT);

    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
}

#[test]
fn user_shell_cancel_keeps_the_output_streamed_so_far() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!printf cancel-keeps-marker; sleep 30");
    tui.wait_for("cancel-keeps-marker", WAIT);
    tui.wait_for("Running", WAIT);

    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
    assert!(
        tui.screen_row("cancel-keeps-marker").is_some(),
        "cancel must keep the streamed output: {}",
        tui.screen_text()
    );

    let event = fixture
        .events()
        .into_iter()
        .find(|event| event["type"] == "user_shell")
        .unwrap();
    assert_eq!(event["cancelled"], true);
    assert!(
        event["output"]
            .as_str()
            .unwrap()
            .contains("cancel-keeps-marker"),
        "{event}"
    );
}

#[test]
fn user_shell_stream_stays_ansi_stripped_and_utf8_safe_across_reads() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    // Enough volume that pipe reads split escape sequences and multi-byte
    // characters across chunk boundaries.
    tui.submit(
        "!i=0; while [ $i -lt 2000 ]; do printf '\\033[32mstrip-live-%d\\033[0m \\303\\274n\\303\\257c\\303\\266d\\303\\251-\\342\\234\\223-row\\n' $i; i=$((i+1)); done; sleep 30",
    );
    tui.wait_for("strip-live-199", WAIT);
    tui.wait_for("Running", WAIT);
    let live = tui.screen_text();
    assert!(
        !live.contains('\u{1b}') && !live.contains('\u{FFFD}'),
        "live stream must render without escapes or mangled characters: {live:?}"
    );

    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
    let event = fixture
        .events()
        .into_iter()
        .find(|event| event["type"] == "user_shell")
        .unwrap();
    let output = event["output"].as_str().unwrap();
    assert!(output.contains("strip-live-1999"), "output: {output:?}");
    assert!(output.contains("ünïcödé-✓-row"), "output: {output:?}");
    assert!(!output.contains('\u{1b}'), "output: {output:?}");
}

#[test]
fn user_shell_running_detail_follows_the_streaming_tail() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit(
        "!i=1; while [ $i -le 25 ]; do printf 'tail-follow-%02d\\n' $i; i=$((i+1)); done; printf 'TAIL-FOLLOW-LAST\\n'; sleep 30",
    );
    tui.wait_for("TAIL-FOLLOW-LAST", WAIT);
    tui.wait_for("Running", WAIT);
    assert!(
        tui.screen_row("tail-follow-01").is_none() && tui.screen_row("tail-follow-16").is_none(),
        "early rows must scroll off the tail view: {}",
        tui.screen_text()
    );
    assert!(tui
        .screen_row("tail-follow-25")
        .is_some_and(|row| !row.contains('%')));

    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
}

#[test]
fn user_shell_session_persists_only_the_finished_event() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!printf persistence-shell-marker; printf ' and-second-line'");
    tui.wait_for("Exit 0", WAIT);
    tui.send(b"\x04");
    tui.wait_exit();

    let events = fixture.events();
    let types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        types.iter().filter(|kind| **kind == "user_shell").count(),
        1
    );
    assert!(
        !types
            .iter()
            .any(|kind| *kind == "user_shell_start" || *kind == "user_shell_delta"),
        "live-only events must never be persisted: {types:?}"
    );
    let shell = events
        .iter()
        .find(|event| event["type"] == "user_shell")
        .unwrap();
    assert_eq!(shell["exit_code"], 0);
    let output = shell["output"].as_str().unwrap();
    assert!(
        output.contains("persistence-shell-marker and-second-line"),
        "{output:?}"
    );
}

#[test]
fn ctrl_c_cancels_user_shell_and_kills_its_process_group() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!echo $$ > user-shell.pid; exec sleep 60");
    let pid_path = fixture.workspace.join("user-shell.pid");
    let started = std::time::Instant::now();
    while !pid_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut guard = ProcessGuard::new(pid);
    assert!(process_is_alive(pid));

    tui.send(b"\x03");
    tui.wait_for("Cancelled", WAIT);
    wait_for_process_exit(pid);
    guard.disarm();

    let events = fixture.events();
    let bash = events
        .iter()
        .find(|event| event["type"] == "user_shell")
        .unwrap();
    assert_eq!(bash["cancelled"], true);
    assert_eq!(server.request_count(), 0);
}

#[test]
fn user_shell_records_exit_signal_and_large_output_without_blocking_shutdown() {
    let server = MockServer::start(vec![text_response("shell edge context answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("!printf nonzero-shell-marker; exit 23");
    tui.wait_for("Exit 23", WAIT);
    tui.submit("!kill -TERM $$");
    tui.wait_for("Signal 15", WAIT);
    tui.submit("!i=0; while [ $i -lt 8000 ]; do printf 'large-shell-%04d-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n' $i; i=$((i+1)); done");
    tui.wait_for("· truncated", WAIT);
    tui.submit("shell edge context prompt");
    tui.wait_for("shell edge context answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].body.contains("nonzero-shell-marker"));
    assert!(requests[0].body.contains("Command exited with code 23"));
    assert!(requests[0].body.contains("Command terminated by signal 15"));
    assert!(requests[0].body.contains("Output truncated"));
    assert!(!requests[0].body.contains("large-shell-0000"));

    tui.submit("!echo $$ > shutdown-shell.pid; printf shutdown-stream-start; sleep 60");
    let pid_path = fixture.workspace.join("shutdown-shell.pid");
    let started = std::time::Instant::now();
    while !pid_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid: i32 = std::fs::read_to_string(pid_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut guard = ProcessGuard::new(pid);
    tui.send(b"\x04");
    tui.wait_exit();
    wait_for_process_exit(pid);
    guard.disarm();
}

#[test]
fn accepting_auto_mode_dialog_keeps_the_draft_in_the_input_box() {
    let server = MockServer::start(vec![
        tool_response(
            "auto-draft-call",
            r#"return await lofi.bash({ cmd: "printf auto-draft-ran" });"#,
        ),
        delayed_text_response(
            r#"{"decision":"ask","reason":"fixture asks"}"#,
            std::time::Duration::from_millis(400),
        ),
        text_response("auto draft final answer"),
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

    tui.submit("run the auto draft tool");
    tui.wait_for("Working for", WAIT);
    // Type a draft while the run and the auto-mode evaluation are in flight.
    tui.send(b"DRAFT-NOT-A-PROMPT");
    tui.wait_for("Permission Required", WAIT);
    tui.wait_for("Auto evaluation asks", WAIT);

    tui.send(b"\r");
    tui.wait_for("auto draft final answer", WAIT);

    let row = tui
        .screen_row("DRAFT-NOT-A-PROMPT")
        .expect("draft must remain in the input box");
    assert!(row.contains('▌'), "draft row is not the input area: {row}");
    let requests = server.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body.contains("run the auto draft tool"));
    assert!(!requests[2].body.contains("DRAFT-NOT-A-PROMPT"));
    assert!(fixture
        .events()
        .iter()
        .all(|event| !event.to_string().contains("DRAFT-NOT-A-PROMPT")));
}

#[test]
fn user_shell_reading_stdin_does_not_swallow_typed_keys() {
    let server = MockServer::start(Vec::new());
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    let script = fixture.workspace.join("probe.sh");
    std::fs::write(
        &script,
        "i=0\nwhile [ $i -lt 120 ] && [ ! -e probe-got-ab ]; do sleep 0.1; read -r -t 0.05 line && printf '%s' \"$line\" > probe-got-ab; i=$((i+1)); done\nexit 0\n",
    )
    .unwrap();
    tui.submit("!sh probe.sh");
    tui.wait_for("Working for", WAIT);
    std::thread::sleep(std::time::Duration::from_millis(700));

    tui.send(b"ZZPROBE-MARKER-ZZ\r");
    tui.send(b"ab\r");
    let got_path = fixture.workspace.join("probe-got-ab");
    let started = std::time::Instant::now();
    while !got_path.exists() && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        !got_path.exists(),
        "the !-shell must not read the TUI's terminal stdin"
    );
    tui.wait_for("Exit 0", WAIT);
    tui.wait_for("ZZPROBE-MARKER-ZZ", WAIT);
}

#[test]
fn no_session_recall_and_result_report_that_persistence_is_unavailable() {
    let server = MockServer::start(vec![
        tool_response(
            "ephemeral-recovery",
            r#"const recall = await lofi.recall({ query: "anything", scope: "all" });
const result = await lofi.result("missing-event");
return { recall, result };"#,
        ),
        text_response("ephemeral recovery answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--no-session"]);

    tui.submit("try recovery without a session");
    tui.wait_for("ephemeral recovery answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].body.contains("recall unavailable"));
    assert!(requests[1].body.contains("result unavailable"));
    assert!(fixture.session_files().is_empty());
}

#[test]
fn sigterm_leaves_the_terminal_ready_for_the_next_process() {
    let server = MockServer::start(vec![text_response("signal terminal answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("signal terminal prompt");
    tui.wait_for("signal terminal answer", WAIT);
    tui.signal(Signal::SIGTERM);
    tui.wait_exit();

    let next = fixture.output(&["--list-models"]);
    assert!(next.status.success());
    assert!(String::from_utf8_lossy(&next.stdout).contains("mock/chat"));
}

#[test]
fn resize_reflows_the_transcript_without_losing_the_latest_answer() {
    let server = MockServer::start(vec![text_response("resize reflow answer")]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("resize reflow prompt");
    tui.wait_for("resize reflow answer", WAIT);
    tui.resize(24, 80);
    tui.wait_for("resize reflow answer", WAIT);
    tui.send(b"\x04");
    tui.wait_exit();
}
