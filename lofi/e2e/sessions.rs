[object Object]
/// Resumed geometry starts as a byte estimate and converges lazily, so
/// navigate-mode jumps issued right after startup must land at the requested
/// end of the transcript before and after the estimate settles.
#[test]
fn navigate_mode_scrolls_the_restored_transcript_after_continue() {
    const TURNS: usize = 28;
    let server = MockServer::start(
        (0..TURNS)
            .map(|index| text_response(&format!("navigate continue answer {index:03}")))
            .collect(),
    );
    let fixture = Fixture::new(&server);
    let mut seed = fixture.spawn(&[]);
    for index in 0..TURNS {
        seed.submit(&format!("navigate continue prompt {index:03}"));
        seed.wait_for(&format!("navigate continue answer {index:03}"), WAIT);
    }
    seed.submit("/quit");
    seed.wait_exit();

    let mut tui = fixture.spawn(&["--continue"]);
    tui.wait_for("navigate continue answer 027", WAIT);
    tui.clear_output();
    tui.send(b"\t");
    tui.wait_for("NAV", WAIT);
    tui.send(b"g");
    tui.wait_for("navigate continue prompt 000", WAIT);
    tui.send(b"G");
    tui.wait_for("navigate continue answer 027", WAIT);
    tui.send(b"g");
    tui.send(b"G");
    tui.wait_for("navigate continue answer 027", WAIT);
}

/// The /resume picker must restore the chosen session's tall transcript, keep
/// it navigable in navigate mode, and shape the next request only from the
/// restored history.
#[test]
fn resume_picker_restores_a_tall_session_and_stays_navigable() {
    let mut responses: Vec<MockResponse> = (0..26)
        .map(|index| text_response(&format!("resume alpha answer {index:03}")))
        .collect();
    responses.push(text_response("resume beta answer one"));
    responses.push(text_response("resume beta answer two"));
    let server = MockServer::start(responses);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    for index in 0..26 {
        tui.submit(&format!("resume alpha prompt {index:03}"));
        tui.wait_for(&format!("resume alpha answer {index:03}"), WAIT);
    }
    tui.submit("/new");
    tui.submit("resume beta prompt one");
    tui.wait_for("resume beta answer one", WAIT);
    tui.submit("resume beta prompt two");
    tui.wait_for("resume beta answer two", WAIT);

    tui.clear_output();
    tui.submit("/resume");
    tui.wait_for("Resume a session", WAIT);
    // Entries are most-recent first; step down to the older alpha session.
    tui.send(b"\x1b[B\r");
    tui.wait_for("resume alpha answer 025", WAIT);

    tui.send(b"\t");
    tui.wait_for("NAV", WAIT);
    tui.send(b"g");
    tui.wait_for("resume alpha prompt 000", WAIT);
    tui.send(b"G");
    tui.wait_for("resume alpha answer 025", WAIT);
    tui.send(b"\t");

    server.push(text_response("resume gamma answer"));
    tui.submit("resume gamma prompt");
    tui.wait_for("resume gamma answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 29);
    let resumed_request = &requests.last().unwrap().body;
    assert!(resumed_request.contains("resume alpha prompt 000"));
    assert!(resumed_request.contains("resume alpha answer 025"));
    assert!(!resumed_request.contains("resume beta prompt one"));
}
