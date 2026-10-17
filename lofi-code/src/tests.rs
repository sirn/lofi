#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value as Json};

use lofi_error::Error;

use super::*;
use crate::run::MAX_LOG_BYTES;

#[test]
fn exec_tool_contract_shape() {
    assert_eq!(EXEC_TOOL_NAME, "exec");
    assert!(EXEC_TOOL_DESCRIPTION.contains("sandboxed QuickJS runtime"));
    let schema = exec_tool_input_schema();
    assert_eq!(schema["properties"]["code"]["type"], "string");
    assert_eq!(schema["required"], serde_json::json!(["code"]));
    assert!(schema["properties"].get("strings").is_some());
    assert!(schema["properties"].get("display").is_some());
}

use tempfile::tempdir;

fn ctx(root: &std::path::Path) -> ExecCtx {
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
        shell_policy: crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
        confirm: None,
        auto_mode: None,
        policy_override: crate::policy::PolicyOverride::default(),
        skills_dir: None,
        truncate: crate::tools::truncate::TruncatedCap::default(),
        jobs: crate::tools::JobRegistry::new(),
        job_owner: None,
        on_job_acquired: None,
        subagent: None,
        models: None,
    }
}

fn trusted_ctx(root: &std::path::Path) -> ExecCtx {
    let mut ctx = ctx(root);
    ctx.shell_policy = crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig {
        mode: lofi_types::ShellPolicyMode::Unrestricted,
        ..lofi_types::ShellPolicyConfig::default()
    });
    ctx
}

#[test]
fn compile_ts_strips_type_annotations() {
    let js = compile_ts("const x: number = 42; return x;").unwrap();
    assert!(!js.contains(": number"));
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
async fn exec_returns_number() {
    let dir = tempdir().unwrap();
    let res = exec(
        "const x: number = 42; return x;",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert!(res.has_returned_value());
    assert_eq!(res.value, json!(42));
}

#[tokio::test]
async fn exec_distinguishes_implicit_undefined_from_null() {
    let dir = tempdir().unwrap();
    let res = exec("print('hi');", &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert!(!res.has_returned_value());
    assert_eq!(res.value, Json::Null);
    assert!(res.logs.contains("hi"));

    let res = exec("return null;", &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert!(res.has_returned_value());
    assert_eq!(res.value, Json::Null);
}

#[tokio::test]
async fn exec_pi_read_reads_a_file() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let res = exec(
        "return await lofi.read('a.txt');",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["content"], json!("hello"));
}

#[tokio::test]
async fn exec_read_image_returns_tagged_payload() {
    // 1x1 transparent PNG.
    const PNG: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x64,
        0xF8, 0xCF, 0x50, 0x0F, 0x00, 0x03, 0x86, 0x01, 0x80, 0x5A, 0x34, 0x7D, 0x6B, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("img.png"), PNG).unwrap();
    let res = exec(
        "return await lofi.read('img.png');",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["type"], json!("image"));
    assert_eq!(res.value["media_type"], json!("image/png"));
    let Some(image) = res.image else {
        panic!("missing out-of-band image");
    };
    let mut decoded = Vec::new();
    std::io::Read::read_to_end(&mut &image.file, &mut decoded).unwrap();
    assert_eq!(decoded, PNG);
    assert_eq!(image.bytes, PNG.len());
    assert!(res.value.get("image_token").is_none());
    // The text path must NOT have run (no content/lines keys).
    assert!(res.value.get("content").is_none());
}

#[tokio::test]
async fn exec_bounds_pending_image_handles() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("img.png"), b"not decoded").unwrap();
    let calls = std::iter::repeat_n("lofi.read('img.png')", 33)
        .collect::<Vec<_>>()
        .join(",");
    let source = format!("await Promise.all([{calls}]); return null;");

    let error = exec(&source, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap_err();

    assert!(error.to_string().contains("32-image limit"), "{error}");
}

#[tokio::test]
async fn exec_read_non_image_still_text() {
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "plain").unwrap();
    let res = exec(
        "return await lofi.read('note.txt');",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["content"], json!("plain"));
    assert!(res.value.get("type").is_none());
}

#[tokio::test]
async fn exec_pi_write_then_read_round_trip() {
    let dir = tempdir().unwrap();
    let src = "await lofi.write({ path: 'nested/x.txt', text: 'hi' }); return await lofi.read('nested/x.txt');";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value["content"], json!("hi"));
}

