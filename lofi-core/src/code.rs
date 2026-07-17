//! Code-mode sandbox: `TypeScript` strip (swc) + `QuickJS` runtime.
//!
//! The agent's single tool is `exec`, which compiles a TypeScript snippet to
//! JS (types stripped via swc) and runs it in an embedded `QuickJS` runtime.
//! Tools are exposed as native methods on a global `pi` object; top-level
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

use crate::error::{Error, Result};
use crate::tools::builtins::BuiltinTools;

/// Default wall-clock budget for a single `exec` call (120s).
pub const DEFAULT_GUEST_TIMEOUT: Duration = Duration::from_mins(2);

/// Sentinel object key used by the IIFE wrapper to surface an uncaught guest
/// exception as a resolved (not rejected) promise.
const SANDBOX_ERROR_KEY: &str = "__lofi_sandbox_error__";

/// Pluggable subagent callback used by `pi.agent`/`pi.spawn`.
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

/// Per-call execution context: workspace root, named strings, and an optional
/// subagent callback.
#[derive(Clone)]
pub struct ExecCtx {
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Named strings exposed as the global `π` / `pi_strings` objects.
    pub strings: HashMap<String, String>,
    /// Optional `pi.agent` / `pi.spawn` implementation.
    pub agent: Option<AgentFn>,
}

impl std::fmt::Debug for ExecCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("root", &self.root)
            .field("strings", &self.strings)
            .field("agent", &self.agent.is_some())
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
/// # Errors
/// Returns [`Error::Sandbox`] for compile failures, guest timeouts, guest
/// exceptions, or tool errors that propagate as thrown exceptions.
pub async fn exec(src: &str, ctx: &ExecCtx, opts: &ExecOptions) -> Result<ExecResult> {
    let js = compile_ts(src)?;

    let rt = AsyncRuntime::new().map_err(|e| Error::Sandbox(format!("runtime: {e}")))?;
    let actx = AsyncContext::full(&rt)
        .await
        .map_err(|e| Error::Sandbox(format!("context: {e}")))?;

    let tools = Arc::new(BuiltinTools::new(ctx.root.clone()));
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

/// Install the `pi` object, `print`, and the `π` / `pi_strings` globals.
fn install_globals(
    ctx: &Ctx<'_>,
    tools: &Arc<BuiltinTools>,
    strings: &HashMap<String, String>,
    agent: Option<AgentFn>,
    logs: &Arc<Mutex<String>>,
) -> rquickjs::Result<()> {
    let pi = Object::new(ctx.clone())?;
    bind_tools(ctx, &pi, tools, agent)?;
    ctx.globals().set("pi", pi)?;

    let print = Function::new(ctx.clone(), {
        let logs = logs.clone();
        move |args: Rest<Coerced<std::string::String>>| -> rquickjs::Result<()> {
            let mut line = String::new();
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    line.push(' ');
                }
                line.push_str(arg);
            }
            line.push('\n');
            if let Ok(mut buf) = logs.lock() {
                buf.push_str(&line);
            }
            Ok(())
        }
    })?;
    ctx.globals().set("print", print)?;

    let strings_obj = Object::new(ctx.clone())?;
    for (k, v) in strings {
        strings_obj.set(k.as_str(), v.as_str())?;
    }
    ctx.globals().set("π", strings_obj.clone())?;
    ctx.globals().set("pi_strings", strings_obj)?;
    Ok(())
}

/// Bind the builtin file/shell tool methods onto `pi`.
fn bind_tools<'js>(
    ctx: &Ctx<'js>,
    pi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    agent: Option<AgentFn>,
) -> rquickjs::Result<()> {
    bind_file_tools(ctx, pi, tools)?;
    bind_agent_tool(ctx, pi, agent)?;
    Ok(())
}

