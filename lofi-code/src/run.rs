use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::async_with;
use rquickjs::prelude::*;
use rquickjs::{AsyncContext, AsyncRuntime, Ctx, Function, Object, Promise, Value};
use serde_json::Value as Json;

use lofi_error::{Error, Result};

use crate::bind::bind_tools;
use crate::compile::compile_ts;
use crate::convert::{js_to_json_with_limit, SANDBOX_ERROR_KEY};
use crate::ctx::{
    ExecCtx, ExecImage, ExecOptions, ExecResult, ModelsFn, RecallFn, ResultFn, SubagentFn,
};
use crate::tools::BuiltinTools;

const GUEST_MEMORY_LIMIT: usize = 8 * 1024 * 1024;
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

pub(crate) const MAX_LOG_BYTES: usize = 1024 * 1024;
const LOG_TRUNCATION_MARKER: &str = "\n<logs truncated>";

const TRACK_FOREGROUND_TOOLS: &str = r#"
    lofi => {
        const pending = new Set();
        const errors = [];
        const errorText = error =>
            String(error && error.message ? error.message : error);

        // Await-heavy loops (while-true around an await) barely tick the
        // QuickJS interrupt handler, so the CPU budget cannot bound them.
        // Every await on a host tool funnels through this wrapper, so cap
        // the number of tool calls per exec instead. 4096 is far above any
        // legitimate script (paging, batch edits) and trips in about a
        // second for a tight loop.
        const MAX_CALLS = 4096;
        let calls = 0;

        for (const name of Object.keys(lofi)) {
            const tool = lofi[name];
            if (typeof tool !== "function") continue;
            lofi[name] = (...args) => {
                calls += 1;
                if (calls > MAX_CALLS) {
                    throw new Error(
                        "tool call budget exceeded (" + MAX_CALLS + " calls per exec)"
                    );
                }
                const promise = Promise.resolve().then(() => tool(...args));
                const tracked = promise.then(
                    value => {
                        if (value !== null && typeof value === "object" && value.ok === false) {
                            errors.push({ tool: name, result: value });
                        }
                        return value;
                    },
                    error => {
                        errors.push({ tool: name, error: errorText(error) });
                        throw error;
                    }
                );
                pending.add(tracked);
                tracked.then(
                    () => pending.delete(tracked),
                    () => pending.delete(tracked)
                );
                return tracked;
            };
        }

        return async () => {
            while (pending.size > 0) {
                await Promise.allSettled(Array.from(pending));
            }
            return errors;
        };
    }
"#;

