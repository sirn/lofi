use std::path::PathBuf;
use std::sync::Arc;

use rquickjs::prelude::*;
use rquickjs::{Ctx, Exception, Function, Object, Value};
use serde_json::{json, Value as Json};

use lofi_error::Error;

use crate::convert::{js_to_json, json_to_js};
use crate::ctx::{ModelsFn, RecallFn, ResultFn, SubagentFn, SubagentOp, ToolEvent};
use crate::tools::BuiltinTools;

#[allow(clippy::too_many_arguments)]
pub(super) fn bind_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    skills_dir: Option<PathBuf>,
    subagent: Option<&SubagentFn>,
    models: Option<ModelsFn>,
) -> rquickjs::Result<()> {
    bind_file_tools(ctx, lofi, tools)?;
    bind_job_tools(ctx, lofi, tools)?;
    bind_recall_tool(ctx, lofi, recall)?;
    bind_result_tool(ctx, lofi, result)?;
    bind_skills_tools(ctx, lofi, tools, skills_dir)?;
    bind_docs_tools(ctx, lofi)?;
    bind_subagent_tool(ctx, lofi, tools, subagent)?;
    bind_models_tool(ctx, lofi, models)?;
    Ok(())
}

/// `lofi.models` lists the models available for `lofi.subagent` overrides.
/// The tool stays bound without a hook so a nested sandbox gets a clear
/// error instead of an undefined function.
fn bind_models_tool<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    models: Option<ModelsFn>,
) -> rquickjs::Result<()> {
    lofi.set(
        "models",
        Function::new(ctx.clone(), move || -> rquickjs::Result<JsonV> {
            match &models {
                Some(list) => Ok(JsonV(list())),
                None => Ok(JsonV(json!({
                    "ok": false,
                    "status": "unavailable",
                    "error": "lofi.models is not available in this context",
                }))),
            }
        }),
    )?;
    Ok(())
}

/// `lofi.subagent*` delegates work to child agent runs owned by the host
/// engine. The tools stay bound without a hook so a nested sandbox gets a
/// clear error instead of an undefined function. Only `subagent` (Run) arms
/// the hook's drop-cancellation; dropping a `subagentWait` future aborts
/// the wait but never the run.
fn bind_subagent_tool<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    subagent: Option<&SubagentFn>,
) -> rquickjs::Result<()> {
    for (name, op, cancellable) in [
        ("subagent", SubagentOp::Run, true),
        ("subagentSpawn", SubagentOp::Spawn, false),
        ("subagentStatus", SubagentOp::Status, false),
        ("subagentList", SubagentOp::List, false),
        ("subagentWait", SubagentOp::Wait, true),
        ("subagentRead", SubagentOp::Read, false),
        ("subagentKill", SubagentOp::Kill, false),
        ("subagentNotify", SubagentOp::Notify, false),
    ] {
        let t = tools.clone();
        // Clone the Arc out of the reference: the closures are 'js-lived
        // and cannot capture the caller's reference.
        let subagent = subagent.cloned();
        lofi.set(
            name,
            Function::new(
                ctx.clone(),
                Async(move |args: Opt<Value>| {
                    let t = t.clone();
                    let subagent = subagent.clone();
                    let args = args.0.map_or(Json::Null, |v| js_to_json(&v));
                    async move {
                        let id = t.next_tool_id();
                        let label = args
                            .get("prompt")
                            .or_else(|| args.get("id"))
                            .and_then(Json::as_str)
                            .unwrap_or("");
                        t.emit(ToolEvent::Start {
                            id,
                            name: name.into(),
                            args: cap_first_line(label, 120),
                        });
                        let res: std::result::Result<Json, Error> = match &subagent {
                            Some(hook) => {
                                // Race the hook against user cancellation:
                                // while the guest awaits the subagent, no
                                // QuickJS bytecode ticks, so the interrupt
                                // handler can't observe `cancel`. Dropping
                                // the Run hook future triggers the
                                // subagent's CancelOnDrop; the Wait hook
                                // holds no such guard.
                                match (cancellable, t.cancel_flag()) {
                                    (true, Some(flag)) => {
                                        let cancel_wait = crate::tools::wait_for_cancel(&flag);
                                        tokio::pin!(cancel_wait);
                                        tokio::select! {
                                            biased;
                                            out = hook(op, args) => Ok(out),
                                            () = &mut cancel_wait => Err(Error::Tool(
                                                "cancelled".to_string(),
                                            )),
                                        }
                                    }
                                    _ => Ok(hook(op, args).await),
                                }
                            }
                            // Resolve rather than throw, like the hook's own
                            // argument errors: guest code checks the `ok`
                            // field instead of mixing try/catch with status
                            // checks.
                            None => Ok(json!({
                                "ok": false,
                                "status": "unavailable",
                                "error": "subagent is not available in this context",
                            })),
                        };
                        let (result, is_error) = tool_preview(&res);
                        t.emit(ToolEvent::End {
                            id,
                            result,
                            is_error,
                        });
                        tool_result(res)
                    }
                }),
            )?,
        )?;
    }
    Ok(())
}

