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
    /// Config the evaluator was opened from, with its provider resolved. A
    /// child re-attaches from it, so its requests carry the same credentials.
    pub(super) auto_mode_config: Option<Arc<lofi_types::Config>>,
    /// Turn context the evaluator reads; see [`Agent::auto_mode_hints`].
    pub(super) auto_mode_hints: lofi_code::AutoModeHintHandle,
    pub(super) policy_override: lofi_code::policy::PolicyOverride,
    pub(super) skills_dir: Option<PathBuf>,
    pub(super) jobs: lofi_code::tools::JobRegistry,
    pub(super) timers: lofi_code::tools::TimerRegistry,
    pub(super) job_owner: Option<String>,
    pub(super) exec_worker: Arc<std::sync::OnceLock<lofi_code::ExecWorker>>,
    /// `lofi.subagent` hook, `None` where subagents are unsupported (nested
    /// subagent sandboxes).
    pub(super) subagent: Option<lofi_code::SubagentFn>,
    /// Mailbox capability of a spawned child: its own inbox plus the right to
    /// write to the parent. `None` on a session executor, which addresses
    /// runs through the registry instead.
    pub(super) mailbox: Option<super::mailbox::MailboxScope>,
    /// Spawned subagents of this session, resumable by id. The registry lives
    /// here so a CLI backend exposes the same runs an HTTP one does.
    pub(super) subagents: super::SubagentRegistry,
    /// Config and registry the subagent hook resolves model overrides with.
    /// `None` on a child, which cannot spawn its own subagents.
    pub(super) subagent_support: Option<super::SubagentSupport>,
    /// `lofi.models` hook, `None` where the host has no model registry.
    pub(super) models: Option<lofi_code::ModelsFn>,
}