#[tokio::test]
async fn exec_path_escape_thrown_as_js_error() {
    let dir = tempdir().unwrap();
    let src = "try { await lofi.read('../escape'); return 'no-throw'; } catch (e) { return 'caught:' + e.message; }";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    let s = res.value.as_str().unwrap();
    assert!(s.starts_with("caught:"), "got {s}");
    assert!(s.contains("escapes workspace root"));
}

#[tokio::test]
async fn exec_uncaught_error_surfaces_as_sandbox_error() {
    let dir = tempdir().unwrap();
    let src = "await lofi.read('../escape'); return 'ok';";
    let err = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Sandbox(_)));
    assert!(err.to_string().contains("escapes workspace root"));
}

#[tokio::test]
async fn exec_print_buffers_logs() {
    let dir = tempdir().unwrap();
    let src = "print('a', 'b'); print('c'); return 1;";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, json!(1));
    assert_eq!(res.logs, "a b\nc\n");
}

#[tokio::test]
async fn exec_circular_return_surfaces_as_sandbox_error() {
    // A self-referential object must not recurse unboundedly; the
    // bounded converter returns the sentinel, which becomes Error::Sandbox.
    let dir = tempdir().unwrap();
    let src = "const x = { a: 1 }; x.self = x; return x;";
    let err = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Sandbox(_)));
}

#[tokio::test]
async fn exec_print_logs_are_capped() {
    // A chatty guest must not exhaust memory; logs are truncated at the cap.
    let dir = tempdir().unwrap();
    let src = "for (let i = 0; i < 1_000_000; i++) print('x'.repeat(64)); return 1;";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert!(res.logs.contains("<logs truncated>"));
    assert!(res.logs.len() < 2 * 1024 * 1024);
}

#[tokio::test]
async fn exec_print_cap_survives_multibyte_boundary() {
    // Filling the buffer to just under the cap and then printing a
    // multibyte char must not panic on a mid-character slice.
    let dir = tempdir().unwrap();
    let src = "for (let i = 0; i < 200000; i++) print('é'.repeat(20)); return 1;";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert!(res.logs.contains("<logs truncated>"));
    assert!(res.logs.len() <= MAX_LOG_BYTES);
}

#[tokio::test]
async fn exec_result_budget_truncates_before_host_conversion() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        max_value_bytes: 1024,
        ..ExecOptions::default()
    };
    let res = exec("return \"é\".repeat(1024 * 1024);", &ctx(dir.path()), &opts)
        .await
        .unwrap();

    assert!(res.value_truncated);
    assert!(res.value.as_str().unwrap().len() <= opts.max_value_bytes);
}

#[tokio::test]
async fn exec_result_budget_is_shared_with_logs() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        max_value_bytes: 1024,
        ..ExecOptions::default()
    };
    let res = exec(
        "print(\"p\".repeat(1024 * 1024)); return \"x\".repeat(1024 * 1024);",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap();

    assert!(res.logs.len() <= opts.max_value_bytes);
    assert!(res.value_truncated);
    assert_eq!(res.value.as_str(), Some(""));
}

#[tokio::test]
async fn exec_result_budget_rejects_oversized_object_key() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        max_value_bytes: 1024,
        ..ExecOptions::default()
    };
    let res = exec(
        "return { [\"x\".repeat(1024 * 1024)]: true };",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap();

    assert!(res.value_truncated);
    assert_eq!(res.value, json!({}));
}

#[tokio::test]
async fn exec_huge_sparse_array_surfaces_as_sandbox_error() {
    // A sparse array cheap to construct in JS but huge to walk must be
    // rejected before iterating, not exhaust the host.
    let dir = tempdir().unwrap();
    let src = "const a = new Array(1_000_000_000); a[0] = 1; return a;";
    let err = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Sandbox(_)));
}

#[tokio::test]
async fn exec_strings_exposed_as_lofi_strings() {
    let dir = tempdir().unwrap();
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
        shell_policy: crate::policy::defaults::resolve(&lofi_types::ShellPolicyConfig::default()),
        confirm: None,
        auto_mode: None,
        policy_override: crate::policy::PolicyOverride::default(),
        skills_dir: None,
        truncate: crate::tools::truncate::TruncatedCap::default(),
        jobs: crate::tools::JobRegistry::new(),
        job_owner: None,
        on_job_acquired: None,
        subagent: None,
        models: None,
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
    let dir = tempdir().unwrap();
    let src = "const r = await lofi.bash({ cmd: 'echo hi' }); return r.output.trim();";
    let res = exec(src, &trusted_ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, json!("hi"));
}