/// The async closures deliberately do **not** capture a `Ctx` clone: doing
/// so would create a `context -> globals -> lofi -> function -> Ctx -> context`
/// reference cycle that never collects and trips `QuickJS`'s `gc_obj_list`
/// assertion at runtime shutdown. `rquickjs` passes the call-site `Ctx` to
/// `JsonV::into_js` for us, so the closures only need to capture the `Arc`
/// tool bundle.
#[allow(clippy::too_many_lines)]
fn bind_file_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
) -> rquickjs::Result<()> {
    let t = tools.clone();
    lofi.set(
        "read",
        Function::new(
            ctx.clone(),
            Async(move |path: String, opts: Opt<Value>| {
                let t = t.clone();
                let (offset, limit) = parse_read_opts(opts);
                let args_label = if let Some(o) = offset {
                    format!("{path} +{o}")
                } else {
                    path.clone()
                };
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "read".into(),
                        args: cap_first_line(&args_label, 120),
                    });
                    let res = t.read(&path, offset, limit).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "ls",
        Function::new(
            ctx.clone(),
            Async(move |dir: Opt<String>| {
                let t = t.clone();
                let d = dir.0.as_deref().unwrap_or("").to_string();
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "ls".into(),
                        args: cap_first_line(&d, 120),
                    });
                    let res = t.ls(&d).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "find",
        Function::new(
            ctx.clone(),
            Async(move |glob: String, dir: Opt<String>, filtered: Opt<bool>| {
                let t = t.clone();
                let d = dir.0.as_deref().unwrap_or("").to_string();
                let filtered = filtered.0.unwrap_or(true);
                let args = if d.is_empty() {
                    glob.clone()
                } else {
                    format!("{glob} in {d}")
                };
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "find".into(),
                        args: cap_first_line(&args, 120),
                    });
                    let res = t.find(&glob, Some(d.as_str()), filtered).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "grep",
        Function::new(
            ctx.clone(),
            Async(move |pattern: Value, path: Opt<String>| {
                let t = t.clone();
                let p = js_to_json(&pattern);
                let path = path.0;
                let args = native_args_label("grep", &p);
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "grep".into(),
                        args,
                    });
                    let res = t.grep(p, path.as_deref()).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "write",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                let label = native_args_label("write", &args);
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "write".into(),
                        args: label,
                    });
                    let res = t.write(args).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "edit",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                let label = native_args_label("edit", &args);
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "edit".into(),
                        args: label,
                    });
                    let res = t.edit(args).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "patch",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                let label = native_args_label("patch", &args);
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "patch".into(),
                        args: label,
                    });
                    let res = t.patch(args).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t = tools.clone();
    lofi.set(
        "bash",
        Function::new(
            ctx.clone(),
            Async(move |args: Value| {
                let t = t.clone();
                let args = js_to_json(&args);
                let label = native_args_label("bash", &args);
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "bash".into(),
                        args: label,
                    });
                    let res = t.bash(args).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    Ok(())
}

/// Background job tools share the bash policy/confirmation path (via
/// `jobSpawn`'s `check_policy`) and emit the same Start/End tool events as
/// the file tools, so the UI tiles and transcript records stay uniform.
fn bind_job_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
) -> rquickjs::Result<()> {
    type JobMethod = for<'a> fn(
        &'a BuiltinTools,
        serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = lofi_error::Result<Json>> + Send + 'a>,
    >;
    const JOB_TOOLS: &[(&str, JobMethod)] = &[
        ("jobSpawn", |t, a| Box::pin(t.job_spawn(a))),
        ("jobStatus", |t, a| Box::pin(t.job_status(a))),
        ("jobRead", |t, a| Box::pin(t.job_read(a))),
        ("jobScreen", |t, a| Box::pin(t.job_screen(a))),
        ("jobKill", |t, a| Box::pin(t.job_kill(a))),
        ("jobNotify", |t, a| Box::pin(t.job_notify(a))),
        ("jobType", |t, a| Box::pin(t.job_type(a))),
        ("jobKeyPress", |t, a| Box::pin(t.job_key_press(a))),
    ];
    for &(name, method) in JOB_TOOLS {
        let t = tools.clone();
        lofi.set(
            name,
            Function::new(
                ctx.clone(),
                Async(move |args: Value| {
                    let t = t.clone();
                    let args = js_to_json(&args);
                    async move {
                        let id = t.next_tool_id();
                        t.emit(ToolEvent::Start {
                            id,
                            name: name.into(),
                            args: native_args_label("bash", &args),
                        });
                        let res = method(&t, args).await;
                        let (result, is_error) = tool_preview(&res);
                        t.emit(ToolEvent::End {
                            id,
                            result,
                            is_error,
                        });
                        tool_result(res)
                    }
                }),
            )?,
        )?;
    }
    // jobList takes no arguments; bind outside the uniform-args table so
    // calling it with zero args does not fail arity checks.
    let t = tools.clone();
    lofi.set(
        "jobList",
        Function::new(
            ctx.clone(),
            Async(move |_: Opt<Value>| {
                let t = t.clone();
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "jobList".into(),
                        args: String::new(),
                    });
                    let res = t.job_list(serde_json::Value::Null).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;
    Ok(())
}

fn bind_skills_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    _skills_dir: Option<PathBuf>,
) -> rquickjs::Result<()> {
    let t = tools.clone();

    let t1 = t.clone();
    lofi.set(
        "skills",
        Function::new(
            ctx.clone(),
            Async(move |search: Opt<String>| {
                let t = t1.clone();
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "skills".into(),
                        args: search.0.clone().unwrap_or_default(),
                    });
                    let res = t.skills(search.0.as_deref()).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    let t2 = t.clone();
    lofi.set(
        "skill",
        Function::new(
            ctx.clone(),
            Async(move |name: String| {
                let t = t2.clone();
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "skill".into(),
                        args: cap_first_line(&name, 120),
                    });
                    let res = t.skill(&name).await;
                    let (result, is_error) = tool_preview(&res);
                    t.emit(ToolEvent::End {
                        id,
                        result,
                        is_error,
                    });
                    tool_result(res)
                }
            }),
        )?,
    )?;

    Ok(())
}

