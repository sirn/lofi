//! Code-mode sandbox: `TypeScript` strip (swc) + `QuickJS` runtime.
//!
//! The agent's single tool is `exec`, which compiles a TypeScript snippet to
//! JS (types stripped via swc) and runs it in an embedded `QuickJS` runtime.
//! Tools are exposed as native methods on a global `lofi` object; top-level
//! `await`/`return` work via an async-IIFE wrapper.
//!
//! ## Async bridge
//!
//! `rquickjs`'s `futures` feature gives us [`AsyncRuntime`]/[`AsyncContext`]
//! and the `async_with!` macro. Native tool functions are `Async` closures
//! returning `rquickjs::Result<JsonV>`; rquickjs wraps their future in a
//! resolved/rejected promise via `Promise::wrap_future`. The guest's IIFE is
//! evaluated to a promise and awaited with `Promise::into_future`; the
//! `async_with!` driver polls both the guest future and rquickjs's internal
//! spawner (which runs the tool futures) on each wake, so tokio-backed tools
//! (e.g. `bash`) drive naturally on the host runtime.
//!
//! Tool errors are surfaced as thrown JS exceptions: the closure returns
//! `Err(rquickjs::Error::IntoJs { message })`, which rquickjs converts into a
//! rejected promise carrying a JS `Error` whose `.message` is the tool error
//! text. The IIFE wrapper installs a top-level `try`/`catch` so an uncaught
//! tool error becomes a structured `{ __lofi_sandbox_error__ }` result instead
//! of a rejected promise (rejected promises retain their rejection value in
//! rquickjs 0.9 and trip a runtime-GC assertion at shutdown; resolving always
//! is leak-free).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::LocalBoxFuture;
use rquickjs::async_with;
use rquickjs::prelude::*;
use rquickjs::{Array, AsyncContext, AsyncRuntime, Ctx, Function, IntoJs, Object, Promise, Value};
use serde_json::{json, Value as Json};
use swc_common::sync::Lrc;
use swc_common::{FileName, FilePathMapping, Globals, SourceMap, GLOBALS};
use swc_ecma_ast::Module;
use swc_ecma_codegen::to_code_default;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_typescript::strip_type;
use swc_ecma_visit::VisitMutWith;

use lofi_error::{Error, Result};

mod convert;
use convert::{js_to_json, json_to_js};

mod bind;
use bind::bind_tools;

pub mod tools;

pub use tools::BashEnv;
use crate::tools::BuiltinTools;

/// Default wall-clock budget for a single `exec` call (120s).
pub const DEFAULT_GUEST_TIMEOUT: Duration = Duration::from_mins(2);

/// Sentinel object key used by the IIFE wrapper to surface an uncaught guest
/// exception as a resolved (not rejected) promise.
const SANDBOX_ERROR_KEY: &str = "__lofi_sandbox_error__";

/// Cap on buffered `print` output retained for the result.
const MAX_LOG_BYTES: usize = 1024 * 1024;
/// Depth and node caps for converting a guest value to `JSON`, guarding against
/// self-referential or pathologically nested structures.
const JS_TO_JSON_MAX_DEPTH: usize = 64;
const JS_TO_JSON_MAX_NODES: usize = 10_000;
/// Maximum cumulative bytes of converted strings and object keys, so a single
/// huge guest string cannot exhaust host memory despite staying within the
/// node budget.
const JS_TO_JSON_MAX_BYTES: usize = 64 * 1024 * 1024;

/// Pluggable subagent callback used by `lofi.agent`/`lofi.spawn`.
///
/// The real implementation is wired in `agent.rs`; the sandbox only needs a
/// way to invoke one nested round-trip, so it carries an `Arc`'d async
/// closure. The returned future is a [`LocalBoxFuture`] (not `Send`):
/// `rquickjs`'s `AsyncRuntime` is `!Send` without the `parallel` feature, so
/// the nested loop — which drives the code-mode sandbox — is inherently
/// single-threaded and the closure future is `!Send`.
pub type AgentFn =
    Arc<dyn Fn(AgentRequest) -> LocalBoxFuture<'static, Result<String>> + Send + Sync>;

/// A request to a nested agent.
#[derive(Debug, Clone)]
pub struct AgentRequest {
    /// The user prompt for the subagent.
    pub prompt: String,
    /// Optional caller-supplied options (model override, system prompt, ...).
    pub opts: Option<Json>,
}