#[test]
fn compile_ts_malformed_escape_reports_location() {
    let err = compile_ts(r#"return "\u{GG}";"#).unwrap_err();
    let msg = match err {
        Error::Sandbox(m) => m,
        other => panic!("expected Sandbox error, got {other:?}"),
    };
    assert!(msg.starts_with("parse error"), "not a parse error: {msg}");
    assert!(
        msg.contains("at 1:"),
        "missing user-relative location: {msg}"
    );
}

#[tokio::test]
async fn exec_unicode_escapes_round_trip() {
    let dir = tempdir().unwrap();
    let cases: &[(&str, &str)] = &[
        ("return \"\\u00E9\";", "é"),
        ("return \"\\u{1F600}\";", "😀"),
        ("return \"\\uD83D\\uDE00\";", "😀"),
    ];
    for (src, expected) in cases {
        let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, json!(*expected), "{src} did not round-trip");
    }
}

#[tokio::test]
async fn exec_escaped_unicode_in_tool_arg() {
    let dir = tempdir().unwrap();
    let src = r#"const r = await lofi.bash({ cmd: "echo \u00E9" }); return r.output.trim();"#;
    let res = exec(src, &trusted_ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, json!("é"));
}

#[tokio::test]
async fn exec_sync_infinite_loop_times_out() {
    // `while (true) {}` blocks inside native `ctx.eval` and never reaches
    // an `.await` — the interrupt handler is the only thing that can
    // break it. The CPU budget accumulates and fires well under 1s.
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_millis(500),
        ..ExecOptions::default()
    };
    let err = exec("while (true) {}", &ctx(dir.path()), &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Sandbox(ref m) if m.contains("CPU budget")),
        "{err:?}"
    );
}

#[tokio::test]
async fn exec_cpu_heavy_loop_times_out() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_millis(500),
        ..ExecOptions::default()
    };
    let err = exec(
        "let x = 0; while (true) { x = (x + 1) * 3; } return x;",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, Error::Sandbox(ref m) if m.contains("CPU budget")),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_sync_loop_can_be_cancelled() {
    // The guest loop blocks inside native `ctx.eval` on this thread, so
    // the cancel flag must be set from a *separate* spawned task; the
    // QuickJS interrupt handler then sees it on the next interpreter tick
    // and aborts. A single-threaded runtime would deadlock here.
    let dir = tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = ExecOptions {
        timeout: Duration::from_secs(30),
        cancel: Some(cancel.clone()),
        ..ExecOptions::default()
    };

    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.store(true, Ordering::Relaxed);
        }
    });

    let err = exec("while (true) {}", &ctx(dir.path()), &opts)
        .await
        .unwrap_err();
    assert!(
        matches!(err, Error::Sandbox(ref m) if m.contains("cancelled")),
        "{err:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_works_after_cancelled_exec() {
    let dir = tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let exec_ctx = ctx(dir.path());
    let opts = ExecOptions {
        timeout: Duration::from_secs(30),
        cancel: Some(cancel.clone()),
        ..ExecOptions::default()
    };

    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel.store(true, Ordering::Relaxed);
        }
    });
    let _ = exec("while (true) {}", &exec_ctx, &opts).await;

    let res = exec("return 42;", &exec_ctx, &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value, json!(42));
}

#[tokio::test]
async fn exec_cannot_reenable_error_backtraces() {
    let dir = tempdir().unwrap();
    let src = r#"
        const changed = Reflect.set(Error, "stackTraceLimit", 10);
        const descriptor = Object.getOwnPropertyDescriptor(Error, "stackTraceLimit");
        return { changed, limit: Error.stackTraceLimit, descriptor };
    "#;
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value["changed"], json!(false));
    assert_eq!(res.value["limit"], json!(0));
    assert_eq!(res.value["descriptor"]["writable"], json!(false));
    assert_eq!(res.value["descriptor"]["configurable"], json!(false));
}

#[tokio::test]
async fn exec_memory_limit_aborts_unbounded_allocation() {
    // Growing an array without bound must hit the memory limit and throw
    // a JS exception (surfaced as Sandbox) rather than exhausting the host.
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_secs(5),
        ..ExecOptions::default()
    };
    let err = exec(
        "const a = []; while (true) a.push('x'.repeat(1024)); return a.length;",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Sandbox(_)), "{err:?}");
}

