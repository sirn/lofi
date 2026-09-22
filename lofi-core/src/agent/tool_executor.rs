#![allow(clippy::wildcard_imports)]

use super::*;

/// Session-scoped execution state shared by native and external backends.
#[derive(Clone)]
pub struct ToolExecutor {
    pub(super) root: PathBuf,
    pub(super) tmp_dir: PathBuf,
    pub(super) tmp_lease: Option<Arc<state::SessionTempDir>>,
    pub(super) bash_env: BashEnv,
    pub(super) shell_policy: ResolvedPolicy,
    pub(super) truncate: lofi_code::TruncatedCap,
    pub(super) image: lofi_types::ImageConfig,
    pub(super) confirm_tx: Option<tokio::sync::mpsc::UnboundedSender<ConfirmRequest>>,
    pub(super) confirm_counter: Arc<AtomicU64>,
    pub(super) auto_mode: Option<lofi_code::AutoModeFn>,
    pub(super) policy_override: lofi_code::policy::PolicyOverride,
    pub(super) skills_dir: Option<PathBuf>,
    pub(super) jobs: lofi_code::tools::JobRegistry,
    pub(super) exec_worker: Arc<std::sync::OnceLock<lofi_code::ExecWorker>>,
}

/// Hooks for one tool call. All callbacks belong to the owning session.
#[derive(Clone, Default)]
pub struct ToolCallContext {
    pub events: Option<Sender<AgentEvent>>,
    pub session: Option<crate::session::store::SessionCursor>,
    pub recall: Option<RecallFn>,
    pub result: Option<ResultFn>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub on_job_acquired: Option<lofi_code::JobAcquireFn>,
    pub record_tool: Option<Arc<dyn Fn(ContentBlock) -> Result<()> + Send + Sync>>,
}

impl ToolExecutor {
    /// Create execution state without constructing a model provider.
    /// # Errors
    /// Returns an error when the session temporary directory cannot be created.
    pub fn new(
        root: &std::path::Path,
        config: &lofi_types::Config,
        env: &[(String, String)],
    ) -> Result<Self> {
        let lease = state::create_session_tmp_dir(root)?;
        Ok(Self {
            root: root.to_path_buf(),
            tmp_dir: lease.path().to_path_buf(),
            tmp_lease: Some(Arc::new(lease)),
            bash_env: crate::bash_env::resolve_bash_env(&config.bash, env),
            shell_policy: lofi_code::policy::defaults::resolve(&config.shell_policy),
            truncate: lofi_code::TruncatedCap {
                max_lines: config.truncate.max_lines,
                max_bytes: config.truncate.max_bytes,
            },
            image: config.image,
            confirm_tx: None,
            confirm_counter: Arc::new(AtomicU64::new(0)),
            auto_mode: None,
            policy_override: lofi_code::policy::PolicyOverride::default(),
            skills_dir: None,
            jobs: lofi_code::tools::JobRegistry::new(),
            exec_worker: Arc::new(std::sync::OnceLock::new()),
        })
    }

    /// Configure shell approval without loading unrelated model providers.
    /// # Errors
    /// Returns evaluator configuration, credential or model registry errors.
    pub async fn with_auto_mode_config(
        mut self,
        config: &lofi_types::Config,
        env: &[(String, String)],
    ) -> Result<Self> {
        let Some(auto_cfg) = config
            .shell_policy
            .auto_mode
            .as_ref()
            .filter(|mode| mode.enable)
        else {
            return Ok(self);
        };
        let _env_guard = crate::config_loader::apply_env_specs(env);
        let mut evaluator_config = config.clone();
        evaluator_config
            .providers
            .retain(|name, _| name == &auto_cfg.provider);
        crate::config_loader::resolve_config(&mut evaluator_config).await?;
        let registry = match ModelRegistry::load_async(&evaluator_config).await {
            Ok(registry) => registry,
            Err(error) => {
                tracing::debug!(%error, "auto-mode discovery failed; using static models");
                ModelRegistry::load(&evaluator_config)?
            }
        };
        self.auto_mode = super::auto_mode::build_auto_mode(
            &evaluator_config,
            &registry,
            auto_cfg,
            &self.root,
            self.skills_dir.as_deref(),
        )?;
        Ok(self)
    }