/// Bind `read`/`ls`/`find`/`grep`/`write`/`edit`/`bash`.
///
/// The async closures deliberately do **not** capture a `Ctx` clone: doing
/// so would create a `context -> globals -> pi -> function -> Ctx -> context`
/// reference cycle that never collects and trips `QuickJS`'s `gc_obj_list`
/// assertion at runtime shutdown. `rquickjs` passes the call-site `Ctx` to
/// `JsonV::into_js` for us, so the closures only need to capture the `Arc`
/// tool bundle.
#[allow(clippy::too_many_lines)]
fn bind_file_tools<'js>(
    ctx: &Ctx<'js>,
    pi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
) -> rquickjs::Result<()> {
    let t = tools.clone();
    pi.set(
        "read",
        Function::new(
            ctx.clone(),
            Async(move |path: String| {
                let t = t.clone();
                async move { tool_result(t.read(&path).await) }
            }),
        )?,
    )?;

    let t = tools.clone();
    pi.set(
        "ls",
        Function::new(
            ctx.clone(),
            Async(move |dir: Opt<String>| {
                let t = t.clone();
                async move { tool_result(t.ls(dir.0.as_deref().unwrap_or("")).await) }
            }),
        )?,
    )?;

    let t = tools.clone();
    pi.set(
        "find",
        Function::new(
            ctx.clone(),
            Async(move |glob: String, dir: Opt<String>| {
                let t = t.clone();
                async move { tool_result(t.find(&glob, dir.0.as_deref()).await) }
            }),
        )?,
    )?;

    let t = tools.clone();
    pi.set(
        "grep",
        Function::new(
            ctx.clone(),
            Async(move |pattern: Value, path: Opt<String>| {
                let t = t.clone();
                let p = js_to_json(&pattern);
                let path = path.0;
                async move { tool_result(t.grep(p, path.as_deref()).await) }
            }),
        )?,
    )?;

    let t = tools.clone();
    pi.set(
        "write",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                async move { tool_result(t.write(args).await) }
            }),
        )?,
    )?;

    let t = tools.clone();
    pi.set(
        "edit",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                async move { tool_result(t.edit(args).await) }
            }),
        )?,
    )?;

    let t = tools.clone();
    pi.set(
        "bash",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                async move { tool_result(t.bash(args).await) }
            }),
        )?,
    )?;

    Ok(())
}

/// Bind `agent` / `spawn`.
fn bind_agent_tool<'js>(
    ctx: &Ctx<'js>,
    pi: &Object<'js>,
    agent: Option<AgentFn>,
) -> rquickjs::Result<()> {
    pi.set(
        "agent",
        Function::new(
            ctx.clone(),
            Async(move |prompt: String, opts: Opt<Value>| {
                let agent = agent.clone();
                let opts_json = opts.0.map(|v| js_to_json(&v)).filter(|v| !v.is_null());
                async move {
                    let Some(agent) = agent else {
                        return Err(rquickjs::Error::IntoJs {
                            from: "lofi",
                            to: "value",
                            message: Some("agent() is not available in this context".to_string()),
                        });
                    };
                    let req = AgentRequest {
                        prompt,
                        opts: opts_json,
                    };
                    match agent(req).await {
                        Ok(s) => Ok(JsonV(json!(s))),
                        Err(e) => Err(rquickjs::Error::IntoJs {
                            from: "lofi",
                            to: "value",
                            message: Some(e.to_string()),
                        }),
                    }
                }
            }),
        )?,
    )?;
    Ok(())
}

/// Translate a builtin tool [`Result`] into a `rquickjs::Result<JsonV>`,
/// surfacing tool errors as a thrown JS `Error` (via rquickjs's `IntoJs`
/// error path, which is leak-free in rquickjs 0.9).
fn tool_result(res: std::result::Result<Json, Error>) -> rquickjs::Result<JsonV> {
    match res {
        Ok(v) => Ok(JsonV(v)),
        Err(e) => Err(rquickjs::Error::IntoJs {
            from: "lofi",
            to: "value",
            message: Some(e.to_string()),
        }),
    }
}