#[tokio::test]
async fn exec_memory_limit_aborts_unbounded_read_loop() {
    // Regression: a helper that accumulates `lofi.read` content across awaits
    // (e.g. paging on a missing `nextOffset`) must hit the memory limit and
    // throw rather than growing host RSS without bound.
    let dir = tempdir().unwrap();
    let body = ("x".repeat(80) + "\n").repeat(4000);
    std::fs::write(dir.path().join("big.txt"), body).unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_secs(5),
        ..ExecOptions::default()
    };
    let err = exec(
        "let s = ''; while (true) { s += (await lofi.read('big.txt')).content; } return s.length;",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Sandbox(_)), "{err:?}");
}

#[tokio::test]
async fn exec_deep_recursion_hits_stack_limit() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_secs(5),
        ..ExecOptions::default()
    };
    let err = exec(
        "function f() { return f(); } return f();",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::Sandbox(_)), "{err:?}");
}

#[tokio::test]
async fn exec_awaited_tool_does_not_consume_cpu_budget() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_millis(500),
        ..ExecOptions::default()
    };
    let src = r#"
        for (let i = 0; i < 3; i++) {
            await lofi.bash({ cmd: "sleep 1" });
        }
        return "done";
    "#;
    let res = exec(src, &ctx(dir.path()), &opts).await.unwrap();
    assert_eq!(res.value, json!("done"));
}

#[tokio::test]
async fn exec_docs_index_and_entry() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions::default();
    let src = r#"
        const idx = await lofi.docs();
        const names = idx.entries.map(e => e.name);
        const hasRead = names.includes("lofi.read");
        const entry = await lofi.docs("lofi.bash");
        const hasContent = entry.content.includes("timeoutMs");
        return { hasRead, hasContent, count: names.length };
    "#;
    let res = exec(src, &ctx(dir.path()), &opts).await.unwrap();
    assert_eq!(res.value["hasRead"], json!(true));
    assert_eq!(res.value["hasContent"], json!(true));
    assert!(res.value["count"].as_u64().unwrap() > 10);
}

#[tokio::test]
async fn exec_docs_search() {
    let dir = tempdir().unwrap();
    let opts = ExecOptions::default();
    let src = r#"
        const res = await lofi.docsSearch("write file");
        const topName = res.results[0].name;
        return topName;
    "#;
    let res = exec(src, &ctx(dir.path()), &opts).await.unwrap();
    assert_eq!(res.value, json!("lofi.write"));
}

#[tokio::test]
async fn exec_read_pages_whole_file_via_next_offset() {
    // The documented paging loop must reassemble a multi-page file exactly.
    let dir = tempdir().unwrap();
    let body = ("x".repeat(80) + "\n").repeat(4000);
    std::fs::write(dir.path().join("big.txt"), &body).unwrap();
    let src = r#"
        let offset = 1;
        const pages = [];
        let hops = 0;
        while (true) {
            const page = await lofi.read('big.txt', { offset });
            pages.push(page.content);
            hops++;
            if (page.next_offset === null) break;
            offset = page.next_offset;
        }
        return { joined: pages.join("\n").length, hops };
    "#;
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert!(res.value["hops"].as_u64().unwrap() > 1, "{res:?}");
    // Pages split on line boundaries, so joining with "\n" restores the file.
    assert_eq!(res.value["joined"], json!(body.len()));
}

#[tokio::test]
async fn exec_read_overlong_final_line_returns_null_cursor() {
    // A truncated last line without trailing newline consumes every line; the
    // cursor must not point past EOF, or the paging loop errors out of range.
    let dir = tempdir().unwrap();
    let body = "x".repeat(60 * 1024);
    std::fs::write(dir.path().join("long.txt"), &body).unwrap();
    let src = r"
        const page = await lofi.read('long.txt');
        return { truncated: page.truncated, next: page.next_offset };
    ";
    let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
        .await
        .unwrap();
    assert_eq!(res.value["truncated"], json!(true), "{res:?}");
    assert_eq!(res.value["next"], json!(null), "{res:?}");
}