    #[must_use]
    pub fn has_auto_mode(&self) -> bool {
        self.auto_mode.is_some()
    }

    #[must_use]
    pub fn with_confirm_tx(
        mut self,
        tx: tokio::sync::mpsc::UnboundedSender<ConfirmRequest>,
    ) -> Self {
        self.confirm_tx = Some(tx);
        self
    }

    #[must_use]
    pub fn with_skills_dir(mut self, path: Option<PathBuf>) -> Self {
        self.skills_dir = path;
        self
    }

    #[must_use]
    pub(crate) fn system_prompt(&self) -> String {
        super::model::assemble_system_prompt(
            self.skills_dir.as_deref().and_then(std::path::Path::parent),
            &self.root,
        )
    }

    #[must_use]
    pub fn jobs(&self) -> lofi_code::tools::JobRegistry {
        self.jobs.clone()
    }

    #[must_use]
    pub fn policy_override(&self) -> lofi_code::policy::PolicyOverride {
        self.policy_override.clone()
    }

    /// Execute through the same path as the native agent.
    /// # Errors
    /// Returns an error if the consumer disconnects or execution cannot start.
    pub async fn execute(
        &self,
        id: &str,
        input: &serde_json::Value,
        context: ToolCallContext,
    ) -> Result<ContentBlock> {
        if context
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(Error::Cancelled);
        }
        if !emit(
            context.events.as_ref(),
            AgentEvent::ToolStart {
                id: id.into(),
                name: "exec".into(),
            },
        )
        .await
        {
            return Err(Error::Cancelled);
        }
        if let Some(record) = &context.record_tool {
            record(ContentBlock::ToolUse {
                id: id.into(),
                name: "exec".into(),
                input: input.clone(),
            })?;
        }
        let mut stats = TurnStats::new();
        stats.tool_start(id);
        let result = self
            .execute_tools(
                &[(id, "exec", input)],
                context.events.as_ref(),
                Some(&mut stats),
                context.session,
                context.recall,
                context.result,
                context.cancel.as_ref(),
                context.on_job_acquired,
            )
            .await?
            .pop()
            .ok_or_else(|| Error::State("exec returned no result".into()))?;
        if let Some(record) = &context.record_tool {
            record(result.clone())?;
        }
        Ok(result)
    }

    fn sandbox_worker(&self) -> &lofi_code::ExecWorker {
        self.exec_worker.get_or_init(lofi_code::ExecWorker::spawn)
    }

    #[allow(clippy::too_many_lines, clippy::too_many_arguments)]
    pub(super) async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        tx: Option<&Sender<AgentEvent>>,
        mut stats: Option<&mut TurnStats>,
        native_cursor: Option<crate::session::store::SessionCursor>,
        recall: Option<RecallFn>,
        result: Option<ResultFn>,
        cancel: Option<&Arc<AtomicBool>>,
        on_job_acquired: Option<lofi_code::JobAcquireFn>,
    ) -> Result<Vec<ContentBlock>> {
        if tool_uses.is_empty() {
            return Ok(Vec::new());
        }
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        // Native tool events are emitted from a *sync* `on_tool_event`
        // callback inside the sandbox, so they can't `await` on the bounded
        // UI channel. `try_send` would drop events when the channel fills
        // (e.g. a `Promise.all` burst of Start/End events), leaving native
        // tiles stuck "running" even after the exec succeeds. Relay them
        // through an unbounded channel whose forwarder drains onto the
        // bounded channel with `send().await` — lossless and order-preserving.
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let relay_dst = tx.cloned();
        let _forwarder = tokio::spawn(async move {
            while let Some(ev) = relay_rx.recv().await {
                match &relay_dst {
                    Some(dst) => {
                        if dst.send(ev).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        drop(ev);
                    }
                }
            }
        });
        for (id, name, input) in tool_uses {
            if *name != "exec" {
                let content = format!("unknown tool: {name}");
                let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.to_string(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                    images: Vec::new(),
                });
                continue;
            }
            let (code, strings, display) = parse_exec_input(input);
            if code.is_empty() {
                let content = "exec: missing required 'code' argument".to_string();
                let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
                if !emit(
                    tx,
                    AgentEvent::ToolEnd {
                        id: id.to_string(),
                        result: content.clone(),
                        is_error: true,
                        elapsed_ms,
                    },
                )
                .await
                {
                    return Err(Error::Cancelled);
                }
                results.push(ContentBlock::ToolResult {
                    tool_use_id: id.to_string(),
                    content,
                    is_error: true,
                    images: Vec::new(),
                });
                continue;
            }
            // The ToolInput event must land before we invoke the sandbox;
            // if the consumer is gone, stop instead of running tools (which
            // may shell out or mutate files) for a dead receiver.
            if !emit(
                tx,
                AgentEvent::ToolInput {
                    id: id.to_string(),
                    code: code.clone(),
                    label: exec_label(&display),
                },
            )
            .await
            {
                return Err(Error::Cancelled);
            }
            let native_tx = relay_tx.clone();
            let parent = id.to_string();
            let native_pending: Arc<Mutex<HashMap<u64, (String, String)>>> =
                Arc::new(Mutex::new(HashMap::new()));
            let on_tool_event: Arc<dyn Fn(ToolEvent) + Send + Sync> = {
                let native_pending = native_pending.clone();
                let native_cursor = native_cursor.clone();
                let event_parent = parent.clone();
                Arc::new(move |ev: ToolEvent| match ev {
                    ToolEvent::Start { id, name, args } => {
                        lock(&native_pending).insert(id, (name.clone(), args.clone()));
                        let _ = native_tx.send(AgentEvent::NativeToolStart {
                            parent: event_parent.clone(),
                            id,
                            name,
                            args,
                        });
                    }
                    ToolEvent::End {
                        id,
                        result,
                        is_error,
                    } => {
                        let display_result = cap_tool_result(&result);
                        if let Some((name, args)) = lock(&native_pending).remove(&id) {
                            if let Some(cursor) = native_cursor.as_ref() {
                                let record = NativeToolRecord {
                                    parent: event_parent.clone(),
                                    call_id: id,
                                    name,
                                    args,
                                    result,
                                    is_error,
                                };
                                if let Err(error) = cursor.record(
                                    crate::session::recorder::SessionRecord::NativeTool { record },
                                ) {
                                    let _ = native_tx.send(AgentEvent::Notice(format!(
                                        "transcript write failed for native tool result: {error}"
                                    )));
                                }
                            }
                        }
                        let _ = native_tx.send(AgentEvent::NativeToolEnd {
                            parent: event_parent.clone(),
                            id,
                            result: display_result,
                            is_error,
                        });
                    }
                }) as Arc<dyn Fn(ToolEvent) + Send + Sync>
            };
            let confirm: Option<lofi_code::ConfirmFn> = self.confirm_tx.as_ref().map(|tx| {
                let tx = tx.clone();
                let counter = self.confirm_counter.clone();
                Arc::new(move |prompt: lofi_code::ConfirmPrompt| {
                    let tx = tx.clone();
                    let counter = counter.clone();
                    Box::pin(async move {
                        let id = counter.fetch_add(1, Ordering::SeqCst);
                        let (resp_tx, resp_rx) = oneshot::channel();
                        let req = ConfirmRequest {
                            id,
                            command: prompt.command,
                            reason: prompt.reason,
                            active: prompt.active,
                            respond: resp_tx,
                        };
                        if tx.send(req).is_err() {
                            return false;
                        }
                        resp_rx.await.unwrap_or(false)
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + Sync>>
                }) as lofi_code::ConfirmFn
            });
            let exec_ctx = ExecCtx {
                root: self.root.clone(),
                tmp_dir: self.tmp_dir.clone(),
                strings,
                recall: recall.clone(),
                result: result.clone(),
                on_tool_event: Some(on_tool_event),
                bash_env: self.bash_env.clone(),
                shell_policy: self.shell_policy.clone(),
                confirm,
                auto_mode: self.auto_mode.clone(),
                policy_override: self.policy_override.clone(),
                skills_dir: self.skills_dir.clone(),
                truncate: self.truncate,
                jobs: self.jobs.clone(),
                on_job_acquired: on_job_acquired.clone(),
            };
            let outcome = self
                .sandbox_worker()
                .exec(
                    &code,
                    exec_ctx,
                    ExecOptions {
                        timeout: lofi_code::DEFAULT_GUEST_TIMEOUT,
                        max_value_bytes: MAX_EXEC_RESULT_BYTES,
                        cancel: cancel.cloned(),
                    },
                )
                .await;
            // A tool may return a tagged image (`read` on an image file).
            // Upgrade it to a `ToolResultImage` carried on this result block,
            // so the model sees the image in the same round as the result on
            // every provider. The heavyweight base64 is stripped out of the
            // text payload, leaving a compact `bytes` count in the transcript.
            let (content, is_error, result_images) = match outcome {
                Ok(r) => {
                    let has_value = r.has_returned_value();
                    let mut value = r.value;
                    match upgrade_exec_image(&mut value, r.image, &self.image) {
                        Ok(img) => {
                            let is_error = !r.errors.is_empty();
                            let mut output = serde_json::Map::new();
                            output.insert("ok".into(), serde_json::Value::Bool(!is_error));
                            if is_error {
                                output.insert("errors".into(), serde_json::Value::Array(r.errors));
                            }
                            if has_value {
                                output.insert("value".into(), value);
                            }
                            if !r.logs.is_empty() {
                                output.insert("logs".into(), serde_json::Value::String(r.logs));
                            }
                            if r.value_truncated {
                                output.insert("truncated".into(), serde_json::Value::Bool(true));
                            }
                            let content = serde_json::to_string(&output)
                                .unwrap_or_else(|_| "{\"ok\":true}".to_string());
                            (cap_exec_result(&content), is_error, img)
                        }
                        Err(error) => (cap_exec_result(&error.to_string()), true, None),
                    }
                }
                Err(e) => (cap_exec_result(&e.to_string()), true, None),
            };
            if is_error {
                // A rejected guest promise can short-circuit concurrent native
                // calls before their futures emit `ToolEvent::End`. Close every
                // still-pending row explicitly so the live UI cannot leave a
                // spinner behind after the parent exec has already failed.
                // These synthetic cancellations are also persisted, making
                // resume reproduce the settled state.
                let pending = {
                    let mut pending = lock(&native_pending);
                    pending.drain().collect::<Vec<_>>()
                };
                for (call_id, (name, args)) in pending {
                    let result = "cancelled because parent exec failed".to_string();
                    if let Some(cursor) = native_cursor.as_ref() {
                        let record = NativeToolRecord {
                            parent: parent.clone(),
                            call_id,
                            name,
                            args,
                            result: result.clone(),
                            is_error: true,
                        };
                        if let Err(error) = cursor
                            .record(crate::session::recorder::SessionRecord::NativeTool { record })
                        {
                            let _ = relay_tx.send(AgentEvent::Notice(format!(
                                "transcript write failed for native tool result: {error}"
                            )));
                        }
                    }
                    let _ = relay_tx.send(AgentEvent::NativeToolEnd {
                        parent: parent.clone(),
                        id: call_id,
                        result,
                        is_error: true,
                    });
                }
            }
            let elapsed_ms = stats.as_deref_mut().map_or(0, |s| s.tool_end(id));
            if !emit(
                tx,
                AgentEvent::ToolEnd {
                    id: id.to_string(),
                    result: content.clone(),
                    is_error,
                    elapsed_ms,
                },
            )
            .await
            {
                return Err(Error::Cancelled);
            }
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content,
                is_error,
                images: result_images.into_iter().collect(),
            });
        }
        Ok(results)
    }
}

/// Normalize an image returned out of band by the exec sandbox. `QuickJS` carries
/// only a small token, so source image bytes never become a base64 guest string
/// or a second host JSON allocation. The durable result keeps only a byte count.
pub(super) fn upgrade_exec_image(
    value: &mut serde_json::Value,
    image: Option<lofi_code::ExecImage>,
    config: &lofi_types::ImageConfig,
) -> Result<Option<lofi_types::ToolResultImage>> {
    use std::io::Seek as _;

    let Some(mut image) = image else {
        return Ok(None);
    };
    image.file.rewind()?;
    let reader = std::io::BufReader::new(image.file);
    let (bytes, media_type) = crate::image::normalize_reader(reader, config)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("bytes".to_string(), serde_json::json!(bytes.len()));
        obj.insert("media_type".to_string(), serde_json::json!(&media_type));
    }
    Ok(Some(lofi_types::ToolResultImage { bytes, media_type }))
}
