//! Binding of the builtin tools onto the guest `lofi` object.
//!
//! `bind_tools` mounts the file/shell/agent methods as native `QuickJS`
//! functions; `tool_result` translates a tool `Result` into a
//! `rquickjs::Result<JsonV>` so tool errors surface as thrown JS `Error`s.

use super::convert::js_to_json;
#[allow(clippy::wildcard_imports)]
use super::*;

/// Bind the builtin file/shell tool methods onto `lofi`.
pub(super) fn bind_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    agent: Option<AgentFn>,
    recall: Option<RecallFn>,
    result: Option<ResultFn>,
    skills_dir: Option<PathBuf>,
) -> rquickjs::Result<()> {
    bind_file_tools(ctx, lofi, tools)?;
    bind_agent_tool(ctx, lofi, tools, agent)?;
    bind_recall_tool(ctx, lofi, recall)?;
    bind_result_tool(ctx, lofi, result)?;
    bind_skills_tools(ctx, lofi, tools, skills_dir)?;
    bind_docs_tools(ctx, lofi)?;
    Ok(())
}

/// Bind `read`/`ls`/`find`/`grep`/`write`/`edit`/`bash`.
///
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
            Async(move |glob: String, dir: Opt<String>| {
                let t = t.clone();
                let d = dir.0.as_deref().unwrap_or("").to_string();
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
                    let res = t.find(&glob, Some(d.as_str())).await;
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

/// Bind `agent` / `spawn`.
fn bind_agent_tool<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    agent: Option<AgentFn>,
) -> rquickjs::Result<()> {
    let tools = tools.clone();
    lofi.set(
        "agent",
        Function::new(
            ctx.clone(),
            Async(move |prompt: String, opts: Opt<Value>| {
                let agent = agent.clone();
                let tools = tools.clone();
                let opts_json = opts.0.map(|v| js_to_json(&v)).filter(|v| !v.is_null());
                async move {
                    // Surface the subagent call as a native tool event so the
                    // UI can render it (with a preview of its result) under
                    // the parent exec block, just like read/bash.
                    let id = tools.next_tool_id();
                    tools.emit(ToolEvent::Start {
                        id,
                        name: "agent".to_string(),
                        args: cap_first_line(&prompt, 120),
                    });
                    let Some(agent) = agent else {
                        let msg = "agent() is not available in this context".to_string();
                        tools.emit(ToolEvent::End {
                            id,
                            result: msg.clone(),
                            is_error: true,
                        });
                        return Err(rquickjs::Error::IntoJs {
                            from: "lofi",
                            to: "value",
                            message: Some(msg),
                        });
                    };
                    let req = AgentRequest {
                        prompt,
                        opts: opts_json,
                    };
                    match agent(req).await {
                        Ok(s) => {
                            tools.emit(ToolEvent::End {
                                id,
                                result: s.clone(),
                                is_error: false,
                            });
                            Ok(JsonV(json!(s)))
                        }
                        Err(e) => {
                            let msg = e.to_string();
                            tools.emit(ToolEvent::End {
                                id,
                                result: msg.clone(),
                                is_error: true,
                            });
                            Err(rquickjs::Error::IntoJs {
                                from: "lofi",
                                to: "value",
                                message: Some(msg),
                            })
                        }
                    }
                }
            }),
        )?,
    )?;
    Ok(())
}

/// Bind `lofi.skills()` and `lofi.skill(name)`.
///
/// Skills are markdown files discovered from a global directory (`<config_dir>/skills/`)
/// and a per-workspace directory (`<root>/.lofi/skills/`). The `skills_dir`
/// parameter is the global directory; the workspace directory is derived from
/// the tool bundle's `root`.
fn bind_skills_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    _skills_dir: Option<PathBuf>,
) -> rquickjs::Result<()> {
    // Use the existing tool bundle, which already carries skills_dir and
    // the event callback. A separate bundle would drop tool events.
    let t = tools.clone();

    let t1 = t.clone();
    lofi.set(
        "skills",
        Function::new(
            ctx.clone(),
            Async(move || {
                let t = t1.clone();
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "skills".into(),
                        args: String::new(),
                    });
                    let res = t.skills().await;
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

/// Bind `lofi.docs(name?)` and `lofi.docs_search(query)`.
///
/// These are pure computation on compile-time-embedded data (no I/O), but
/// use `Async` closures to go through the same promise/`IntoJs` path as all
/// other tool bindings — the sync `Function::new` path has different GC
/// interactions under memory pressure.
/// Bind `lofi.docs(name?)` and `lofi.docs_search(query)`.
///
/// These are pure computation on compile-time-embedded data (no I/O), but
/// use `Async` closures to go through the same promise/`IntoJs` path as all
/// other tool bindings.
fn bind_docs_tools<'js>(ctx: &Ctx<'js>, lofi: &Object<'js>) -> rquickjs::Result<()> {
    lofi.set(
        "docs",
        Function::new(
            ctx.clone(),
            Async(move |name: Opt<String>| {
                let val = match &name.0 {
                    Some(n) => crate::docs::docs_entry(n),
                    None => crate::docs::docs_index(),
                };
                async move { Ok::<JsonV, rquickjs::Error>(JsonV(val)) }
            }),
        )?,
    )?;

    lofi.set(
        "docs_search",
        Function::new(
            ctx.clone(),
            Async(move |query: String| {
                let val = crate::docs::docs_search(&query);
                async move { Ok::<JsonV, rquickjs::Error>(JsonV(val)) }
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

/// Bind `lofi.recall({ query?, scope?, page?, expand? })` — session-history
/// search (including messages a compaction folded away). Delegates to the
/// `RecallFn` supplied by `lofi-core`, which owns the transcript.
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

/// Parse `lofi.recall`'s argument object into a `RecallRequest`.
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

/// Bind `lofi.result(eventId) -> string` — recover the original, pre-elision
/// content of one message event (a stubbed tool result or tool-call) by id.
/// The inverse of compaction's tiered-retention elision; reads the session
/// transcript fresh via the `ResultFn` supplied by `lofi-core`.
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