#[tokio::test]
async fn exec_subagent_cancel_returns_promptly() {
    // While the guest awaits the subagent hook, no QuickJS bytecode ticks,
    // so the interrupt handler cannot observe the flag; the bound tool must
    // race the hook against the cancel flag itself.
    let dir = tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let opts = ExecOptions {
        timeout: Duration::from_secs(30),
        cancel: Some(cancel.clone()),
        ..ExecOptions::default()
    };
    let mut c = ctx(dir.path());
    c.subagent = Some(Arc::new(
        |_, _| -> std::pin::Pin<Box<dyn std::future::Future<Output = Json> + Send>> {
            Box::pin(std::future::pending())
        },
    ));

    tokio::spawn({
        let cancel = cancel.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.store(true, Ordering::Relaxed);
        }
    });

    // The CPU budget cannot interrupt an awaited hook, so bound the exec
    // itself: a broken cancel race must fail the test, not hang it.
    #[allow(clippy::expect_used)]
    let err = tokio::time::timeout(
        Duration::from_secs(5),
        exec("await lofi.subagent({ prompt: 'x' })", &c, &opts),
    )
    .await
    .expect("exec did not return after cancellation")
    .unwrap_err();
    assert!(
        matches!(err, Error::Sandbox(ref m) if m.contains("cancelled")),
        "{err:?}"
    );
}

#[tokio::test]
async fn exec_subagent_without_hook_reports_unavailable() {
    let dir = tempdir().unwrap();
    let res = exec(
        "return await lofi.subagent({ prompt: 'do a thing' });",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["ok"], json!(false), "{res:?}");
    assert_eq!(res.value["status"], json!("unavailable"), "{res:?}");
    assert!(
        res.value["error"]
            .as_str()
            .unwrap()
            .contains("not available"),
        "{res:?}"
    );
}

#[tokio::test]
async fn exec_subagent_requires_prompt() {
    // The hook validates and resolves; the bind layer must not intercept the
    // call or turn it into a thrown exception.
    let dir = tempdir().unwrap();
    let mut c = ctx(dir.path());
    c.subagent = Some(Arc::new(|_op, args| {
        Box::pin(async move {
            match args.get("prompt").and_then(Json::as_str) {
                Some(p) if !p.trim().is_empty() => json!({ "ok": true, "status": "completed" }),
                _ => json!({
                    "ok": false,
                    "status": "failed",
                    "error": "prompt is required and must be non-empty",
                }),
            }
        })
    }));
    let res = exec(
        "return await lofi.subagent({});",
        &c,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value["ok"], json!(false), "{res:?}");
    assert!(
        res.value["error"].as_str().unwrap().contains("prompt"),
        "{res:?}"
    );
}

#[tokio::test]
async fn exec_subagent_passes_args_and_returns_result() {
    let dir = tempdir().unwrap();
    let mut c = ctx(dir.path());
    c.subagent = Some(Arc::new(|_op, args| {
        Box::pin(async move {
            json!({
                "ok": true,
                "id": "s1",
                "status": "completed",
                "result": format!("echo: {}", args["prompt"].as_str().unwrap()),
            })
        })
    }));
    let res = exec(
        "const r = await lofi.subagent({ prompt: 'hello', timeout_ms: 1000 }); return r.result;",
        &c,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value, json!("echo: hello"));
}

#[tokio::test]
async fn exec_models_lists_choices() {
    let dir = tempdir().unwrap();
    let mut c = ctx(dir.path());
    c.models = Some(Arc::new(
        || json!({ "models": [{ "model": "p/m", "name": "M", "thinking_levels": ["off"], "service_tiers": [] }] }),
    ));
    let res = exec(
        "const m = await lofi.models(); return m.models[0].model;",
        &c,
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value, json!("p/m"));
}

#[tokio::test]
async fn exec_models_without_hook_reports_unavailable() {
    let dir = tempdir().unwrap();
    let res = exec(
        "const m = await lofi.models(); return m.status;",
        &ctx(dir.path()),
        &ExecOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(res.value, json!("unavailable"));
}

#[tokio::test]
async fn exec_tool_call_budget_aborts_await_loop() {
    // Await-heavy loops barely tick the interrupt handler, so the CPU budget
    // cannot bound them. Every awaited host call funnels through the tool
    // wrapper, whose per-exec call budget must abort the loop instead.
    let dir = tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let opts = ExecOptions {
        timeout: Duration::from_secs(30),
        ..ExecOptions::default()
    };
    let err = exec(
        "while (true) { await lofi.read('a.txt'); } return 1;",
        &ctx(dir.path()),
        &opts,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, Error::Sandbox(ref m) if m.contains("tool call budget")),
        "{err:?}"
    );
}