/// A native tool call observed inside the sandbox, forwarded to the UI so
/// each `lofi.bash` / `lofi.read` / ... can be rendered as its own line
/// under the parent `exec` block. `id` is a per-exec counter.
#[derive(Debug, Clone)]
pub enum ToolEvent {
    Start { id: u64, name: String, args: String },
    End { id: u64, result: String, is_error: bool },
}

/// Per-call execution context: workspace root, named strings, and an optional
/// subagent callback.
#[derive(Clone)]
pub struct ExecCtx {
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Per-session tmp directory for bash full-output logs and other
    /// agent-produced artifacts. `lofi.read_tmp` is rooted here.
    pub tmp_dir: PathBuf,
    /// Named strings exposed as the global `lofi_strings` object.
    pub strings: HashMap<String, String>,
    /// Optional `lofi.agent` / `lofi.spawn` implementation.
    pub agent: Option<AgentFn>,
    pub on_tool_event: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    /// Resolved `bash` child-env policy + output-redaction set.
    pub bash_env: BashEnv,
}

impl std::fmt::Debug for ExecCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("root", &self.root)
            .field("tmp_dir", &self.tmp_dir)
            .field("strings", &self.strings)
            .field("agent", &self.agent.is_some())
            .field("on_tool_event", &self.on_tool_event.is_some())
            .field("bash_env", &self.bash_env)
            .finish()
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Wall-clock budget for the guest.
    pub timeout: Duration,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_GUEST_TIMEOUT,
        }
    }
}

/// The outcome of a single `exec` call.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// The value the guest IIFE resolved to (`null` if it resolved to
    /// `undefined` or produced no `return`).
    pub value: Json,
    /// Buffered `print(...)` output, joined and newline-terminated.
    pub logs: String,
}

/// Newtype wrapper letting `serde_json::Value` cross the `IntoJs` boundary.
///
/// We build JS values from the Rust side inside rquickjs's spawned tool
/// futures. Returning a raw `rquickjs::Value` from such a future confuses
/// rquickjs 0.9's promise bookkeeping and leaks the value at runtime
/// shutdown; converting a plain Rust enum via a custom `IntoJs` is leak-free.
struct JsonV(Json);

impl<'js> IntoJs<'js> for JsonV {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        json_to_js(ctx, &self.0)
    }
}

/// Compile a TypeScript snippet to a runnable JS string.
///
/// The user body is wrapped in `(async () => { try { <body> } catch ... })()`
/// so top-level `await`/`return` work and uncaught exceptions surface as a
/// structured result. Types are stripped with swc's TypeScript strip pass
/// (no type-checking).
///
/// # Errors
/// Returns [`Error::Sandbox`] on a parse or codegen failure.
pub fn compile_ts(src: &str) -> Result<String> {
    let wrapped = format!(
        "(async () => {{ try {{\n{src}\n}} catch (e) {{ return {{ {SANDBOX_ERROR_KEY}: (e && e.message) ? e.message : String(e) }}; }} }})()"
    );

    let cm: Lrc<SourceMap> = Lrc::new(SourceMap::new(FilePathMapping::empty()));
    let fm = cm.new_source_file(FileName::Custom("lofi.ts".into()).into(), wrapped);

    let code = GLOBALS.set(&Globals::new(), || -> Result<String> {
        let mut parser = Parser::new(
            Syntax::Typescript(TsSyntax::default()),
            StringInput::from(&*fm),
            None,
        );
        let mut module: Module = parser
            .parse_module()
            .map_err(|e| Error::Sandbox(format!("parse error: {}", e.kind().msg())))?;
        if let Some(e) = parser.take_errors().into_iter().next() {
            // Non-fatal recovered errors: report the first as a sandbox error
            // so typos don't silently produce wrong code.
            return Err(Error::Sandbox(format!("parse error: {}", e.kind().msg())));
        }
        module.visit_mut_with(&mut strip_type());
        let js = to_code_default(cm, None, &module);
        Ok(js)
    })?;

    Ok(code)
}

