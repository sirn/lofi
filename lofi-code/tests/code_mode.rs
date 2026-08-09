#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::path::Path;

use lofi_code::{compile_ts, exec, BashEnv, ExecCtx, ExecOptions, ToolEvent};
use serde_json::{json, Value};

fn ctx(root: &Path) -> ExecCtx {
    let tmp_dir = std::env::temp_dir().join("lofi-test");
    let _ = std::fs::create_dir_all(&tmp_dir);
    ExecCtx {
        root: root.to_path_buf(),
        tmp_dir,
        strings: HashMap::new(),
        on_tool_event: None,
        recall: None,
        result: None,
        bash_env: BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm: None,
        auto_mode: None,
        skills_dir: None,
        truncate: lofi_code::TruncatedCap::default(),
        jobs: lofi_code::tools::JobRegistry::new(),
    }
}

fn trusted_ctx(root: &Path) -> ExecCtx {
    let mut ctx = ctx(root);
    ctx.shell_policy = lofi_code::policy::defaults::resolve(&lofi_types::ShellPolicyConfig {
        mode: lofi_types::ShellPolicyMode::Unrestricted,
        ..lofi_types::ShellPolicyConfig::default()
    });
    ctx
}

#[test]
fn compile_ts_strips_type_annotations() {
    let js = compile_ts("const x: number = 42; return x;").unwrap();
    assert!(
        !js.contains(": number"),
        "type annotation leaked into output: {js}"
    );
    assert!(js.contains("const x = 42"));
}

#[test]
fn compile_ts_strips_interface() {
    let src = "interface Foo { a: number } return 1;";
    let js = compile_ts(src).unwrap();
    assert!(!js.contains("interface"));
    assert!(!js.contains("Foo"));
}

#[tokio::test]
async fn exec_returns_a_value() {
    let dir = tempfile::tempdir().unwrap();
    let res = exec("return 42;", &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, json!(42));
}

#[tokio::test]
async fn exec_top_level_await_works() {
    let dir = tempfile::tempdir().unwrap();
    let res = exec(
        "const v = await Promise.resolve(7); return v;",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value, json!(7));
}

#[tokio::test]
async fn pi_read_reads_a_tempdir_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("file.txt"), "hello world").unwrap();
    let res = exec(
        "return await lofi.read('file.txt');",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["content"], json!("hello world"));
}

#[tokio::test]
async fn pi_write_then_read_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = "await lofi.write({ path: 'out/nested/x.txt', text: 'hi' }); return await lofi.read('out/nested/x.txt');";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value["content"], json!("hi"));
}

#[tokio::test]
async fn every_native_api_result_can_be_returned_as_is() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".lofi/skills/demo")).unwrap();
    std::fs::write(
        dir.path().join(".lofi/skills/demo/SKILL.md"),
        "# Demo\nA test skill.\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("source.txt"), "alpha\nbeta\n").unwrap();

    let src = r#"
        const read = await lofi.read("source.txt");
        const ls = await lofi.ls(".");
        const find = await lofi.find("*.txt", ".");
        const grep = await lofi.grep("beta", "source.txt");
        const write = await lofi.write({ path: "written.txt", text: "before" });
        const edit = await lofi.edit({ path: "written.txt", old: "before", new: "after" });
        const bash = await lofi.bash({ cmd: "printf native-output" });
        const skills = await lofi.skills();
        const skill = await lofi.skill("demo");
        const docs = await lofi.docs("lofi.read");
        const docsSearch = await lofi.docsSearch("read file");
        const hasLegacyDocsSearch = Object.prototype.hasOwnProperty.call(lofi, "docs_search");
        const recall = await lofi.recall();
        const result = await lofi.result("missing");
        return { read, ls, find, grep, write, edit, bash, skills, skill,
                 docs, docsSearch, hasLegacyDocsSearch, recall, result };
    "#;
    let res = exec(src, &trusted_ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();

    let value = res.value.as_object().expect("returned API result map");
    for name in [
        "read",
        "ls",
        "find",
        "grep",
        "write",
        "edit",
        "bash",
        "skills",
        "skill",
        "docs",
        "docsSearch",
        "recall",
        "result",
    ] {
        assert!(value.contains_key(name), "missing direct result for {name}");
    }
    assert_eq!(value["read"]["content"], json!("alpha\nbeta\n"));
    assert_eq!(value["bash"]["output"], json!("native-output"));
    assert_eq!(value["skill"]["name"], json!("demo"));
    assert_eq!(value["docs"]["ok"], json!(true));
    assert_eq!(value["hasLegacyDocsSearch"], json!(false));
    assert_eq!(value["recall"]["status"], json!("unavailable"));
    assert!(value["result"].as_str().is_some());
}

#[tokio::test]
async fn path_escape_throws_a_js_error() {
    let dir = tempfile::tempdir().unwrap();
    let src = "try { await lofi.read('../escape'); return 'no-throw'; } catch (e) { return 'caught:' + e.message; }";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s = res.value.as_str().unwrap();
    assert!(s.starts_with("caught:tool error:"), "got {s}");
    assert!(
        s.contains("escapes workspace root"),
        "escape message missing: {s}"
    );
    assert!(
        !s.contains("Error converting from"),
        "tool failure was mislabeled as value conversion: {s}"
    );
}

#[tokio::test]
async fn uncaught_escape_surfaces_as_sandbox_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = exec(
        "await lofi.read('../escape'); return 'ok';",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, lofi_error::Error::Sandbox(_)),
        "expected Sandbox error, got {err:?}"
    );
    assert!(err.to_string().contains("escapes workspace root"));
}

