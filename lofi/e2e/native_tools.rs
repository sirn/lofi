use crate::support::{
    process_is_alive, text_response, tool_response, transcript_text, wait_for_process_exit,
    Fixture, MockServer, ProcessGuard, WAIT,
};

#[test]
fn native_file_discovery_docs_and_skill_apis_work_through_exec() {
    let server = MockServer::start(vec![
        tool_response(
            "native-api-call",
            r#"
const write = await lofi.write({ path: "data/source.txt", text: "alpha\nbeta\ngamma\n" });
const edit = await lofi.edit({ path: "data/source.txt", old: "alpha", new: "ALPHA" });
const patch = await lofi.patch({ path: "data/source.txt", patch: "@@\n-beta\n+BETA\n" });
const read = await lofi.read("data/source.txt", { offset: 1, limit: 2 });
const ls = await lofi.ls("data");
await lofi.write({ path: ".hidden.txt", text: "hidden search marker" });
await lofi.write({ path: "data/ignored.txt", text: "ignored search marker" });
await lofi.write({ path: ".gitignore", text: "data/ignored.txt\n" });
const find = await lofi.find("*.txt", "data");
const findFiltered = await lofi.find("*.txt", ".", true);
const findAll = await lofi.find("*.txt", ".", false);
const grep = await lofi.grep("BETA", "data");
const grepOptions = await lofi.grep({ regex: "beta", ic: true, ctx: 1, filtered: false }, "data/source.txt");
const bash = await lofi.bash({ cmd: "printf native-bash-marker" });
const skills = await lofi.skills("fixture");
const skill = await lofi.skill("fixture-skill");
const docs = await lofi.docs();
const doc = await lofi.docs("lofi.read");
const search = await lofi.docsSearch("background job");
return {
  write: write.ok,
  edit: edit.ok,
  patch: patch.ok,
  read: read.content,
  ls: ls.entries,
  find: find.matches,
  filteredSawHidden: findFiltered.matches.includes(".hidden.txt"),
  allSawHidden: findAll.matches.includes(".hidden.txt"),
  grep: grep.matches,
  grepOptions: grepOptions.matches,
  bash: bash.output,
  skills: skills.skills,
  skill: skill.content,
  docs: docs.entries.length,
  doc: doc.name,
  search: search.results[0].name,
  tmp: lofi.tmp_dir,
};
"#,
        ),
        text_response("native API final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let skill_dir = fixture
        .config
        .parent()
        .unwrap()
        .join("skills/fixture-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: fixture-skill\ndescription: fixture skill marker\n---\n\nfixture skill body marker\n",
    )
    .unwrap();
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise every native file and discovery API");
    tui.wait_for_scrollback("native API final answer", WAIT);

    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("data/source.txt")).unwrap(),
        "ALPHA\nBETA\ngamma\n"
    );
    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let result_request = &requests[1].body;
    for marker in [
        "ALPHA",
        "BETA",
        "source.txt",
        "native-bash-marker",
        "fixture-skill",
        "fixture skill body marker",
        "lofi.read",
        "lofi.jobSpawn",
        r#"\"filteredSawHidden\":false"#,
        r#"\"allSawHidden\":true"#,
    ] {
        assert!(
            result_request.contains(marker),
            "missing {marker}: {result_request}"
        );
    }
    let transcript = transcript_text(&fixture.events());
    for tool in [
        "write", "edit", "patch", "read", "ls", "find", "grep", "bash", "skills", "skill",
    ] {
        assert!(transcript.contains(&format!(r#""name":"{tool}""#)));
    }
}

#[test]
fn native_tool_errors_stay_contained_and_truncated_results_are_recoverable() {
    let server = MockServer::start(vec![
        tool_response(
            "native-edge-call",
            r#"
await lofi.write({ path: "many.txt", text: Array.from({length: 20}, (_, i) => `line-${i}`).join("\n") });
const read = await lofi.read("many.txt");
const bash = await lofi.bash({ cmd: "for i in $(seq 1 20); do echo shell-$i; done" });
const fullPath = bash.output.match(/Full output: (.+?)\. Use lofi\.read/)[1];
const full = await lofi.read(fullPath);
const timedBash = await lofi.bash({ cmd: "sleep 60", timeoutMs: 20 });
let escape;
try { await lofi.read("../outside.txt"); } catch (error) { escape = String(error); }
let ambiguous;
try {
  await lofi.write({ path: "dupe.txt", text: "same same" });
  await lofi.edit({ path: "dupe.txt", old: "same", new: "changed" });
} catch (error) { ambiguous = String(error); }
return { read, bash, full, timedBash, escape, ambiguous };
"#,
        ),
        text_response("native edge final answer"),
    ]);
    let fixture = Fixture::new(&server);
    fixture.set_truncation(3, 256);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise native tool boundaries");
    tui.wait_for_scrollback("native edge final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    assert!(body.contains("truncated"));
    assert!(body.contains("outside"));
    assert!(body.contains("expected exactly one"));
    assert!(body.contains("shell-1"));
    assert!(body.contains(r#"\"status\":\"timeout\""#));
    assert!(!fixture
        .workspace
        .parent()
        .unwrap()
        .join("outside.txt")
        .exists());
}

#[test]
fn every_background_job_api_reports_output_status_wait_notify_and_kill() {
    let server = MockServer::start(vec![
        tool_response(
            "job-api-call",
            r#"
const first = await lofi.jobSpawn({ cmd: "printf job-page-one; sleep 0.2; printf job-page-two", notify: false });
const notify = await lofi.jobNotify({ id: first.id, enabled: true, intervalMs: 1, changed: false });
const silenced = await lofi.jobNotify({ id: first.id, enabled: false });
const early = await lofi.jobWait({ id: first.id, timeoutMs: 1 });
const running = await lofi.jobStatus({ id: first.id });
const done = await lofi.jobWait({ id: first.id, timeoutMs: 5000 });
const page1 = await lofi.jobRead({ id: first.id, limit: 12 });
const page2 = await lofi.jobRead({ id: first.id, cursor: page1.cursor, limit: 100 });
const second = await lofi.jobSpawn({ cmd: "sleep 60", notify: false });
const killed = await lofi.jobKill({ id: second.id, reason: "job kill marker" });
const killedStatus = await lofi.jobStatus({ id: second.id });
const killedAgain = await lofi.jobKill({ id: second.id, reason: "idempotent marker" });
const timed = await lofi.jobSpawn({ cmd: "sleep 60", timeoutMs: 20, notify: false });
const timedStatus = await lofi.jobWait({ id: timed.id, timeoutMs: 5000 });
const failed = await lofi.jobSpawn({ cmd: "printf failed-job-marker; exit 23", notify: false });
const failedStatus = await lofi.jobWait({ id: failed.id });
const failedLog = await lofi.jobRead({ id: failed.id });
const large = await lofi.jobSpawn({ cmd: "yes x | head -c 70000", notify: false });
await lofi.jobWait({ id: large.id });
const largePage = await lofi.jobRead({ id: large.id, limit: 999999 });
const pastEnd = await lofi.jobRead({ id: large.id, cursor: 999999, limit: 1 });
const concurrentA = await lofi.jobSpawn({ cmd: "sleep 0.05; printf concurrent-a", notify: false });
const concurrentB = await lofi.jobSpawn({ cmd: "sleep 0.03; printf concurrent-b", notify: false });
const concurrentC = await lofi.jobSpawn({ cmd: "sleep 0.01; printf concurrent-c", notify: false });
const concurrentDoneA = await lofi.jobWait({ id: concurrentA.id });
const concurrentDoneB = await lofi.jobWait({ id: concurrentB.id });
const concurrentDoneC = await lofi.jobWait({ id: concurrentC.id });
const list = await lofi.jobList();
const missing = {
  status: await lofi.jobStatus({ id: "999999" }),
  read: await lofi.jobRead({ id: "999999" }),
  wait: await lofi.jobWait({ id: "999999", timeoutMs: 1 }),
  kill: await lofi.jobKill({ id: "999999" }),
  notify: await lofi.jobNotify({ id: "999999" }),
};
return {
  notify, silenced, early, running, done, page1, page2, killed, killedStatus, killedAgain,
  timedStatus, failedStatus, failedLog,
  largePage: { cursor: largePage.cursor, totalBytes: largePage.totalBytes, outputBytes: largePage.output.length },
  pastEnd, concurrentIds: [concurrentA.id, concurrentB.id, concurrentC.id],
  concurrentUnique: new Set([concurrentA.id, concurrentB.id, concurrentC.id]).size === 3,
  concurrentStates: [concurrentDoneA.state, concurrentDoneB.state, concurrentDoneC.state],
  listCount: list.jobs.length,
  listNewestIsConcurrentC: list.jobs[0].id === concurrentC.id,
  listHasAllSpawned: [first.id, second.id, timed.id, failed.id, large.id, concurrentA.id, concurrentB.id, concurrentC.id]
    .every((id) => list.jobs.some((j) => j.id === id)),
  missing,
};
"#,
        ),
        text_response("job API final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise every background job API");
    tui.wait_for_scrollback("job API final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        "job-page-one",
        "job-page-two",
        "job kill marker",
        "cancelled",
        "intervalMs",
        "5000",
        "idempotent marker",
        "timed_out",
        "failed-job-marker",
        r#"\"exitCode\":23"#,
        r#"\"signal\":9"#,
        r#"\"cursor\":65536"#,
        r#"\"totalBytes\":70000"#,
        "concurrent-a",
        "concurrent-b",
        "concurrent-c",
        r#"\"concurrentUnique\":true"#,
        r#"\"listCount\":8"#,
        r#"\"listNewestIsConcurrentC\":true"#,
        r#"\"listHasAllSpawned\":true"#,
        "no such job",
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
    let transcript = transcript_text(&fixture.events());
    for tool in [
        "jobSpawn",
        "jobNotify",
        "jobWait",
        "jobStatus",
        "jobList",
        "jobRead",
        "jobKill",
    ] {
        assert!(transcript.contains(&format!(r#""name":"{tool}""#)));
    }
}

#[test]
fn every_background_job_api_rejects_invalid_arguments() {
    let server = MockServer::start(vec![
        tool_response("invalid-job-spawn", "return await lofi.jobSpawn({});"),
        tool_response(
            "invalid-job-status",
            r#"return await lofi.jobStatus({ id: "not-an-id" });"#,
        ),
        tool_response("invalid-job-read", "return await lofi.jobRead({});"),
        tool_response(
            "invalid-job-wait",
            "return await lofi.jobWait({ id: null });",
        ),
        tool_response("invalid-job-kill", "return await lofi.jobKill({ id: -1 });"),
        tool_response("invalid-job-notify", "return await lofi.jobNotify({});"),
        text_response("invalid job arguments final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise invalid background job arguments");
    tui.wait_for_scrollback("invalid job arguments final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 7);
    let body = &requests[6].body;
    assert!(body.contains("jobSpawn: missing"));
    assert_eq!(body.matches("job: missing or invalid").count(), 5);
}

#[test]
fn tui_shutdown_kills_a_live_background_job_process_group() {
    let server = MockServer::start(vec![
        tool_response(
            "shutdown-job-call",
            r#"return await lofi.jobSpawn({ cmd: "echo $$ > shutdown-job.pid; exec sleep 60", notify: false });"#,
        ),
        text_response("shutdown job answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("start a job before shutdown");
    let started = std::time::Instant::now();
    while server.request_count() < 1 && started.elapsed() < WAIT {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(server.request_count(), 1);
    tui.wait_for_scrollback("Permission Required", WAIT);
    tui.send(b"a");
    tui.wait_for_scrollback("shutdown job answer", WAIT);
    let pid: i32 = std::fs::read_to_string(fixture.workspace.join("shutdown-job.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut job = ProcessGuard::new(pid);
    assert!(process_is_alive(pid));

    tui.send(b"\x04");
    tui.wait_exit();
    wait_for_process_exit(pid);
    job.disarm();
}

#[test]
fn interactive_job_accepts_typed_input_and_reports_idle() {
    let server = MockServer::start(vec![
        tool_response(
            "interactive-job-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "printf 'Name? '; read name; echo \"hello:$name\"", tty: true, notify: false });
const waiting = await lofi.jobWait({ id: s.id, idleMs: 500, timeoutMs: 5000 });
const typed = await lofi.jobType({ id: s.id, text: "ada" });
const entered = await lofi.jobKeyPress({ id: s.id, key: "Enter" });
const done = await lofi.jobWait({ id: s.id, timeoutMs: 5000 });
const log = await lofi.jobRead({ id: s.id });
return { tty: s.tty, idle: waiting.idle, typed: typed.sent, entered: entered.ok, state: done.state, output: log.output };
"#,
        ),
        text_response("interactive job final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("answer an interactive background job");
    tui.wait_for_scrollback("interactive job final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"tty\":true"#,
        r#"\"idle\":true"#,
        r#"\"typed\":3"#,
        r#"\"entered\":true"#,
        r#"\"state\":\"completed\""#,
        "hello:ada",
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
    let transcript = transcript_text(&fixture.events());
    for tool in ["jobSpawn", "jobType", "jobKeyPress", "jobWait", "jobRead"] {
        assert!(transcript.contains(&format!(r#""name":"{tool}""#)));
    }
}

#[test]
fn tty_job_answers_a_multi_prompt_flow_by_pattern() {
    let server = MockServer::start(vec![
        tool_response(
            "multi-prompt-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "printf 'A: '; read a; printf 'B: '; read b; echo sum:$a$b", tty: true, notify: false });
const w1 = await lofi.jobWait({ id: s.id, pattern: "A:", timeoutMs: 5000 });
const t1 = await lofi.jobType({ id: s.id, text: "one" });
await lofi.jobKeyPress({ id: s.id, key: "Enter" });
const w2 = await lofi.jobWait({ id: s.id, pattern: "B:", timeoutMs: 5000 });
const t2 = await lofi.jobType({ id: s.id, text: "two" });
await lofi.jobKeyPress({ id: s.id, key: "Enter" });
const done = await lofi.jobWait({ id: s.id, timeoutMs: 5000 });
const log = await lofi.jobRead({ id: s.id });
return { w1: w1.matched, w2: w2.matched, t1: t1.sent, t2: t2.sent, state: done.state, output: log.output };
"#,
        ),
        text_response("multi prompt final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("answer a multi prompt interactive job");
    tui.wait_for_scrollback("multi prompt final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"w1\":\"A:\""#,
        r#"\"w2\":\"B:\""#,
        r#"\"t1\":3"#,
        r#"\"t2\":3"#,
        r#"\"state\":\"completed\""#,
        "sum:onetwo",
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn tty_job_key_presses_deliver_xterm_byte_sequences() {
    let server = MockServer::start(vec![
        tool_response(
            "key-bytes-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "stty -icanon -echo; echo READY; od -An -tx1 -N 5; echo", tty: true, notify: false });
await lofi.jobWait({ id: s.id, pattern: "READY", timeoutMs: 5000 });
await lofi.jobKeyPress({ id: s.id, key: "Backspace" });
await lofi.jobKeyPress({ id: s.id, key: "Left" });
await lofi.jobType({ id: s.id, text: "X" });
const done = await lofi.jobWait({ id: s.id, timeoutMs: 5000 });
const log = await lofi.jobRead({ id: s.id });
return { state: done.state, output: log.output };
"#,
        ),
        text_response("key bytes final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("deliver xterm key byte sequences");
    tui.wait_for_scrollback("key bytes final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [r#"\"state\":\"completed\""#, "7f 1b 5b 44 58"] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn tty_job_ctrl_c_interrupts_a_running_program() {
    let server = MockServer::start(vec![
        tool_response(
            "ctrl-c-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "sleep 60", tty: true, notify: false });
const waiting = await lofi.jobWait({ id: s.id, idleMs: 300, timeoutMs: 5000 });
await lofi.jobKeyPress({ id: s.id, key: "Ctrl+C" });
const done = await lofi.jobWait({ id: s.id, timeoutMs: 5000 });
return { idle: waiting.idle, state: done.state, signal: done.signal };
"#,
        ),
        text_response("ctrl-c final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("interrupt an interactive job with ctrl-c");
    tui.wait_for_scrollback("ctrl-c final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"idle\":true"#,
        r#"\"state\":\"failed\""#,
        r#"\"signal\":2"#,
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn tty_job_ctrl_d_closes_stdin_to_a_read_loop() {
    let server = MockServer::start(vec![
        tool_response(
            "ctrl-d-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "echo go; while IFS= read -r line; do echo line:$line; done; echo eof", tty: true, notify: false });
await lofi.jobWait({ id: s.id, pattern: "go", timeoutMs: 5000 });
await lofi.jobType({ id: s.id, text: "hello" });
await lofi.jobKeyPress({ id: s.id, key: "Enter" });
await lofi.jobType({ id: s.id, text: "world" });
await lofi.jobKeyPress({ id: s.id, key: "Enter" });
await lofi.jobKeyPress({ id: s.id, key: "Ctrl+D" });
const done = await lofi.jobWait({ id: s.id, timeoutMs: 5000 });
const log = await lofi.jobRead({ id: s.id });
return { state: done.state, output: log.output };
"#,
        ),
        text_response("ctrl-d final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("close a read loop with ctrl-d");
    tui.wait_for_scrollback("ctrl-d final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"state\":\"completed\""#,
        "line:hello",
        "line:world",
        "eof",
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn tty_job_has_term_dimensions_and_a_working_controlling_terminal() {
    let server = MockServer::start(vec![
        tool_response(
            "terminal-env-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "echo term=$TERM; stty size; read x < /dev/tty; echo tty:$x", tty: true, cols: 88, rows: 26, notify: false });
await lofi.jobWait({ id: s.id, pattern: "26 88", timeoutMs: 5000 });
await lofi.jobType({ id: s.id, text: "data" });
await lofi.jobKeyPress({ id: s.id, key: "Enter" });
const done = await lofi.jobWait({ id: s.id, timeoutMs: 5000 });
const log = await lofi.jobRead({ id: s.id });
return { tty: s.tty, cols: s.cols, rows: s.rows, state: done.state, output: log.output };
"#,
        ),
        text_response("terminal env final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("inspect a job terminal environment");
    tui.wait_for_scrollback("terminal env final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"tty\":true"#,
        r#"\"cols\":88"#,
        r#"\"rows\":26"#,
        r#"\"state\":\"completed\""#,
        "term=xterm-256color",
        "26 88",
        "tty:data",
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn plain_job_reports_idle_when_configured_via_job_notify() {
    let server = MockServer::start(vec![
        tool_response(
            "plain-idle-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "printf burst; sleep 60", notify: false });
const n = await lofi.jobNotify({ id: s.id, idleMs: 500, enabled: false });
const waiting = await lofi.jobWait({ id: s.id, idleMs: 600, timeoutMs: 5000 });
const status = await lofi.jobStatus({ id: s.id });
await lofi.jobKill({ id: s.id });
return { notifyIdleMs: n.idleMs, waitingIdle: waiting.idle, statusIdle: status.idle, idleMs: status.idleMs, tty: status.tty };
"#,
        ),
        text_response("plain idle final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("detect idle on a plain background job");
    tui.wait_for_scrollback("plain idle final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"notifyIdleMs\":500"#,
        r#"\"waitingIdle\":true"#,
        r#"\"statusIdle\":true"#,
        r#"\"idleMs\":500"#,
        r#"\"tty\":false"#,
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn tty_job_clamps_spawn_dimensions_and_idle_threshold() {
    let server = MockServer::start(vec![
        tool_response(
            "clamp-call",
            r#"
const s = await lofi.jobSpawn({ cmd: "sleep 60", tty: true, cols: 99999, rows: 0, idleMs: 1, notify: false });
await lofi.jobKill({ id: s.id });
return { tty: s.tty, cols: s.cols, rows: s.rows, idleMs: s.idleMs };
"#,
        ),
        text_response("clamp final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("clamp interactive job dimensions");
    tui.wait_for_scrollback("clamp final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        r#"\"tty\":true"#,
        r#"\"cols\":1000"#,
        r#"\"rows\":1"#,
        r#"\"idleMs\":500"#,
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn interactive_job_tools_reject_invalid_arguments() {
    let server = MockServer::start(vec![
        tool_response(
            "invalid-interactive-call",
            r#"
const tty = await lofi.jobSpawn({ cmd: "sleep 60", tty: true, notify: false });
const plain = await lofi.jobSpawn({ cmd: "sleep 60", notify: false });
let typeNoId;
try { await lofi.jobType({}); } catch (e) { typeNoId = String(e); }
let typeNoText;
try { await lofi.jobType({ id: tty.id }); } catch (e) { typeNoText = String(e); }
let typeNonTty;
try { await lofi.jobType({ id: plain.id, text: "x" }); } catch (e) { typeNonTty = String(e); }
let keyUnknown;
try { await lofi.jobKeyPress({ id: tty.id, key: "NotAKey" }); } catch (e) { keyUnknown = String(e); }
let keyNonTty;
try { await lofi.jobKeyPress({ id: plain.id, key: "Enter" }); } catch (e) { keyNonTty = String(e); }
const typeMissing = await lofi.jobType({ id: "999999", text: "x" });
const keyMissing = await lofi.jobKeyPress({ id: "999999", key: "Enter" });
const waitMissing = await lofi.jobWait({ id: "999999", pattern: "x" });
await lofi.jobKill({ id: tty.id });
await lofi.jobKill({ id: plain.id });
return { typeNoId, typeNoText, typeNonTty, keyUnknown, keyNonTty, typeMissing, keyMissing, waitMissing };
"#,
        ),
        text_response("invalid interactive arguments final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise invalid interactive job arguments");
    tui.wait_for_scrollback("invalid interactive arguments final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    for marker in [
        "job: missing or invalid 'id'",
        "jobType: missing 'text'",
        "job is not a tty job",
        "jobKeyPress: unknown key 'NotAKey'",
        "no such job: 999999",
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
}

#[test]
fn recall_and_result_recover_durable_session_content_by_query_and_event_id() {
    let server = MockServer::start(vec![
        tool_response(
            "recover-source-call",
            r#"return { marker: "recover original tool marker" };"#,
        ),
        text_response("recover source answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("recover source prompt marker");
    tui.wait_for("recover source answer marker", WAIT);
    fixture.wait_for_event_count("turn_end", 1);
    let result_event_id = fixture
        .events()
        .into_iter()
        .find(|event| {
            event["type"] == "message"
                && event["role"] == "tool"
                && event["blocks"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_result"))
        })
        .and_then(|event| event["id"].as_str().map(ToString::to_string))
        .unwrap();

    server.push(tool_response(
        "recover-api-call",
        &format!(
            r#"const recall = await lofi.recall({{ query: "recover source prompt marker", scope: "all" }});
const result = await lofi.result("{result_event_id}");
return {{ recall, result }};"#
        ),
    ));
    server.push(text_response("recover API final answer"));
    tui.submit("use durable recovery APIs");
    tui.wait_for("recover API final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    let body = &requests[3].body;
    assert!(body.contains("recover source prompt marker"));
    assert!(body.contains("recover original tool marker"));
}

#[test]
fn image_tool_results_are_normalized_and_sent_only_to_vision_models() {
    let server = MockServer::start(vec![
        tool_response(
            "vision-image-call",
            r#"return await lofi.read("pixel.png");"#,
        ),
        text_response("vision image answer"),
        tool_response(
            "nonvision-image-call",
            r#"return await lofi.read("pixel.png");"#,
        ),
        text_response("nonvision image answer"),
    ]);
    let fixture = Fixture::new(&server);
    std::fs::write(fixture.workspace.join("pixel.png"), one_pixel_png()).unwrap();

    let mut vision = fixture.spawn(&[]);
    vision.submit("read image with vision model");
    vision.wait_for("vision image answer", WAIT);
    vision.submit("/quit");
    vision.wait_exit();

    let mut nonvision = fixture.spawn(&["--model", "mock/alt"]);
    nonvision.submit("read image without vision model");
    nonvision.wait_for("nonvision image answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1].body.contains("data:image/jpeg;base64,"),
        "{}",
        requests[1].body
    );
    assert!(!requests[3].body.contains("data:image/jpeg;base64,"));
    assert!(requests[3].body.contains("image omitted"));
    let transcript = transcript_text(&fixture.events());
    assert!(!transcript.contains("data:image"));
}

fn one_pixel_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 4, 0, 0, 0, 0xb5, 0x1c, 0x0c, 0x02, 0, 0, 0, 0x0b, 0x49, 0x44,
        0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0, 1, 5, 1, 1, 0x27, 0x18, 0xe3, 0x66, 0,
        0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}
