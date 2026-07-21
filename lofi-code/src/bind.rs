//! Binding of the builtin tools onto the guest `lofi` object.
//!
//! `bind_tools` mounts the file/shell/agent methods as native `QuickJS`
//! functions; `tool_result` translates a tool `Result` into a
//! `rquickjs::Result<JsonV>` so tool errors surface as thrown JS `Error`s.

#[allow(clippy::wildcard_imports)]
use super::*;
use super::convert::js_to_json;

/// Bind the builtin file/shell tool methods onto `lofi`.
pub(super) fn bind_tools<'js>(
    ctx: &Ctx<'js>,
    lofi: &Object<'js>,
    tools: &Arc<BuiltinTools>,
    agent: Option<AgentFn>,
) -> rquickjs::Result<()> {
    bind_file_tools(ctx, lofi, tools)?;
    bind_agent_tool(ctx, lofi, tools, agent)?;
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
        "read_tmp",
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
                        name: "read_tmp".into(),
                        args: cap_first_line(&args_label, 120),
                    });
                    let res = t.read_tmp(&path, offset, limit).await;
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
            Async(move |dir: Opt<String>, opts: Opt<Value>| {
                let t = t.clone();
                let d = dir.0.as_deref().unwrap_or("").to_string();
                let limit = parse_limit_opt(opts);
                async move {
                    let id = t.next_tool_id();
                    t.emit(ToolEvent::Start {
                        id,
                        name: "ls".into(),
                        args: cap_first_line(&d, 120),
                    });
                    let res = t.ls(&d, limit).await;
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
            Async(move |glob: String, dir: Opt<String>, opts: Opt<Value>| {
                let t = t.clone();
                let d = dir.0.as_deref().unwrap_or("").to_string();
                let limit = parse_limit_opt(opts);
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
                    let res = t.find(&glob, Some(d.as_str()), limit).await;
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