/// Hooks for one tool call. All callbacks belong to the owning session.
#[derive(Clone, Default)]
pub struct ToolCallContext {
    pub events: Option<crate::agent::turn_session::EventSink>,
    pub session: Option<crate::session::store::SessionCursor>,
    pub recall: Option<RecallFn>,
    pub result: Option<ResultFn>,
    pub cancel: Option<Arc<AtomicBool>>,
    pub on_job_acquired: Option<lofi_code::JobAcquireFn>,
    pub record_tool: Option<Arc<dyn Fn(ContentBlock) -> Result<()> + Send + Sync>>,
    /// The lifetime of the turn that owns this context. A tool that settles
    /// after the turn dropped must not extend the transcript. None means the
    /// caller is not a turn, so nothing is sealed.
    pub alive: Option<Arc<AtomicBool>>,
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
            auto_mode_config: None,
            auto_mode_hints: super::auto_mode::hint_handle(),
            policy_override: lofi_code::policy::PolicyOverride::default(),
            skills_dir: None,
            jobs: lofi_code::tools::JobRegistry::new(),
            timers: lofi_code::tools::TimerRegistry::new(),
            job_owner: None,
            exec_worker: Arc::new(std::sync::OnceLock::new()),
            subagent: None,
            mailbox: None,
            subagents: super::SubagentRegistry::default(),
            subagent_support: None,
            models: None,
        })
    }

    /// Configure shell approval without loading unrelated model providers.
    /// # Errors
    /// Returns evaluator configuration, credential or model registry errors.
    pub async fn with_auto_mode_config(
        self,
        config: &lofi_types::Config,
        env: &[(String, String)],
    ) -> Result<Self> {
        super::auto_mode::configure(self, config, env).await
    }

    #[must_use]
    pub fn subagent_hook(&self) -> Option<lofi_code::SubagentFn> {
        self.subagent.clone()
    }

    /// Install the `lofi.subagent` hook the sandbox reports from.
    #[must_use]
    pub fn with_subagent_hook(mut self, hook: Option<lofi_code::SubagentFn>) -> Self {
        self.subagent = hook;
        self
    }

    /// Spawned subagents of this session, shared by every clone.
    #[must_use]
    pub fn subagents(&self) -> super::SubagentRegistry {
        self.subagents.clone()
    }

    /// Config and registry a subagent hook resolves model overrides with.
    #[must_use]
    pub fn subagent_support(&self) -> Option<super::SubagentSupport> {
        self.subagent_support.clone()
    }

    /// Attach subagent support: the session's registry, its model listing and
    /// the config a child's model override resolves against.
    #[must_use]
    pub fn with_subagent_support(mut self, support: super::SubagentSupport) -> Self {
        self.models = Some(support.models_fn());
        self.subagent_support = Some(support);
        self
    }

    #[must_use]
    pub fn has_auto_mode(&self) -> bool {
        self.auto_mode.is_some()
    }

    /// Drop the subagent hook. Used when the executor moves to a backend
    /// whose registry the UI cannot reach for cleanup. The models listing
    /// goes with it: `lofi.models` exists so a guest can pick a model for
    /// `lofi.subagent`, and leaving it behind would advertise overrides the
    /// session cannot use.
    #[must_use]
    pub fn without_subagent(mut self) -> Self {
        self.subagent = None;
        self.models = None;
        self.subagent_support = None;
        self
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

    /// Execution state for a spawned child: the session's jobs, policy and
    /// sandbox configuration stay shared, while the identity, the hook and
    /// the sandbox thread are the child's own.
    #[must_use]
    pub(crate) fn with_child_identity(
        mut self,
        owner: &str,
        mailbox: Option<super::mailbox::MailboxScope>,
        subagent: Option<lofi_code::SubagentFn>,
        models: Option<lofi_code::ModelsFn>,
    ) -> Self {
        self.job_owner = Some(owner.to_string());
        self.mailbox = mailbox;
        self.subagent = subagent;
        self.models = models;
        // A child owns its own runs and cannot spawn: a shared registry would
        // also form an Arc cycle with the entries that hold the children.
        self.subagents = super::SubagentRegistry::default();
        self.subagent_support = None;
        self.reset_exec_worker()
    }

    /// A fresh sandbox thread, so an idle entry does not hold one: it
    /// restarts lazily on the first exec of the next resume.
    #[must_use]
    pub fn reset_exec_worker(mut self) -> Self {
        // Sharing the parent's worker deadlocks: the parent worker is busy
        // awaiting the subagent hook while the child's exec jobs queue
        // behind it.
        self.exec_worker = Arc::new(std::sync::OnceLock::new());
        self
    }

    /// Owning subagent run id, when this executor belongs to a child.
    #[must_use]
    pub fn job_owner(&self) -> Option<&str> {
        self.job_owner.as_deref()
    }

    /// Give this executor a fresh auto-mode turn context, so a child does not
    /// read the session hints its parent's evaluator captured.
    #[must_use]
    pub fn with_fresh_auto_mode_hints(mut self) -> Self {
        self.auto_mode_hints = super::auto_mode::hint_handle();
        self
    }

    /// Replace the auto-mode evaluator, or drop it.
    #[must_use]
    pub fn with_auto_mode(mut self, auto_mode: Option<lofi_code::AutoModeFn>) -> Self {
        self.auto_mode = auto_mode;
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

    /// Session-scoped wake-up timers shared with the owning agent.
    #[must_use]
    pub fn timers(&self) -> lofi_code::tools::TimerRegistry {
        self.timers.clone()
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
        let result = self
            .execute_tools(&[(id, "exec", input)], &context)
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

    #[allow(clippy::too_many_lines)]
    pub(super) async fn execute_tools(
        &self,
        tool_uses: &[(&str, &str, &serde_json::Value)],
        call: &ToolCallContext,
    ) -> Result<Vec<ContentBlock>> {
        if tool_uses.is_empty() {
            return Ok(Vec::new());
        }
        let native_cursor = call.session.clone();
        let recall = call.recall.clone();
        let result = call.result.clone();
        let cancel = call.cancel.as_ref();
        let on_job_acquired = call.on_job_acquired.clone();
        let mut results: Vec<ContentBlock> = Vec::with_capacity(tool_uses.len());
        // Native tool events are emitted from a *sync* `on_tool_event`
        // callback inside the sandbox, so they can't `await` on the bounded
        // UI channel. `try_send` would drop events when the channel fills
        // (e.g. a `Promise.all` burst of Start/End events), leaving native
        // tiles stuck "running" even after the exec succeeds. Relay them
        // through an unbounded channel whose forwarder drains onto the
        // bounded channel with `send().await` — lossless and order-preserving.
        let (relay_tx, mut relay_rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let relay_dst = call.events.clone();
        let _forwarder = tokio::spawn(async move {
            while let Some(ev) = relay_rx.recv().await {
                match &relay_dst {
                    Some(dst) => {
                        if !dst.emit(ev).await {
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
            let dispatch = std::time::Instant::now();
            if *name != "exec" {
                let content = format!("unknown tool: {name}");
                let elapsed_ms = dispatch.elapsed().as_millis() as u64;
                if !emit(
                    call.events.as_ref(),
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
                let elapsed_ms = dispatch.elapsed().as_millis() as u64;
                if !emit(
                    call.events.as_ref(),
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
                call.events.as_ref(),
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
            let native_records: Arc<Mutex<Vec<NativeToolRecord>>> =
                Arc::new(Mutex::new(Vec::new()));
            let on_tool_event: Arc<dyn Fn(ToolEvent) + Send + Sync> = {
                let native_pending = native_pending.clone();
                let native_records = native_records.clone();
                let retain_records = native_cursor.is_some();
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
                            if retain_records {
                                lock(&native_records).push(NativeToolRecord {
                                    parent: event_parent.clone(),
                                    call_id: id,
                                    name,
                                    args,
                                    result,
                                    is_error,
                                });
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
                timers: self.timers.clone(),
                job_owner: self.job_owner.clone(),
                on_job_acquired: on_job_acquired.clone(),
                subagent: self.subagent.clone(),
                models: self.models.clone(),
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
                    if native_cursor.is_some() {
                        lock(&native_records).push(NativeToolRecord {
                            parent: parent.clone(),
                            call_id,
                            name,
                            args,
                            result: result.clone(),
                            is_error: true,
                        });
                    }
                    let _ = relay_tx.send(AgentEvent::NativeToolEnd {
                        parent: parent.clone(),
                        id: call_id,
                        result,
                        is_error: true,
                    });
                }
            }
            // A tool that outlives its turn may not extend a transcript no turn
            // will settle.
            let live = call
                .alive
                .as_ref()
                .is_none_or(|alive| alive.load(std::sync::atomic::Ordering::Acquire));
            if let (Some(cursor), true) = (native_cursor.as_ref(), live) {
                let records = std::mem::take(&mut *lock(&native_records));
                if let Err(error) =
                    cursor.record(crate::session::recorder::SessionRecord::NativeTools { records })
                {
                    let _ = relay_tx.send(AgentEvent::Notice(format!(
                        "transcript write failed for native tool result: {error}"
                    )));
                }
            }
            let elapsed_ms = dispatch.elapsed().as_millis() as u64;
            if !emit(
                call.events.as_ref(),
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

/// Publish one tool event through the turn's sink.
///
/// The sink folds the event into the accounting the transcript summary is
/// built from, so a tool's elapsed time survives a resume. False when the
/// consumer is gone.
async fn emit(events: Option<&crate::agent::turn_session::EventSink>, event: AgentEvent) -> bool {
    match events {
        Some(events) => events.emit(event).await,
        None => true,
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
