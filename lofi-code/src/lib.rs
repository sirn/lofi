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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::LocalBoxFuture;
use rquickjs::async_with;
use rquickjs::prelude::*;
use rquickjs::{Array, AsyncContext, AsyncRuntime, Ctx, Function, IntoJs, Object, Promise, Value};
use serde_json::{json, Value as Json};
use swc_common::sync::Lrc;
use swc_common::{FileName, FilePathMapping, Globals, SourceMap, Span, Spanned, GLOBALS};
use swc_ecma_ast::Module;
use swc_ecma_codegen::to_code_default;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_typescript::strip_type;
use swc_ecma_visit::VisitMutWith;

use lofi_error::{Error, Result};

pub mod docs;
mod convert;
use convert::{js_to_json, json_to_js};

mod bind;
use bind::bind_tools;

pub mod tools;

pub use tools::BashEnv;
use crate::tools::BuiltinTools;

/// Default wall-clock budget for a single `exec` call (120s).
pub const DEFAULT_GUEST_TIMEOUT: Duration = Duration::from_mins(2);

/// Maximum heap the `QuickJS` runtime may allocate before `JS_SetMemoryLimit`
/// rejects further growth. Generous but finite — prevents a runaway guest
/// (e.g. building an unbounded array) from exhausting host memory.
const GUEST_MEMORY_LIMIT: usize = 512 * 1024 * 1024;
/// Maximum native call-stack depth the interpreter may use. `QuickJS` checks
/// this at function-entry granularity, so a deeply recursive guest aborts
/// with a stack-overflow exception rather than segfaulting the host.
const GUEST_MAX_STACK: usize = 1024 * 1024;

/// Interrupt-reason state shared between the host and the `QuickJS` interrupt
/// handler. The handler — called periodically by the interpreter — returns
/// `true` (abort) when this is non-zero, breaking out of synchronous tight
/// loops that the cooperative tokio timeout cannot preempt.
const INTR_RUNNING: u8 = 0;
const INTR_TIMEOUT: u8 = 1;
const INTR_CANCELLED: u8 = 2;

/// Gap between interrupt-handler calls that distinguishes continuous
/// interpreter execution (microseconds) from a suspended `await` (>= a
/// tool round-trip). Gaps below this are accumulated as CPU time; gaps at
/// or above it reset the accumulator, so long-running awaited tools don't
/// count toward the CPU budget.
const SUSPEND_THRESHOLD: Duration = Duration::from_millis(1);

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
/// Optional `lofi.recall` implementation: a sync callback from the
/// `lofi.recall` native tool into the engine that owns the session
/// transcript. The callback reads the session file fresh and runs the
/// recall engine, so the sandbox never depends on `lofi-core`.
pub type RecallFn =
    Arc<dyn Fn(&lofi_types::recall::RecallRequest) -> lofi_types::recall::RecallOutcome + Send + Sync>;

/// Optional `lofi.result` implementation: a sync callback that recovers the
/// original, pre-elision content of one message event by id. The inverse of
/// compaction's tiered-retention elision — the model re-expands a stubbed
/// tool result or tool-call by passing the event id the stub names. Reads
/// the session file fresh, like `RecallFn`.
pub type ResultFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

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
    /// agent-produced artifacts. `lofi.bash_read` is rooted here.
    pub tmp_dir: PathBuf,
    /// Named strings exposed as the global `lofi_strings` object.
    pub strings: HashMap<String, String>,
    /// Optional `lofi.agent` / `lofi.spawn` implementation.
    pub agent: Option<AgentFn>,
    /// Optional `lofi.recall` implementation (session-history search).
    pub recall: Option<RecallFn>,
    /// Optional `lofi.result` implementation (elision recovery).
    pub result: Option<ResultFn>,
    pub on_tool_event: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    /// Resolved `bash` child-env policy + output-redaction set.
    pub bash_env: BashEnv,
    /// Optional skills directory (`<config_dir>/skills`). When set,
    /// `lofi.skills()` / `lofi.skill(name)` discover and read markdown
    /// skill files from here and from `<root>/.lofi/skills/`.
    pub skills_dir: Option<PathBuf>,
}

impl std::fmt::Debug for ExecCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("root", &self.root)
            .field("tmp_dir", &self.tmp_dir)
            .field("strings", &self.strings)
            .field("agent", &self.agent.is_some())
            .field("recall", &self.recall.is_some())
            .field("result", &self.result.is_some())
            .field("on_tool_event", &self.on_tool_event.is_some())
            .field("bash_env", &self.bash_env)
            .field("skills_dir", &self.skills_dir)
            .finish()
    }
}