#[tokio::test]
async fn print_buffers_into_logs() {
    let dir = tempfile::tempdir().unwrap();
    let res = exec(
        "print('hello','world'); print('c'); return 0;",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value, json!(0));
    assert_eq!(res.logs, "hello world\nc\n");
}

#[tokio::test]
async fn strings_exposed_as_lofi_strings() {
    let dir = tempfile::tempdir().unwrap();
    let mut strings = HashMap::new();
    strings.insert("greeting".to_string(), "hello".to_string());
    let cx = ExecCtx {
        root: dir.path().to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-test"),
        strings,
        on_tool_event: None,
        recall: None,
        result: None,
        bash_env: BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm: None,
        auto_mode: None,
        skills_dir: None,
        truncate: lofi_code::TruncatedCap::default(),
        jobs: lofi_code::tools::JobRegistry::new(),
    };
    let res = exec(
        "return lofi_strings.greeting;",
        &cx,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value, json!("hello"));
}

#[tokio::test]
async fn exec_bash_echo() {
    let dir = tempfile::tempdir().unwrap();
    let src = "const r = await lofi.bash({ cmd: 'echo hi' }); return r.output.trim();";
    let res = exec(src, &trusted_ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s: &str = res.value.as_str().unwrap();
    assert_eq!(s, "hi");
}

#[tokio::test]
async fn exec_bash_read_pages_bash_log() {
    let dir = tempfile::tempdir().unwrap();
    let src = "const r = await lofi.bash({ cmd: 'for i in $(seq 1 5000); do echo \"output line number $i with some padding text to make it longer\"; done' }); return r.output;";
    let res = exec(src, &trusted_ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let out: &str = res.value.as_str().unwrap();
    assert!(out.contains("Full output:"), "got: {out}");
    let path = out
        .split("Full output: ")
        .nth(1)
        .and_then(|s| {
            s.split(". Use lofi.read")
                .next()
                .map(|s| s.trim().to_string())
        })
        .expect("notice should contain an absolute path");
    let src2 = format!("const r = await lofi.read({path:?}, {{ offset: 1, limit: 3 }}); return r;");
    let res2 = exec(&src2, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s2: &str = res2.value["content"].as_str().unwrap();
    assert!(
        s2.contains("output line number 1"),
        "first page should start at line 1: path={path:?} s2={s2}"
    );
}

#[tokio::test]
async fn exec_tmp_dir_exposed() {
    let dir = tempfile::tempdir().unwrap();
    let src = "return lofi.tmp_dir;";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s: &str = res.value.as_str().unwrap();
    assert!(s.contains("lofi-test"), "tmp_dir should be exposed: {s}");
}

#[tokio::test]
async fn write_and_edit_emit_written_content_as_result() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let events: Arc<Mutex<Vec<ToolEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let cb = {
        let events = events.clone();
        Arc::new(move |ev: ToolEvent| events.lock().unwrap().push(ev))
            as Arc<dyn Fn(ToolEvent) + Send + Sync>
    };
    let cx = ExecCtx {
        root: dir.path().to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-test"),
        strings: HashMap::new(),
        on_tool_event: Some(cb),
        recall: None,
        result: None,
        bash_env: BashEnv::default(),
        shell_policy: lofi_code::policy::defaults::resolve(
            &lofi_types::ShellPolicyConfig::default(),
        ),
        confirm: None,
        auto_mode: None,
        skills_dir: None,
        truncate: lofi_code::TruncatedCap::default(),
        jobs: lofi_code::tools::JobRegistry::new(),
    };
    let src = "await lofi.write({path:'a.txt', text:'written line one\\nwritten line two'}); \
               await lofi.edit({path:'a.txt', old:'written line one', new:'edited line one'}); \
               return 'ok';";
    exec(src, &cx, &ExecOptions::default()).await.unwrap();
    let evs = events.lock().unwrap();
    let ends: Vec<&String> = evs
        .iter()
        .filter_map(|e| match e {
            ToolEvent::End {
                result,
                is_error: false,
                ..
            } => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(ends.len(), 2, "expected two successful tool ends: {evs:?}");
    assert!(
        ends.iter().any(|r| r.contains("written line one")),
        "write content missing: {ends:?}"
    );
    assert!(
        ends.iter().any(|r| r.contains("edited line one")),
        "edit content missing: {ends:?}"
    );
}

#[tokio::test]
async fn exec_returns_undefined_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let res = exec("print('hi');", &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, Value::Null);
    assert!(res.logs.contains("hi"));
}

#[tokio::test]
async fn exec_honours_configured_truncate_cap() {
    // The configured cap flows from ExecCtx into the read tool: a tiny cap
    // truncates a 10-line file to the first lines and flags truncation.
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("big.txt"),
        (1..=10).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "line {i}");
            s
        }),
    )
    .unwrap();
    let mut c = ctx(dir.path());
    c.truncate = lofi_code::TruncatedCap {
        max_lines: 3,
        max_bytes: 1 << 30,
    };
    let res = exec(
        "const r = await lofi.read('big.txt'); return r;",
        &c,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["truncated"], json!(true));
    let content = res.value["content"].as_str().unwrap();
    assert!(content.contains("line 1"));
    assert!(!content.contains("line 10"));
}