fn bind_docs_tools<'js>(ctx: &Ctx<'js>, lofi: &Object<'js>) -> rquickjs::Result<()> {
    lofi.set(
        "docs",
        Function::new(
            ctx.clone(),
            Async(move |name: Opt<String>| async move {
                let val = tokio::task::spawn_blocking(move || match name.0 {
                    Some(n) => crate::docs::docs_entry(&n),
                    None => crate::docs::docs_index(),
                })
                .await
                .map_err(|_| rquickjs::Error::Unknown)?;
                Ok::<JsonV, rquickjs::Error>(JsonV(val))
            }),
        )?,
    )?;

    lofi.set(
        "docsSearch",
        Function::new(
            ctx.clone(),
            Async(move |query: String| async move {
                let val = tokio::task::spawn_blocking(move || crate::docs::docs_search(&query))
                    .await
                    .map_err(|_| rquickjs::Error::Unknown)?;
                Ok::<JsonV, rquickjs::Error>(JsonV(val))
            }),
        )?,
    )?;

    Ok(())
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

/// Translate a builtin tool [`lofi_error::Result`] into the owned value that crosses
/// the native-future boundary. Conversion to a JS value/error happens only
/// when rquickjs settles the promise.
fn tool_result(res: std::result::Result<Json, Error>) -> ToolOutput {
    match res {
        Ok(value) => ToolOutput::Value(value),
        Err(error) => ToolOutput::Error(error.to_string()),
    }
}