/// Per-call options.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Maximum *CPU time* (not wall-clock) for synchronous guest code.
    /// Awaited tool calls (`lofi.bash`, `lofi.agent`, …) do not consume
    /// this budget — only pure JS computation does. See [`exec`](fn.exec.html).
    pub timeout: Duration,
    /// Optional external cancellation flag. When set to `true`, the `QuickJS`
    /// interrupt handler breaks out of any running synchronous guest code and
    /// [`exec`](fn.exec.html) returns [`Error::Sandbox`] with a
    /// "cancelled" message. Without this, a tight `while (true) {}` loop
    /// blocks inside native `ctx.eval` and cannot be preempted by tokio's
    /// task-abort mechanism — the interrupt handler is the only way in.
    pub cancel: Option<Arc<AtomicBool>>,
}

impl Default for ExecOptions {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_GUEST_TIMEOUT,
            cancel: None,
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

/// Render a span as a user-relative `line:col` (1-based). The async-IIFE
/// wrapper prepends one line before the user's source, so the user's first
/// line is wrapped-line 2; subtract one to report user-relative lines so a
/// malformed escape points where the user wrote it.
fn span_loc(cm: &SourceMap, span: Span) -> String {
    let loc = cm.lookup_char_pos(span.lo());
    let line = loc.line.saturating_sub(1).max(1);
    let col = loc.col.0 + 1;
    format!("{line}:{col}")
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
        let mut module: Module = parser.parse_module().map_err(|e| {
            Error::Sandbox(format!(
                "parse error: {} at {}",
                e.kind().msg(),
                span_loc(&cm, e.span())
            ))
        })?;
        if let Some(e) = parser.take_errors().into_iter().next() {
            // Non-fatal recovered errors: report the first as a sandbox error
            // so typos don't silently produce wrong code. Include the
            // user-relative source location so a malformed escape or token
            // points at its line and column.
            return Err(Error::Sandbox(format!(
                "parse error: {} at {}",
                e.kind().msg(),
                span_loc(&cm, e.span())
            )));
        }
        module.visit_mut_with(&mut strip_type());
        let js = to_code_default(cm, None, &module);
        Ok(js)
    })?;

    Ok(code)
}