#[tokio::test]
async fn job_spawn_returns_immediately_and_completes() {
    let dir = tempfile::tempdir().unwrap();
    // Shared registry across two execs: the second call must see the job the
    // first one spawned, or the whole feature is a no-op.
    let jobs = lofi_code::tools::JobRegistry::new();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = jobs.clone();
    let spawn = exec(
        "const r = await lofi.jobSpawn({ cmd: 'echo bg-ok' }); return r;",
        &cx,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(spawn.value["ok"], json!(true));
    assert_eq!(spawn.value["state"], json!("running"));
    let id = spawn.value["id"].as_str().unwrap().to_string();

    let src = format!(
        "const s = await lofi.jobWait({{ id: '{id}' }}); return {{ state: s.state, code: s.exitCode }};"
    );
    let done = exec(&src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(done.value["state"], json!("completed"));
    assert_eq!(done.value["code"], json!(0));
}

#[tokio::test]
async fn job_spawn_without_timeout_has_no_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let spawn = exec(
        "const r = await lofi.jobSpawn({ cmd: 'echo x' }); return r.timeoutMs;",
        &cx,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    // No default deadline: the caller must opt in to a timeout.
    assert!(spawn.value.is_null());
}

#[tokio::test]
async fn job_read_pages_output_over_a_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = r#"
        const s = await lofi.jobSpawn({ cmd: 'printf \"l1\\nl2\\nl3\\n\"' });
        await lofi.jobWait({ id: s.id });
        const p1 = await lofi.jobRead({ id: s.id, limit: 3 });
        const p2 = await lofi.jobRead({ id: s.id, cursor: p1.cursor });
        return { a: p1.output, b: p2.output, done: p2.done };
    "#;
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value["a"], json!("l1\n"));
    assert_eq!(res.value["b"], json!("l2\nl3\n"));
    assert_eq!(res.value["done"], json!(true));
}

#[tokio::test]
async fn job_wait_with_timeout_returns_running() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "const s = await lofi.jobSpawn({ cmd: 'sleep 30' }); const w = await lofi.jobWait({ id: s.id, timeoutMs: 50 }); await lofi.jobKill({ id: s.id }); return w.state;";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value, json!("running"));
}

#[tokio::test]
async fn job_kill_is_idempotent_and_kills_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("survivor");
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let cmd =
        serde_json::to_string(&format!("(sleep 5; touch {}) & wait", marker.display())).unwrap();
    let src = format!("const s = await lofi.jobSpawn({{ cmd: {cmd} }}); const k1 = await lofi.jobKill({{ id: s.id }}); const k2 = await lofi.jobKill({{ id: s.id }}); return {{ a: k1.state, b: k2.state }};");
    let res = exec(&src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value["a"], json!("cancelled"));
    assert_eq!(res.value["b"], json!("cancelled"));
    // Give a straggler a chance to fire; the marker must never appear.
    tokio::time::sleep(std::time::Duration::from_millis(5_500)).await;
    assert!(!marker.exists(), "background child survived kill");
}

#[tokio::test]
async fn job_completion_queues_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = lofi_code::tools::JobRegistry::new();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = jobs.clone();
    let src = "const s = await lofi.jobSpawn({ cmd: 'exit 3' }); await lofi.jobWait({ id: s.id }); return s.id;";
    exec(src, &cx, &ExecOptions::default()).await.unwrap();
    let notices = jobs.drain_notices();
    assert_eq!(notices.len(), 1, "notices: {notices:?}");
    assert!(notices[0].contains("failed"), "notice: {}", notices[0]);
    assert!(notices[0].contains("exit 3"), "notice: {}", notices[0]);
}