fn bind_recall_tool<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    recall: Option<RecallFn>,
) -> rquickjs::Result<()> {
    let Some(recall) = recall else {
        lofi.set(
            "recall",
            Function::new(
                ctx.clone(),
                Async(move |_: Opt<Value>| async move {
                    Ok::<JsonV, rquickjs::Error>(JsonV(json!({
                        "text": "recall unavailable: no session file for this session.",
                        "status": "unavailable",
                    })))
                }),
            )?,
        )?;
        return Ok(());
    };
    lofi.set(
        "recall",
        Function::new(
            ctx.clone(),
            Async(move |args: Opt<Value>| {
                let recall = recall.clone();
                let args = args.0.map_or(serde_json::Value::Null, |v| js_to_json(&v));
                async move {
                    let req = parse_recall_args(&args);
                    let outcome = recall(&req);
                    Ok::<JsonV, rquickjs::Error>(JsonV(json!({
                        "text": outcome.text,
                        "status": outcome.status,
                    })))
                }
            }),
        )?,
    )?;
    Ok(())
}

fn parse_recall_args(json: &serde_json::Value) -> lofi_types::recall::RecallRequest {
    use lofi_types::recall::{CompactionTarget, RecallRequest, RecallScope};
    let Some(obj) = json.as_object() else {
        return RecallRequest::default();
    };
    let query = obj
        .get("query")
        .and_then(serde_json::Value::as_str)
        .map(std::string::ToString::to_string);
    let page = obj
        .get("page")
        .and_then(serde_json::Value::as_u64)
        .map_or(1, |n| n.max(1) as usize);
    let expand: Vec<usize> = obj
        .get("expand")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default();
    let scope = match obj.get("scope").and_then(serde_json::Value::as_str) {
        Some("all") => RecallScope::All,
        Some("lineage") => RecallScope::Lineage,
        Some("latest") => RecallScope::Compaction(CompactionTarget::Latest),
        Some(s) if s.starts_with("compaction:") => {
            let n = s.strip_prefix("compaction:").unwrap_or("");
            match n.parse::<usize>() {
                Ok(i) => RecallScope::Compaction(CompactionTarget::Index(i)),
                Err(_) => RecallScope::Lineage,
            }
        }
        _ => RecallScope::Lineage,
    };
    RecallRequest {
        query,
        scope,
        page,
        expand,
    }
}

fn bind_result_tool<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    result: Option<ResultFn>,
) -> rquickjs::Result<()> {
    let Some(result) = result else {
        lofi.set(
            "result",
            Function::new(
                ctx.clone(),
                Async(move |_: String| async move {
                    Ok::<JsonV, rquickjs::Error>(JsonV(json!(
                        "result unavailable: no session file for this session."
                    )))
                }),
            )?,
        )?;
        return Ok(());
    };
    lofi.set(
        "result",
        Function::new(
            ctx.clone(),
            Async(move |id: String| {
                let result = result.clone();
                async move {
                    let text = result(&id);
                    Ok::<JsonV, rquickjs::Error>(JsonV(json!(text)))
                }
            }),
        )?,
    )?;
    Ok(())
}