/// Compile and run `src` in a fresh `QuickJS` runtime.
///
/// Execution is bounded on three fronts:
///
/// - **Memory & stack** — `JS_SetMemoryLimit` and `JS_SetMaxStackSize` cap
///   the guest heap and call depth so a runaway allocation or deep recursion
///   aborts with a JS exception instead of exhausting the host.
///
/// - **CPU budget** — the `QuickJS` interrupt handler (installed via
///   `set_interrupt_handler`) is called on every interpreter tick. It
///   measures *CPU time*, not wall-clock time: gaps between handler calls
///   that are shorter than [`SUSPEND_THRESHOLD`] (microseconds during
///   continuous execution) are accumulated; larger gaps (the interpreter
///   was suspended inside an `await`ed tool) reset the accumulator. When
///   the accumulated CPU time exceeds `opts.timeout` the handler returns
///   *abort*, breaking out of synchronous tight loops (`while (true) {}`)
///   that the cooperative `tokio` runtime cannot preempt.
///
///   This means a long-running awaited tool — a subagent that takes hours,
///   a slow `bash` command — does *not* consume the budget. Only pure
///   synchronous guest code does. Individual tools carry their own
///   timeouts; the exec-level CPU budget is a backstop for loops, not a
///   wall-clock deadline on the whole call.
///
/// - **Cancellation** — if `opts.cancel` is provided, setting it to `true`
///   causes the same interrupt path and surfaces a distinct "cancelled"
///   error. This is how the UI's Ctrl+C breaks a sync loop that `handle
///   .abort()` alone cannot preempt.
///
/// `print` output and the returned value are always bounded (see
/// [`MAX_LOG_BYTES`] and [`js_to_json`]).
///
/// # Errors
/// Returns [`Error::Sandbox`] for compile failures, CPU-budget exhaustion,
/// cancellation, guest exceptions, or tool errors that propagate as thrown
/// exceptions.
pub async fn exec(src: &str, ctx: &ExecCtx, opts: &ExecOptions) -> Result<ExecResult> {
    let js = compile_ts(src)?;

    let rt = AsyncRuntime::new().map_err(|e| Error::Sandbox(format!("runtime: {e}")))?;
    let actx = AsyncContext::full(&rt)
        .await
        .map_err(|e| Error::Sandbox(format!("context: {e}")))?;

    // Bound guest memory and stack so a runaway allocation or deep recursion
    // throws a catchable JS exception instead of exhausting the host.
    rt.set_memory_limit(GUEST_MEMORY_LIMIT).await;
    rt.set_max_stack_size(GUEST_MAX_STACK).await;

    // CPU-budget interrupt handler. QuickJS calls the handler every ~256
    // bytecode instructions. During continuous execution the gap between
    // calls is microseconds; when the guest `await`s a tool the interpreter
    // suspends and the gap is the tool's duration (milliseconds to hours).
    // We accumulate only the sub-threshold gaps as CPU time and reset on
    // larger ones, so long-running awaited tools don't count toward the
    // budget. This is the only mechanism that can break a synchronous tight
    // loop blocking inside native `ctx.eval` — tokio's task abort cannot
    // preempt it because the loop never reaches an `.await` point.
    let intr = Arc::new(AtomicU8::new(INTR_RUNNING));
    let cpu_budget = opts.timeout;
    let cancel = opts.cancel.clone();
    {
        let intr = intr.clone();
        let mut last_tick = Instant::now();
        let mut cpu_accumulated = Duration::ZERO;
        rt.set_interrupt_handler(Some(Box::new(move || {
            if intr.load(Ordering::Relaxed) != INTR_RUNNING {
                return true;
            }
            if let Some(c) = &cancel {
                if c.load(Ordering::Relaxed) {
                    intr.store(INTR_CANCELLED, Ordering::Relaxed);
                    return true;
                }
            }
            let now = Instant::now();
            let delta = now.saturating_duration_since(last_tick);
            last_tick = now;
            if delta < SUSPEND_THRESHOLD {
                cpu_accumulated += delta;
            } else {
                cpu_accumulated = Duration::ZERO;
            }
            if cpu_accumulated >= cpu_budget {
                intr.store(INTR_TIMEOUT, Ordering::Relaxed);
                return true;
            }
            false
        })))
        .await;
    }

    let tools = Arc::new(BuiltinTools::with_tool_cb(
        ctx.root.clone(),
        ctx.on_tool_event.clone(),
        ctx.tmp_dir.clone(),
        ctx.bash_env.clone(),
    ));
    let strings = ctx.strings.clone();
    let agent = ctx.agent.clone();
    let recall = ctx.recall.clone();
    let result = ctx.result.clone();
    let skills_dir = ctx.skills_dir.clone();
    let logs = Arc::new(Mutex::new(String::new()));

    let outcome = async {
        async_with!(&actx => |ctx| {
            install_globals(&ctx, &tools, &strings, agent, recall, result, skills_dir, &logs)
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
    }
    .await;

    let reason = intr.load(Ordering::Relaxed);
    match outcome {
        Ok(r) => Ok(r),
        Err(e) => match reason {
            INTR_TIMEOUT => Err(Error::Sandbox(format!(
                "guest CPU budget exceeded ({}ms)",
                opts.timeout.as_millis()
            ))),
            INTR_CANCELLED => Err(Error::Sandbox("guest cancelled".to_string())),
            _ => Err(e),
        },
    }
}

/// Install the `lofi` object, `print`, and the `lofi_strings` global.
#[allow(clippy::too_many_arguments)]
fn install_globals(
    ctx: &Ctx<'_>,
    tools: &Arc<BuiltinTools>,
    strings: &HashMap<String, String>,
    agent: Option<AgentFn>,
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    skills_dir: Option<PathBuf>,
    logs: &Arc<Mutex<String>>,
) -> rquickjs::Result<()> {
    let lofi = Object::new(ctx.clone())?;
    bind_tools(ctx, &lofi, tools, agent, recall, result, skills_dir)?;
    // Expose the per-session tmp dir path so the model knows where bash
    // full-output logs live (and can reference them if needed beyond
    // `lofi.bash_read`, which takes a basename relative to this dir).
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

/// First line of `s`, truncated to `cap` visible chars with an ellipsis.
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

/// Stringify a native tool's structured return for the UI. Plain strings
/// (e.g. `read` content) pass through; structured objects serialize to JSON.
/// The renderer interprets the result per tool — this never extracts fields.
fn tool_preview(res: &std::result::Result<Json, Error>) -> (String, bool) {
    match res {
        Ok(v) => match v {
            Json::String(s) => (s.clone(), false),
            other => (other.to_string(), false),
        },
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
            recall: None,
            result: None,
            bash_env: BashEnv::default(),
            skills_dir: None,
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
        assert_eq!(res.value["content"], json!("hello"));
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
            recall: None,
            result: None,
            bash_env: BashEnv::default(),
            skills_dir: None,
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

    #[test]
    fn compile_ts_malformed_escape_reports_location() {
        // `\u{GG}` is not valid hex; the error must carry the user-relative
        // source location (line 1) so a malformed escape points where it is.
        let err = compile_ts(r#"return "\u{GG}";"#).unwrap_err();
        let msg = match err {
            Error::Sandbox(m) => m,
            other => panic!("expected Sandbox error, got {other:?}"),
        };
        assert!(msg.starts_with("parse error"), "not a parse error: {msg}");
        assert!(msg.contains("at 1:"), "missing user-relative location: {msg}");
    }

    #[tokio::test]
    async fn exec_unicode_escapes_round_trip() {
        // `\uXXXX`, `\u{...}` (non-BMP), and a UTF-16 surrogate pair all
        // decode to the same code point and survive parse → codegen → eval.
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
        // An escape inside a tool argument string is decoded before the arg
        // reaches the tool, so `echo \u00E9` echoes é.
        let dir = tempdir().unwrap();
        let src = r#"const r = await lofi.bash({ cmd: "echo \u00E9" }); return r.output.trim();"#;
        let res = exec(src, &ctx(dir.path()), &ExecOptions::default())
            .await
            .unwrap();
        assert_eq!(res.value, json!("é"));
    }

    // ── Interrupt handler: deadlines & cancellation ──

    #[tokio::test]
    async fn exec_sync_infinite_loop_times_out() {
        // `while (true) {}` blocks inside native `ctx.eval` and never reaches
        // an `.await` — the interrupt handler is the only thing that can
        // break it. The CPU budget accumulates and fires well under 1s.
        let dir = tempdir().unwrap();
        let opts = ExecOptions {
            timeout: Duration::from_millis(500),
            cancel: None,
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
        // A CPU-bound loop that does real work each iteration must also be
        // interrupted — the handler fires on interpreter ticks, not just
        // idle loops.
        let dir = tempdir().unwrap();
        let opts = ExecOptions {
            timeout: Duration::from_millis(500),
            cancel: None,
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
        // Setting the external cancel flag must interrupt a synchronous tight
        // loop and surface a distinct "cancelled" error (not "timeout").
        //
        // The guest loop blocks inside native `ctx.eval` on this thread, so
        // the cancel flag must be set from a *separate* spawned task; the
        // QuickJS interrupt handler then sees it on the next interpreter tick
        // and aborts. A single-threaded runtime would deadlock here.
        let dir = tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let opts = ExecOptions {
            timeout: Duration::from_secs(30),
            cancel: Some(cancel.clone()),
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
        // After a cancelled exec, a fresh exec on a new runtime must work —
        // the interrupt did not leave the host in a bad state.
        let dir = tempdir().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let exec_ctx = ctx(dir.path());
        let opts = ExecOptions {
            timeout: Duration::from_secs(30),
            cancel: Some(cancel.clone()),
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
    async fn exec_memory_limit_aborts_unbounded_allocation() {
        // Growing an array without bound must hit the memory limit and throw
        // a JS exception (surfaced as Sandbox) rather than exhausting the host.
        let dir = tempdir().unwrap();
        let opts = ExecOptions {
            timeout: Duration::from_secs(5),
            cancel: None,
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
    async fn exec_deep_recursion_hits_stack_limit() {
        // Unbounded recursion must hit the stack-size limit and throw rather
        // than segfaulting the host.
        let dir = tempdir().unwrap();
        let opts = ExecOptions {
            timeout: Duration::from_secs(5),
            cancel: None,
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
        // A slow `bash` call (sleep 1s) repeated many times must not accumulate
        // CPU budget — the interpreter is suspended during each `await`, so
        // the gap resets the accumulator. With a 500ms CPU budget and 3 rounds
        // of 1s sleep (3s wall-clock), this would fail if the budget were
        // wall-clock but succeeds because only JS computation counts.
        let dir = tempdir().unwrap();
        let opts = ExecOptions {
            timeout: Duration::from_millis(500),
            cancel: None,
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
            const res = await lofi.docs_search("write file");
            const topName = res.results[0].name;
            return topName;
        "#;
        let res = exec(src, &ctx(dir.path()), &opts).await.unwrap();
        assert_eq!(res.value, json!("lofi.write"));
    }
}