/// Convert a `serde_json::Value` into a rquickjs value.
fn json_to_js<'js>(ctx: &Ctx<'js>, v: &Json) -> rquickjs::Result<Value<'js>> {
    let val: Value = match v {
        Json::Null => Value::new_null(ctx.clone()),
        Json::Bool(b) => b.into_js(ctx)?,
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                // JSON integers are bound to i64; rquickjs has no integer
                // type wider than f64 on the JS side, so a widening cast is
                // unavoidable. Loss only matters for |i| > 2^53, which we
                // accept for sandbox return values.
                #[allow(clippy::cast_precision_loss)]
                (i as f64).into_js(ctx)?
            } else {
                n.as_f64().unwrap_or(f64::NAN).into_js(ctx)?
            }
        }
        Json::String(s) => s.into_js(ctx)?,
        Json::Array(arr) => {
            let a = Array::new(ctx.clone())?;
            for (i, item) in arr.iter().enumerate() {
                a.set(i, json_to_js(ctx, item)?)?;
            }
            a.into_value()
        }
        Json::Object(map) => {
            let o = Object::new(ctx.clone())?;
            for (k, v) in map {
                o.set(k.as_str(), json_to_js(ctx, v)?)?;
            }
            o.into_value()
        }
    };
    Ok(val)
}

/// Convert a rquickjs value into a `serde_json::Value`.
///
/// `undefined` maps to `Null` so a guest `return;` (or no return) yields
/// `Value::Null` rather than vanishing. Functions and symbols stringify.
fn js_to_json(v: &Value<'_>) -> Json {
    if v.is_undefined() || v.is_null() {
        return Json::Null;
    }
    if let Some(b) = v.as_bool() {
        return json!(b);
    }
    if let Some(i) = v.as_int() {
        return json!(i);
    }
    if let Some(f) = v.as_float() {
        if f.fract() == 0.0 && f.abs() < 9.0072e15 {
            // The value is an integer-valued float inside the safe-i64 range;
            // the truncation cast is guarded by the bound above.
            #[allow(clippy::cast_possible_truncation)]
            return json!(f as i64);
        }
        return json!(f);
    }
    if v.is_string() {
        if let Some(s) = v.as_string() {
            return json!(s.to_string().unwrap_or_default());
        }
    }
    if v.is_array() {
        if let Some(arr) = v.as_array() {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr.iter::<Value>() {
                let item = item.unwrap_or_else(|_| Value::new_undefined(v.ctx().clone()));
                out.push(js_to_json(&item));
            }
            return Json::Array(out);
        }
    }
    if v.is_object() {
        if let Some(obj) = v.as_object() {
            let mut map = serde_json::Map::new();
            for (k, val) in obj.props::<std::string::String, Value>().flatten() {
                map.insert(k, js_to_json(&val));
            }
            return Json::Object(map);
        }
    }
    Json::Null
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use tempfile::tempdir;

    fn ctx(root: &std::path::Path) -> ExecCtx {
        ExecCtx {
            root: root.to_path_buf(),
            strings: HashMap::new(),
            agent: None,
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
            "return await pi.read('a.txt');",
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
        let src = "await pi.write({ path: 'nested/x.txt', text: 'hi' }); return await pi.read('nested/x.txt');";
        let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, json!("hi"));
    }

    #[tokio::test]
    async fn exec_path_escape_thrown_as_js_error() {
        let dir = tempdir().unwrap();
        let src = "try { await pi.read('../escape'); return 'no-throw'; } catch (e) { return 'caught:' + e.message; }";
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
        let src = "await pi.read('../escape'); return 'ok';";
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
    async fn exec_strings_exposed_as_pi_and_pi_strings() {
        let dir = tempdir().unwrap();
        let mut strings = HashMap::new();
        strings.insert("greeting".to_string(), "hello".to_string());
        let cx = ExecCtx {
            root: dir.path().to_path_buf(),
            strings,
            agent: None,
        };
        let res = exec(
            "return π.greeting + '|' + pi_strings.greeting;",
            &cx,
            &ExecOptions::default(),
        )
        .await
        .unwrap();
        assert_eq!(res.value, json!("hello|hello"));
    }

    #[tokio::test]
    async fn exec_bash_echo() {
        let dir = tempdir().unwrap();
        let src = "const r = await pi.bash({ cmd: 'echo hi' }); return r.output.trim();";
        let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, json!("hi"));
    }
}