/// Compile and run `src` in a fresh `QuickJS` runtime.
///
/// The wall-clock timeout is cooperative: it bounds awaited native tool
/// calls, but a *synchronous* infinite loop (e.g. `while (true) {}`) blocks
/// inside `ctx.eval` and cannot be interrupted, because `rquickjs` 0.9
/// exposes no safe interrupt-handler or memory-limit API and this crate
/// forbids `unsafe`. `print` output and the returned value are still bounded
/// (see [`MAX_LOG_BYTES`] and [`js_to_json`]).
///
/// # Errors
/// Returns [`Error::Sandbox`] for compile failures, guest timeouts, guest
/// exceptions, or tool errors that propagate as thrown exceptions.
pub async fn exec(src: &str, ctx: &ExecCtx, opts: &ExecOptions) -> Result<ExecResult> {
    let js = compile_ts(src)?;

    let rt = AsyncRuntime::new().map_err(|e| Error::Sandbox(format!("runtime: {e}")))?;
    let actx = AsyncContext::full(&rt)
        .await
        .map_err(|e| Error::Sandbox(format!("context: {e}")))?;

    let tools = Arc::new(BuiltinTools::with_tool_cb(
        ctx.root.clone(),
        ctx.on_tool_event.clone(),
        ctx.tmp_dir.clone(),
        ctx.bash_env.clone(),
    ));
    let strings = ctx.strings.clone();
    let agent = ctx.agent.clone();
    let logs = Arc::new(Mutex::new(String::new()));

    let outcome = tokio::time::timeout(opts.timeout, async {
        async_with!(&actx => |ctx| {
            install_globals(&ctx, &tools, &strings, agent, &logs)
                .map_err(|e| Error::Sandbox(format!("install: {e}")))?;
            let promise: Promise = ctx
                .eval(js.as_str())
                .map_err(|e| Error::Sandbox(format!("eval: {e}")))?;
            let value: Value = promise
                .into_future()
                .await
                .map_err(|e| Error::Sandbox(format!("guest promise rejected: {e}")))?;
            let json = js_to_json(&value);
            if let Some(err) = json.get(SANDBOX_ERROR_KEY).and_then(Json::as_str) {
                return Err::<ExecResult, Error>(Error::Sandbox(err.to_string()));
            }
            Ok::<ExecResult, Error>(ExecResult {
                value: json,
                logs: logs.lock().ok().map(|mut l| std::mem::take(&mut *l)).unwrap_or_default(),
            })
        })
        .await
    })
    .await;

    match outcome {
        Ok(inner) => inner,
        Err(_) => Err(Error::Sandbox(format!(
            "guest timeout after {}ms",
            opts.timeout.as_millis()
        ))),
    }
}

/// Install the `lofi` object, `print`, and the `lofi_strings` global.
fn install_globals(
    ctx: &Ctx<'_>,
    tools: &Arc<BuiltinTools>,
    strings: &HashMap<String, String>,
    agent: Option<AgentFn>,
    logs: &Arc<Mutex<String>>,
) -> rquickjs::Result<()> {
    let lofi = Object::new(ctx.clone())?;
    bind_tools(ctx, &lofi, tools, agent)?;
    // Expose the per-session tmp dir path so the model knows where bash
    // full-output logs live (and can reference them if needed beyond
    // `lofi.read_tmp`, which takes a basename relative to this dir).
    lofi.set(
        "tmp_dir",
        tools.tmp_dir().to_string_lossy().to_string(),
    )?;
    ctx.globals().set("lofi", lofi)?;

    let print = Function::new(ctx.clone(), {
        let logs = logs.clone();
        move |args: Rest<Coerced<std::string::String>>| -> rquickjs::Result<()> {
            let mut line = String::new();
            // Cap the assembled line at the log budget while building it, so a
            // single huge argument cannot allocate an unbounded `line` before
            // the post-hoc cap runs. (The per-arg `Coerced<String>` is owned
            // by the runtime; this prevents amplifying it via concatenation.)
            for (i, arg) in args.iter().enumerate() {
                if line.len() >= MAX_LOG_BYTES {
                    break;
                }
                if i > 0 {
                    line.push(' ');
                }
                let room = MAX_LOG_BYTES.saturating_sub(line.len());
                if arg.len() <= room {
                    line.push_str(arg);
                } else {
                    let take = arg.floor_char_boundary(room);
                    line.push_str(&arg[..take]);
                    break;
                }
            }
            line.push('\n');
            if let Ok(mut buf) = logs.lock() {
                // Bound retained logs so a chatty guest can't exhaust memory.
                // Reserve room for the truncation marker and clamp to a valid
                // char boundary so a multibyte tail doesn't panic.
                const MARKER: &str = "\n<logs truncated>";
                if buf.ends_with(MARKER) {
                    return Ok(());
                }
                let cap = MAX_LOG_BYTES.saturating_sub(MARKER.len());
                let room = cap.saturating_sub(buf.len());
                if room == 0 {
                    buf.push_str(MARKER);
                    return Ok(());
                }
                let take = line.len().min(room);
                let take = line.floor_char_boundary(take);
                buf.push_str(&line[..take]);
                if take < line.len() || buf.len() >= cap {
                    buf.push_str(MARKER);
                }
            }
            Ok(())
        }
    })?;
    ctx.globals().set("print", print)?;

    let strings_obj = Object::new(ctx.clone())?;
    for (k, v) in strings {
        strings_obj.set(k.as_str(), v.as_str())?;
    }
    ctx.globals().set("lofi_strings", strings_obj)?;
    Ok(())
}

