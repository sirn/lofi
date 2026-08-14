use crate::support::{text_response, tool_response, transcript_text, Fixture, MockServer, WAIT};

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
const find = await lofi.find("*.txt", "data");
const grep = await lofi.grep("BETA", "data");
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
  grep: grep.matches,
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
    tui.wait_for("native API final answer", WAIT);

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
let escape;
try { await lofi.read("../outside.txt"); } catch (error) { escape = String(error); }
let ambiguous;
try {
  await lofi.write({ path: "dupe.txt", text: "same same" });
  await lofi.edit({ path: "dupe.txt", old: "same", new: "changed" });
} catch (error) { ambiguous = String(error); }
return { read, bash, escape, ambiguous };
"#,
        ),
        text_response("native edge final answer"),
    ]);
    let fixture = Fixture::new(&server);
    fixture.set_truncation(3, 256);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise native tool boundaries");
    tui.wait_for("native edge final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let body = &requests[1].body;
    assert!(body.contains("truncated"));
    assert!(body.contains("outside"));
    assert!(body.contains("expected exactly one"));
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
const early = await lofi.jobWait({ id: first.id, timeoutMs: 1 });
const running = await lofi.jobStatus({ id: first.id });
const done = await lofi.jobWait({ id: first.id, timeoutMs: 5000 });
const page1 = await lofi.jobRead({ id: first.id, limit: 12 });
const page2 = await lofi.jobRead({ id: first.id, cursor: page1.cursor, limit: 100 });
const second = await lofi.jobSpawn({ cmd: "sleep 60", notify: false });
const killed = await lofi.jobKill({ id: second.id, reason: "job kill marker" });
const killedStatus = await lofi.jobStatus({ id: second.id });
return { notify, early, running, done, page1, page2, killed, killedStatus };
"#,
        ),
        text_response("job API final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("exercise every background job API");
    tui.wait_for("job API final answer", WAIT);

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
    ] {
        assert!(body.contains(marker), "missing {marker}: {body}");
    }
    let transcript = transcript_text(&fixture.events());
    for tool in [
        "jobSpawn",
        "jobNotify",
        "jobWait",
        "jobStatus",
        "jobRead",
        "jobKill",
    ] {
        assert!(transcript.contains(&format!(r#""name":"{tool}""#)));
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
