//! Tool closures return an owned `ToolOutput`. Its `IntoJs` implementation
//! recursively converts successful JSON values and throws a genuine JavaScript
//! `Error` for failures. This avoids rquickjs's generic Rust-conversion error
//! wording while retaining the leak-free owned-value bridge. The IIFE wrapper
//! catches uncaught tool errors and resolves them as a structured
//! `{ __lofi_sandbox_error__ }` result; resolving avoids rquickjs 0.9 retaining
//! a rejected promise's value through runtime shutdown.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::async_with;
use rquickjs::prelude::*;
use rquickjs::{
    Array, AsyncContext, AsyncRuntime, Ctx, Exception, Function, IntoJs, Object, Promise, Value,
};
use serde_json::{json, Value as Json};
use swc_common::sync::Lrc;
use swc_common::{FileName, FilePathMapping, Globals, SourceMap, Span, Spanned, GLOBALS};
use swc_ecma_ast::Module;
use swc_ecma_codegen::to_code_default;
use swc_ecma_parser::{Parser, StringInput, Syntax, TsSyntax};
use swc_ecma_transforms_typescript::strip_type;
use swc_ecma_visit::VisitMutWith;

use lofi_error::{Error, Result};

pub mod compact_hook;
mod convert;
pub mod docs;
pub mod policy;
use convert::{js_to_json, json_to_js};

mod bind;
use bind::bind_tools;

pub mod tools;

use crate::tools::BuiltinTools;
pub use tools::BashEnv;

pub const EXEC_TOOL_NAME: &str = "exec";

pub const EXEC_TOOL_DESCRIPTION: &str = "Compile and run a TypeScript program in a sandboxed QuickJS runtime. The program has access to a `lofi` object with file/shell/search tools (read, ls, find, grep, write, edit, patch, bash). Top-level await and return are supported. The returned value is sent back as the tool result; keep it compact and final.";

#[must_use]
pub fn exec_tool_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "code": {
                "type": "string",
                "description": "TypeScript source. Top-level await/return supported."
            },
            "strings": {
                "type": "object",
                "description": "Named string constants exposed as the global `lofi_strings` object."
            },
            "display": {
                "type": "object",
                "description": "Optional display metadata; ignored by the runtime."
            }
        },
        "required": ["code"]
    })
}

pub const DEFAULT_GUEST_TIMEOUT: Duration = Duration::from_mins(2);

const GUEST_MEMORY_LIMIT: usize = 32 * 1024 * 1024;
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

const SUSPEND_THRESHOLD: Duration = Duration::from_millis(1);

/// Sentinel object key used by the IIFE wrapper to surface an uncaught guest
/// exception as a resolved (not rejected) promise.
const SANDBOX_ERROR_KEY: &str = "__lofi_sandbox_error__";

const MAX_LOG_BYTES: usize = 1024 * 1024;
const JS_TO_JSON_MAX_DEPTH: usize = 64;
const JS_TO_JSON_MAX_NODES: usize = 10_000;
/// Maximum cumulative bytes of converted strings and object keys, so a single
/// huge guest string cannot exhaust host memory despite staying within the
/// node budget.
const JS_TO_JSON_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Optional `lofi.recall` implementation: a sync callback from the
/// `lofi.recall` native tool into the engine that owns the session
/// transcript. The callback reads the session file fresh and runs the
/// recall engine, so the sandbox never depends on `lofi-core`.
pub type RecallFn = Arc<
    dyn Fn(&lofi_types::recall::RecallRequest) -> lofi_types::recall::RecallOutcome + Send + Sync,
>;

pub type ResultFn = Arc<dyn Fn(&str) -> String + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmReason {
    Policy,
    AutoEvaluating { started_at: std::time::Instant },
    AutoAsk { reason: String },
    AutoFailed { reason: String },
}