/// First line of `s`, truncated to `cap` visible chars with an ellipsis.
/// Parse an optional `{ limit?: number }` argument object for `lofi.ls`/
/// `lofi.find`.
fn parse_limit_opt(opts: Opt<Value>) -> Option<u64> {
    let v = opts.0?;
    let json = js_to_json(&v);
    let obj = json.as_object()?;
    obj.get("limit").and_then(serde_json::Value::as_u64)
}

/// Parse the optional `{ offset?, limit? }` argument object for `lofi.read`.
/// Accepts either an object (`{ offset: 10, limit: 20 }`) or nothing.
fn parse_read_opts(opts: Opt<Value>) -> (Option<u64>, Option<u64>) {
    let Some(v) = opts.0 else { return (None, None); };
    let json = js_to_json(&v);
    let Some(obj) = json.as_object() else { return (None, None) };
    let offset = obj.get("offset").and_then(serde_json::Value::as_u64);
    let limit = obj.get("limit").and_then(serde_json::Value::as_u64);
    (offset, limit)
}

fn cap_first_line(s: &str, cap: usize) -> String {
    let line = s.split('\n').next().unwrap_or("");
    let mut chars = line.chars();
    let mut out = String::new();
    for _ in 0..cap {
        match chars.next() {
            Some(c) => out.push(c),
            None => break,
        }
    }
    if chars.next().is_some() {
        out.push('…');
    }
    out
}

/// Short label for a native tool's arguments, shown after the tool name.
fn native_args_label(name: &str, v: &serde_json::Value) -> String {
    let pick = |key: &str| v.get(key).and_then(serde_json::Value::as_str).map(std::string::ToString::to_string);
    let raw = match name {
        "write" | "edit" => pick("path"),
        "bash" => pick("cmd"),
        _ => None,
    };
    let s = raw.unwrap_or_else(|| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    cap_first_line(&s, 120)
}

/// Render a native tool result as the string the UI should display, plus
/// whether it was an error. Plain strings (e.g. `read` output) are passed
/// through; structured results (e.g. `bash`) keep their JSON so the UI can
/// pull out the interesting field.
fn tool_preview(res: &std::result::Result<Json, Error>) -> (String, bool) {
    match res {
        Ok(v) => {
            // Hand the UI a ready-to-show string: plain strings (read) pass
            // through; structured results (bash) yield their `output` field
            // so the TUI never has to parse JSON.
            let s = match v {
                Json::String(s) => s.clone(),
                Json::Object(map) if map.contains_key("output") => map
                    .get("output")
                    .and_then(serde_json::Value::as_str).map_or_else(|| v.to_string(), std::string::ToString::to_string),
                other => other.to_string(),
            };
            (s, false)
        }
        Err(e) => (e.to_string(), true),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tempfile::tempdir;

    fn ctx(root: &std::path::Path) -> ExecCtx {
        let tmp_dir = std::env::temp_dir().join("lofi-test");
        let _ = std::fs::create_dir_all(&tmp_dir);
        ExecCtx {
            root: root.to_path_buf(),
            tmp_dir,
            strings: HashMap::new(),
            agent: None,
            on_tool_event: None,
            bash_env: BashEnv::default(),
        }
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
        assert_eq!(res.value, json!(42));
    }

    #[tokio::test]
    async fn exec_returns_undefined_as_null() {
        let dir = tempdir().unwrap();
        let res = exec("print('hi');", &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, Json::Null);
        assert!(res.logs.contains("hi"));
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
        assert_eq!(res.value, json!("hello"));
    }

    #[tokio::test]
    async fn exec_pi_write_then_read_round_trip() {
        let dir = tempdir().unwrap();
        let src = "await lofi.write({ path: 'nested/x.txt', text: 'hi' }); return await lofi.read('nested/x.txt');";
        let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, json!("hi"));
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
        // The IIFE wrapper catches uncaught exceptions and returns the
        // sentinel object; `exec` rewrites that into Error::Sandbox.
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
            agent: None,
            on_tool_event: None,
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
        let dir = tempdir().unwrap();
        let src = "const r = await lofi.bash({ cmd: 'echo hi' }); return r.output.trim();";
        let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, json!("hi"));
    }
}