#[tokio::test]
async fn job_periodic_tick_goes_to_ui_not_model() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = lofi_code::tools::JobRegistry::new();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = jobs.clone();
    // Spawn a job that emits one line then sleeps; enable a 5s-floor tick.
    let src = "const s = await lofi.jobSpawn({ cmd: 'echo hello; sleep 20' }); await lofi.jobNotify({ id: s.id, intervalMs: 5000 }); return s.id;";
    exec(src, &cx, &ExecOptions::default()).await.unwrap();
    // Wait past one tick interval so the progress ticker fires.
    tokio::time::sleep(std::time::Duration::from_millis(6_500)).await;
    // A tick lands in the single notice queue, to become a queued prompt.
    let notices = jobs.drain_notices();
    assert!(!notices.is_empty(), "no tick queued");
    assert!(notices[0].contains("running"), "tick: {}", notices[0]);
}

#[tokio::test]
async fn job_terminal_queues_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = lofi_code::tools::JobRegistry::new();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = jobs.clone();
    let src = "const s = await lofi.jobSpawn({ cmd: 'exit 0' }); await lofi.jobNotify({ id: s.id, intervalMs: 5000 }); await lofi.jobWait({ id: s.id }); return s.id;";
    exec(src, &cx, &ExecOptions::default()).await.unwrap();
    let notices = jobs.drain_notices();
    assert_eq!(notices.len(), 1, "terminal notices: {notices:?}");
}
#[tokio::test]
async fn job_notify_bare_enables_periodic_at_default() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "const s = await lofi.jobSpawn({ cmd: 'sleep 5' }); const n = await lofi.jobNotify({ id: s.id }); await lofi.jobKill({ id: s.id }); return n;";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value["notify"], json!(true));
    assert_eq!(res.value["intervalMs"], json!(30_000));
    assert_eq!(res.value["changed"], json!(true));
}

#[tokio::test]
async fn job_spawn_notify_interval_wires_through_to_status() {
    // Spawn-time knobs land on `Job.notify` so the task driver picks them
    // up. Without the wire, `notifyIntervalMs` is silently dropped and the
    // job runs terminal-only.
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "const s = await lofi.jobSpawn({ cmd: 'sleep 30', notifyIntervalMs: 6000 }); const st = await lofi.jobStatus({ id: s.id }); await lofi.jobKill({ id: s.id }); return { notify: st.notify, iv: st.notifyIntervalMs };";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value["notify"], json!(true));
    assert_eq!(res.value["iv"], json!(6_000));
}

#[tokio::test]
async fn job_spawn_notify_false_silences_everything() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "const s = await lofi.jobSpawn({ cmd: 'sleep 30', notify: false }); const st = await lofi.jobStatus({ id: s.id }); await lofi.jobKill({ id: s.id }); return st.notify;";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value, json!(false));
}

#[tokio::test]
async fn job_notify_clamps_interval_floor() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "const s = await lofi.jobSpawn({ cmd: 'sleep 5' }); const n = await lofi.jobNotify({ id: s.id, intervalMs: 100 }); await lofi.jobKill({ id: s.id }); return n.intervalMs;";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value, json!(5_000));
}

#[tokio::test]
async fn job_notify_disabled_suppresses_notice() {
    let dir = tempfile::tempdir().unwrap();
    let jobs = lofi_code::tools::JobRegistry::new();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = jobs.clone();
    let src = "const s = await lofi.jobSpawn({ cmd: 'true' }); await lofi.jobNotify({ id: s.id, enabled: false }); await lofi.jobWait({ id: s.id }); return s.id;";
    exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert!(jobs.drain_notices().is_empty());
}

#[tokio::test]
async fn job_unknown_id_returns_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let mut cx = trusted_ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "return await lofi.jobStatus({ id: '999' });";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value["ok"], json!(false));
    assert!(res.value["error"].as_str().unwrap().contains("no such job"));
}

#[tokio::test]
async fn job_spawn_honours_shell_policy_deny() {
    // The default (untrusted) ctx denies shell commands without a policy
    // match; a background job must not bypass that.
    let dir = tempfile::tempdir().unwrap();
    let mut cx = ctx(dir.path());
    cx.jobs = lofi_code::tools::JobRegistry::new();
    let src = "return await lofi.jobSpawn({ cmd: 'echo hi' });";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value["ok"], json!(false));
    assert!(res.value["status"].as_str().is_some(), "res: {}", res.value);
}