#[derive(Debug, Clone)]
pub struct ConfirmPrompt {
    pub command: String,
    pub reason: Arc<std::sync::Mutex<ConfirmReason>>,
    /// True while this prompt can still accept a user response. Auto-mode
    /// clears it when an approval or override wins the race, allowing the UI
    /// to dismiss a stale request whose response future was cancelled.
    pub active: Arc<std::sync::atomic::AtomicBool>,
}

pub type ConfirmFn = Arc<
    dyn Fn(
            ConfirmPrompt,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + Sync>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoModeOutcome {
    Allow { reason: String },
    Ask { reason: String },
    Failed { reason: String },
}

pub type AutoModeFn = Arc<
    dyn Fn(
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone)]
pub enum ToolEvent {
    Start {
        id: u64,
        name: String,
        args: String,
    },
    End {
        id: u64,
        result: String,
        is_error: bool,
    },
}

#[derive(Clone)]
pub struct ExecCtx {
    pub root: PathBuf,
    pub tmp_dir: PathBuf,
    pub strings: HashMap<String, String>,
    pub recall: Option<RecallFn>,
    pub result: Option<ResultFn>,
    pub on_tool_event: Option<Arc<dyn Fn(ToolEvent) + Send + Sync>>,
    pub bash_env: BashEnv,
    pub shell_policy: crate::policy::ResolvedPolicy,
    pub confirm: Option<ConfirmFn>,
    pub auto_mode: Option<AutoModeFn>,
    pub skills_dir: Option<PathBuf>,
}

impl std::fmt::Debug for ExecCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecCtx")
            .field("root", &self.root)
            .field("tmp_dir", &self.tmp_dir)
            .field("strings", &self.strings)
            .field("recall", &self.recall.is_some())
            .field("result", &self.result.is_some())
            .field("on_tool_event", &self.on_tool_event.is_some())
            .field("bash_env", &self.bash_env)
            .field("shell_policy", &"<resolved>")
            .field("confirm", &self.confirm.is_some())
            .field("auto_mode", &self.auto_mode.is_some())
            .field("skills_dir", &self.skills_dir)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct ExecOptions {
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

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub value: Json,
    pub logs: String,
}

struct JsonV(Json);

impl<'js> IntoJs<'js> for JsonV {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        json_to_js(ctx, &self.0)
    }
}

enum ToolOutput {
    Value(Json),
    Error(String),
}

impl<'js> IntoJs<'js> for ToolOutput {
    fn into_js(self, ctx: &Ctx<'js>) -> rquickjs::Result<Value<'js>> {
        match self {
            Self::Value(value) => json_to_js(ctx, &value),
            Self::Error(message) => Err(Exception::throw_message(ctx, &message)),
        }
    }
}

fn span_loc(cm: &SourceMap, span: Span) -> String {
    let loc = cm.lookup_char_pos(span.lo());
    let line = loc.line.saturating_sub(1).max(1);
    let col = loc.col.0 + 1;
    format!("{line}:{col}")
}

/// # Errors
/// Returns a sandbox error when TypeScript parsing or transformation fails.
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

/// - **Memory & stack** — `JS_SetMemoryLimit` and `JS_SetMaxStackSize` cap
///   the guest heap and call depth so a runaway allocation or deep recursion
///   aborts with a JS exception instead of exhausting the host.
/// - **CPU budget** — the `QuickJS` interrupt handler (installed via
///   `set_interrupt_handler`) is called on every interpreter tick. It
///   measures *CPU time*, not wall-clock time: gaps between handler calls
///   that are shorter than [`SUSPEND_THRESHOLD`] (microseconds during
///   continuous execution) are accumulated; larger gaps (the interpreter
///   was suspended inside an `await`ed tool) reset the accumulator. When
///   the accumulated CPU time exceeds `opts.timeout` the handler returns
///   *abort*, breaking out of synchronous tight loops (`while (true) {}`)
///   that the cooperative `tokio` runtime cannot preempt.
/// - **Cancellation** — if `opts.cancel` is provided, setting it to `true`
///   causes the same interrupt path and surfaces a distinct "cancelled"
///   error. This is how the UI's Ctrl+C breaks a sync loop that `handle
///   .abort()` alone cannot preempt.
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