// QuickJS runs the interrupt handler during bytecode execution. Awaited host
// work creates long gaps between ticks, so each gap is capped before it is
// charged. The accumulated budget is preserved across suspensions because
// resetting it would let repeated CPU bursts evade the limit.
async fn install_cpu_guard(
    rt: &AsyncRuntime,
    budget: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Arc<AtomicU8> {
    let intr = Arc::new(AtomicU8::new(INTR_RUNNING));
    let intr_clone = intr.clone();
    let mut last_tick = Instant::now();
    let mut cpu_accumulated = Duration::ZERO;
    rt.set_interrupt_handler(Some(Box::new(move || {
        if intr_clone.load(Ordering::Relaxed) != INTR_RUNNING {
            return true;
        }
        if let Some(c) = &cancel {
            if c.load(Ordering::Relaxed) {
                intr_clone.store(INTR_CANCELLED, Ordering::Relaxed);
                return true;
            }
        }
        let now = Instant::now();
        let delta = now.saturating_duration_since(last_tick);
        last_tick = now;
        cpu_accumulated += delta.min(SUSPEND_THRESHOLD);
        if cpu_accumulated >= budget {
            intr_clone.store(INTR_TIMEOUT, Ordering::Relaxed);
            return true;
        }
        false
    })))
    .await;
    intr
}

/// - **Memory & stack** — `JS_SetMemoryLimit` and `JS_SetMaxStackSize` cap
///   the guest heap and call depth so a runaway allocation or deep recursion
///   aborts with a JS exception instead of exhausting the host.
/// - **CPU budget** — the `QuickJS` interrupt handler (installed via
///   `set_interrupt_handler`) is called on every interpreter tick. It
///   approximates CPU time rather than wall-clock time by capping gaps between
///   handler calls at [`SUSPEND_THRESHOLD`]. When the accumulated time exceeds
///   `opts.timeout` the handler returns
///   *abort*, breaking out of synchronous tight loops (`while (true) {}`)
///   that the cooperative `tokio` runtime cannot preempt. Ticks are rare
///   during awaited host work, so await-heavy loops accumulate budget
///   slowly; the tool-call cap in [`TRACK_FOREGROUND_TOOLS`] bounds those
///   instead.
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

    let intr = install_cpu_guard(&rt, opts.timeout, opts.cancel.clone()).await;

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
        .with_policy_override(ctx.policy_override.clone())
        .with_cancel(opts.cancel.clone())
        .with_truncate(ctx.truncate)
        .with_jobs(ctx.jobs.clone())
        .with_job_owner(ctx.job_owner.clone())
        .with_on_job_acquired(ctx.on_job_acquired.clone()),
    );
    let strings = ctx.strings.clone();
    let recall = ctx.recall.clone();
    let result = ctx.result.clone();
    let skills_dir = ctx.skills_dir.clone();
    let subagent = ctx.subagent.clone();
    let models = ctx.models.clone();
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
            let drain_tools = install_globals(
                &ctx,
                &tools,
                &strings,
                recall,
                result,
                skills_dir,
                subagent.as_ref(),
                models,
                &logs,
                opts.max_value_bytes,
            )
            .map_err(|e| Error::Sandbox(format!("install: {e}")))?;
            let promise: Promise = ctx
                .eval(js.as_str())
                .map_err(|e| Error::Sandbox(format!("eval: {e}")))?;
            let value: Value = promise
                .into_future()
                .await
                .map_err(|e| Error::Sandbox(format!("guest promise rejected: {e}")))?;
            let errors_promise: Promise = drain_tools
                .call(())
                .map_err(|e| Error::Sandbox(format!("drain tools: {e}")))?;
            let errors: Value = errors_promise
                .into_future()
                .await
                .map_err(|e| Error::Sandbox(format!("drain tools: {e}")))?;
            convert_exec_result(&value, &errors, &logs, &tools, opts.max_value_bytes)
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

fn convert_exec_result(
    value: &Value<'_>,
    errors: &Value<'_>,
    logs: &Arc<Mutex<String>>,
    tools: &BuiltinTools,
    max_value_bytes: usize,
) -> Result<ExecResult> {
    let has_value = !value.is_undefined();
    let (errors_json, errors_truncated) = js_to_json_with_limit(errors, max_value_bytes);
    if let Some(error) = errors_json.get(SANDBOX_ERROR_KEY).and_then(Json::as_str) {
        return Err(Error::Sandbox(format!("nested tool errors {error}")));
    }
    let errors = errors_json.as_array().cloned().unwrap_or_default();
    let errors_bytes = serde_json::to_string(&errors).map_or(0, |json| json.len());
    let logs_budget = max_value_bytes.saturating_sub(errors_bytes);
    let mut logs = logs
        .lock()
        .ok()
        .map(|mut logs| std::mem::take(&mut *logs))
        .unwrap_or_default();
    let logs_truncated = truncate_logs(&mut logs, logs_budget);
    let value_budget = max_value_bytes
        .saturating_sub(errors_bytes)
        .saturating_sub(logs.len());
    let (mut value, value_truncated) = js_to_json_with_limit(value, value_budget);
    if let Some(error) = value.get(SANDBOX_ERROR_KEY).and_then(Json::as_str) {
        return Err(Error::Sandbox(error.to_string()));
    }
    let image = take_pending_image(&mut value, tools);
    Ok(ExecResult {
        value,
        has_value,
        errors,
        logs,
        value_truncated: value_truncated || errors_truncated || logs_truncated,
        image,
    })
}

