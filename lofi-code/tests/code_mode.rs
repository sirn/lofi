//! Integration tests for the code-mode sandbox (`lofi_code`).
//!
//! These exercise the public `compile_ts`/`exec` surface end-to-end against a
//! real `tempfile::TempDir` workspace root: TS type stripping, value returns,
//! top-level await, the `lofi.read`/`lofi.write` tool bridge, path-escape errors,
//! and `print` log buffering. The `agent` callback is stubbed since the
//! agent loop lands in a later step.

#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]

use std::collections::HashMap;
use std::path::Path;

use lofi_code::{compile_ts, exec, AgentFn, AgentRequest, BashEnv, ExecCtx, ExecOptions, ToolEvent};
use serde_json::{json, Value};

/// Build an `ExecCtx` rooted at `root` with no named strings and a stub
/// `agent` callback that errors if invoked (the tests never call it).
fn ctx(root: &Path) -> ExecCtx {
    let tmp_dir = std::env::temp_dir().join("lofi-test");
    let _ = std::fs::create_dir_all(&tmp_dir);
    ExecCtx {
        root: root.to_path_buf(),
        tmp_dir,
        strings: HashMap::new(),
        agent: None,
        on_tool_event: None,
            recall: None,
        bash_env: BashEnv::default(),
    }
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
    assert_eq!(res.value, json!("hello world"));
}

#[tokio::test]
async fn pi_write_then_read_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let src = "await lofi.write({ path: 'out/nested/x.txt', text: 'hi' }); return await lofi.read('out/nested/x.txt');";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, json!("hi"));
}

#[tokio::test]
async fn path_escape_throws_a_js_error() {
    let dir = tempfile::tempdir().unwrap();
    // The IIFE wrapper would turn an uncaught throw into a sandbox error;
    // catching explicitly lets us inspect the message and confirm the tool
    // surfaced the escape as a thrown JS `Error`.
    let src = "try { await lofi.read('../escape'); return 'no-throw'; } catch (e) { return 'caught:' + e.message; }";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s = res.value.as_str().unwrap();
    assert!(s.starts_with("caught:"), "got {s}");
    assert!(
        s.contains("escapes workspace root"),
        "escape message missing: {s}"
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
        agent: None,
        on_tool_event: None,
            recall: None,
        bash_env: BashEnv::default(),
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
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s: &str = res.value.as_str().unwrap();
    assert_eq!(s, "hi");
}

#[tokio::test]
async fn exec_read_tmp_pages_bash_log() {
    let dir = tempfile::tempdir().unwrap();
    // Generate enough output to trigger tail truncation + a tmp log file.
    let src = "const r = await lofi.bash({ cmd: 'for i in $(seq 1 5000); do echo \"output line number $i with some padding text to make it longer\"; done' }); return r.output;";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let out: &str = res.value.as_str().unwrap();
    assert!(out.contains("Full output:"), "got: {out}");
    let basename = out
        .split("lofi-bash-")
        .nth(1)
        .and_then(|s| s.split('.').next())
        .map(|s| format!("lofi-bash-{s}.log"))
        .expect("notice should name a log file");
    let src2 = format!(
        "const r = await lofi.read_tmp({basename:?}, {{ offset: 1, limit: 3 }}); return r;"
    );
    let res2 = exec(&src2, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s2: &str = res2.value.as_str().unwrap();
    assert!(s2.contains("output line number 1"), "first page should start at line 1: basename={basename:?} s2={s2}");
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
async fn agent_call_emits_tool_events() {
    use std::sync::{Arc, Mutex};
    let dir = tempfile::tempdir().unwrap();
    let events: Arc<Mutex<Vec<ToolEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let cb = {
        let events = events.clone();
        Arc::new(move |ev: ToolEvent| events.lock().unwrap().push(ev))
            as Arc<dyn Fn(ToolEvent) + Send + Sync>
    };
    let agent: AgentFn = Arc::new(|_req: AgentRequest| {
        Box::pin(async { Ok("subagent reply".to_string()) })
    });
    let cx = ExecCtx {
        root: dir.path().to_path_buf(),
        tmp_dir: std::env::temp_dir().join("lofi-test"),
        strings: HashMap::new(),
        agent: Some(agent),
        on_tool_event: Some(cb),
        recall: None,
        bash_env: BashEnv::default(),
    };
    let src = "const r = await lofi.agent('do stuff'); return r;";
    let res = exec(src, &cx, &ExecOptions::default()).await.unwrap();
    assert_eq!(res.value.as_str().unwrap(), "subagent reply");
    let evs = events.lock().unwrap();
    let has = |name: &str| {
        evs.iter().any(|e| matches!(e, ToolEvent::Start { name: n, .. } if n == name))
            && evs.iter().any(|e| matches!(e, ToolEvent::End { .. }))
    };
    assert!(has("agent"), "missing agent tool events: {evs:?}");
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