    let tools = Arc::new(
        BuiltinTools::with_skills_dir(
            ctx.root.clone(),
            ctx.on_tool_event.clone(),
            ctx.tmp_dir.clone(),
            ctx.bash_env.clone(),
            ctx.shell_policy.clone(),
            ctx.confirm.clone(),
            ctx.auto_mode.clone(),
            ctx.skills_dir.clone(),
        )
        .with_cancel(opts.cancel.clone()),
    );
    let strings = ctx.strings.clone();
    let recall = ctx.recall.clone();
    let result = ctx.result.clone();
    let skills_dir = ctx.skills_dir.clone();
    let logs = Arc::new(Mutex::new(String::new()));

    let outcome = async {
        async_with!(&actx => |ctx| {
            // QuickJS can fault while building an error backtrace after its
            // heap limit is exhausted. The harness never exposes guest stacks.
            ctx.eval::<(), _>(
                r#"Error.stackTraceLimit = 0;
                Object.defineProperty(Error, "stackTraceLimit", {
                    value: 0, writable: false, configurable: false
                }); void 0;"#,
            )
            .map_err(|e| Error::Sandbox(format!("context: {e}")))?;
            install_globals(&ctx, &tools, &strings, recall, result, skills_dir, &logs)
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

#[allow(clippy::too_many_arguments)]
fn install_globals(
    ctx: &Ctx<'_>,
    tools: &Arc<BuiltinTools>,
    strings: &HashMap<String, String>,
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    skills_dir: Option<PathBuf>,
    logs: &Arc<Mutex<String>>,
) -> rquickjs::Result<()> {
    let lofi = Object::new(ctx.clone())?;
    bind_tools(ctx, &lofi, tools, recall, result, skills_dir)?;
    lofi.set("tmp_dir", tools.tmp_dir().to_string_lossy().to_string())?;
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

fn parse_read_opts(opts: Opt<Value>) -> (Option<u64>, Option<u64>) {
    let Some(v) = opts.0 else {
        return (None, None);
    };
    let json = js_to_json(&v);
    let Some(obj) = json.as_object() else {
        return (None, None);
    };
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

fn native_args_label(name: &str, v: &serde_json::Value) -> String {
    let pick = |key: &str| {
        v.get(key)
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string)
    };
    let raw = match name {
        "write" | "edit" | "patch" => pick("path"),
        "bash" => pick("cmd"),
        _ => None,
    };
    let s = raw.unwrap_or_else(|| match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    });
    cap_first_line(&s, 120)
}

fn tool_preview(res: &std::result::Result<Json, Error>) -> (String, bool) {
    match res {
        Ok(v) => {
            let is_error = v
                .get("ok")
                .and_then(serde_json::Value::as_bool)
                .is_some_and(|ok| !ok);
            match v {
                Json::String(s) => (s.clone(), is_error),
                other => (other.to_string(), is_error),
            }
        }
        Err(e) => (e.to_string(), true),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

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
            shell_policy: crate::policy::defaults::resolve(
                &lofi_types::ShellPolicyConfig::default(),
            ),
            confirm: None,
            auto_mode: None,
            skills_dir: None,
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
            on_tool_event: None,
            recall: None,
            result: None,
            bash_env: BashEnv::default(),
            shell_policy: crate::policy::defaults::resolve(
                &lofi_types::ShellPolicyConfig::default(),
            ),
            confirm: None,
            auto_mode: None,
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
            const res = await lofi.docsSearch("write file");
            const topName = res.results[0].name;
            return topName;
        "#;
        let res = exec(src, &ctx(dir.path()), &opts).await.unwrap();
        assert_eq!(res.value, json!("lofi.write"));
    }
}