fn truncate_logs(logs: &mut String, max_bytes: usize) -> bool {
    if logs.len() <= max_bytes {
        return false;
    }
    let marker_end =
        LOG_TRUNCATION_MARKER.floor_char_boundary(LOG_TRUNCATION_MARKER.len().min(max_bytes));
    let marker = &LOG_TRUNCATION_MARKER[..marker_end];
    let content_end = logs.floor_char_boundary(max_bytes.saturating_sub(marker.len()));
    logs.truncate(content_end);
    logs.push_str(marker);
    true
}

fn take_pending_image(value: &mut Json, tools: &BuiltinTools) -> Option<ExecImage> {
    let object = value.as_object_mut()?;
    if object.get("type").and_then(Json::as_str) != Some("image") {
        return None;
    }
    let token = object.remove("image_token")?.as_u64()?;
    let pending = tools.take_image(token)?;
    Some(ExecImage {
        file: pending.file,
        bytes: pending.bytes,
    })
}

#[allow(clippy::too_many_arguments)]
fn install_globals<'js>(
    ctx: &Ctx<'js>,
    tools: &Arc<BuiltinTools>,
    strings: &HashMap<String, String>,
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    skills_dir: Option<PathBuf>,
    subagent: Option<&SubagentFn>,
    models: Option<ModelsFn>,
    logs: &Arc<Mutex<String>>,
    max_output_bytes: usize,
) -> rquickjs::Result<Function<'js>> {
    let lofi = Object::new(ctx.clone())?;
    bind_tools(
        ctx, &lofi, tools, recall, result, skills_dir, subagent, models,
    )?;
    lofi.set("tmpDir", tools.tmp_dir().to_string_lossy().to_string())?;
    let install_tracker: Function = ctx.eval(TRACK_FOREGROUND_TOOLS)?;
    let drain_tools = install_tracker.call((lofi.clone(),))?;
    ctx.globals().set("lofi", lofi)?;

    let max_log_bytes = MAX_LOG_BYTES.min(max_output_bytes);
    let print_sink = Function::new(ctx.clone(), {
        let logs = logs.clone();
        move |args: Rest<Coerced<std::string::String>>| -> rquickjs::Result<()> {
            let mut line = String::new();
            // Cap the assembled line at the log budget while building it, so a
            // single huge argument cannot allocate an unbounded `line` before
            // the post-hoc cap runs. (The per-arg `Coerced<String>` is owned
            // by the runtime; this prevents amplifying it via concatenation.)
            for (i, arg) in args.iter().enumerate() {
                if line.len() >= max_log_bytes {
                    break;
                }
                if i > 0 {
                    line.push(' ');
                }
                let room = max_log_bytes.saturating_sub(line.len());
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
                let marker_end = LOG_TRUNCATION_MARKER
                    .floor_char_boundary(LOG_TRUNCATION_MARKER.len().min(max_log_bytes));
                let marker = &LOG_TRUNCATION_MARKER[..marker_end];
                if marker.is_empty() || buf.ends_with(marker) {
                    return Ok(());
                }
                let cap = max_log_bytes.saturating_sub(marker.len());
                let room = cap.saturating_sub(buf.len());
                if room == 0 {
                    buf.push_str(marker);
                    return Ok(());
                }
                let take = line.len().min(room);
                let take = line.floor_char_boundary(take);
                buf.push_str(&line[..take]);
                if take < line.len() || buf.len() >= cap {
                    buf.push_str(marker);
                }
            }
            Ok(())
        }
    })?;
    ctx.globals().set("__lofi_print_sink", print_sink)?;
    ctx.eval::<(), _>(format!(
        r"globalThis.print = ((sink, max) => (...args) =>
            sink(...args.map(value => String(value).slice(0, max))))(
                globalThis.__lofi_print_sink, {max_log_bytes}
            );
            delete globalThis.__lofi_print_sink;"
    ))?;

    let strings_obj = Object::new(ctx.clone())?;
    for (k, v) in strings {
        strings_obj.set(k.as_str(), v.as_str())?;
    }
    ctx.globals().set("lofi_strings", strings_obj)?;
    Ok(drain_tools)
}
