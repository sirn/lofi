use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, PromptKind, Role, RunModel, Usage};
use tokio::sync::{mpsc, oneshot};

use super::claude::ClaudeSession;
use crate::agent::{
    Agent, AgentEvent, ConfirmRequest, RunControls, ToolCallContext, ToolExecutor, TurnInput,
    TurnRequest,
};
use crate::session::recorder::{SessionRecorder, TurnOutcome, TurnSummary};
use crate::session::store::SessionCursor;

/// The active model backend. Conversation history and tools belong to Lofi.
#[derive(Clone)]
pub enum Backend {
    Native(Box<Agent>),
    External(Box<ExternalBackend>),
}

impl From<Agent> for Backend {
    fn from(agent: Agent) -> Self {
        Self::Native(Box::new(agent))
    }
}

impl Backend {
    #[must_use]
    pub fn native(&self) -> Option<&Agent> {
        match self {
            Self::Native(agent) => Some(agent),
            Self::External(_) => None,
        }
    }

    /// Configure an external backend without starting it or reading credentials.
    #[must_use]
    pub fn external(
        name: String,
        config: lofi_types::BackendConfig,
        root: PathBuf,
        executor: ToolExecutor,
    ) -> Self {
        Self::External(Box::new(ExternalBackend {
            model: config.model().map(str::to_owned),
            name,
            config,
            root,
            system_prompt: executor.system_prompt(),
            executor,
            worker: Arc::new(Mutex::new(None)),
        }))
    }

    #[must_use]
    pub fn tool_executor(&self) -> ToolExecutor {
        match self {
            Self::Native(agent) => agent.tool_executor(),
            Self::External(external) => external.executor.clone(),
        }
    }

    #[must_use]
    pub fn with_confirm_tx(&self, tx: mpsc::UnboundedSender<ConfirmRequest>) -> Self {
        match self {
            Self::Native(agent) => agent.with_confirm_tx(tx).into(),
            Self::External(external) => {
                let mut external = external.clone();
                external.executor = external.executor.with_confirm_tx(tx);
                Self::External(external)
            }
        }
    }

    #[must_use]
    pub fn jobs(&self) -> crate::JobRegistry {
        match self {
            Self::Native(agent) => agent.jobs(),
            Self::External(external) => external.executor.jobs(),
        }
    }

    #[must_use]
    pub fn policy_override(&self) -> crate::PolicyOverride {
        match self {
            Self::Native(agent) => agent.policy_override().clone(),
            Self::External(external) => external.executor.policy_override(),
        }
    }

    #[must_use]
    pub fn has_auto_mode(&self) -> bool {
        match self {
            Self::Native(agent) => agent.has_auto_mode(),
            Self::External(external) => external.executor.has_auto_mode(),
        }
    }

    #[must_use]
    pub fn system_prompt(&self) -> &str {
        match self {
            Self::Native(agent) => agent.system_prompt(),
            Self::External(external) => &external.system_prompt,
        }
    }

    #[must_use]
    pub fn model_choices(&self) -> Vec<lofi_types::ModelChoice> {
        let Self::External(external) = self else {
            return Vec::new();
        };
        Self::configured_models(&external.name, &external.config, external.model.as_deref())
    }

    #[must_use]
    pub fn configured_models(
        name: &str,
        config: &lofi_types::BackendConfig,
        selected: Option<&str>,
    ) -> Vec<lofi_types::ModelChoice> {
        let mut models = config.models().to_vec();
        if let Some(model) = selected {
            models.retain(|id| id != model);
            models.insert(0, model.into());
        }
        if models.is_empty() {
            models.push("default".into());
        }
        models
            .into_iter()
            .map(|id| lofi_types::ModelChoice {
                provider: name.into(),
                name: id.clone(),
                id,
                thinking_levels: Vec::new(),
                service_tiers: Vec::new(),
                supports_image: false,
                context_window: None,
            })
            .collect()
    }

    #[must_use]
    pub fn model_label(&self) -> String {
        match self {
            Self::External(external) => external.model.as_ref().map_or_else(
                || format!("{}/default", external.name),
                |model| format!("{}/{model}", external.name),
            ),
            Self::Native(agent) => {
                format!("{}/{}", agent.run_model().provider, agent.run_model().id)
            }
        }
    }

    /// Select the model for subsequent turns without replacing the session.
    /// # Errors
    /// Returns an error for models outside this backend's configured list.
    pub fn select_model(&mut self, query: &str) -> Result<()> {
        let Self::External(external) = self else {
            return Err(Error::Config("expected an external backend".into()));
        };
        let model = query
            .strip_prefix(&format!("{}/", external.name))
            .unwrap_or(query);
        if !external.config.models().iter().any(|id| id == model)
            && external.config.model() != Some(model)
            && model != "default"
        {
            return Err(Error::Config(format!("unknown backend model: {query}")));
        }
        external.model = (model != "default").then(|| model.into());
        Ok(())
    }

    /// Run one turn using a common presentation and tool-execution boundary.
    /// # Errors
    /// Returns native or external turn errors without retrying external prompts.
    pub async fn run_turn(
        &self,
        messages: &mut Vec<Message>,
        request: TurnRequest,
        events: mpsc::Sender<AgentEvent>,
        cursor: Option<&SessionCursor>,
        controls: RunControls,
    ) -> Result<()> {
        match self {
            Self::Native(agent) => {
                agent
                    .run_turn(messages, request, events, cursor, controls)
                    .await
            }
            Self::External(external) => {
                external
                    .run(messages, request, events, cursor.cloned(), controls)
                    .await
            }
        }
    }
}

#[derive(Clone)]
pub struct ExternalBackend {
    name: String,
    model: Option<String>,
    config: lofi_types::BackendConfig,
    root: PathBuf,
    system_prompt: String,
    executor: ToolExecutor,
    worker: Arc<Mutex<Option<mpsc::Sender<Work>>>>,
}

async fn open_external(
    config: &lofi_types::BackendConfig,
    root: &Path,
    executor: ToolExecutor,
    resume: Option<&str>,
    selected_model: Option<&str>,
) -> Result<ClaudeSession> {
    let executable = std::env::current_exe()?;
    match config {
        lofi_types::BackendConfig::ClaudeCode { command, .. } => {
            ClaudeSession::open(command, selected_model, root, &executable, executor, resume).await
        }
    }
}

struct Work {
    model: Option<String>,
    history: Vec<Message>,
    request: TurnRequest,
    events: mpsc::Sender<AgentEvent>,
    cursor: Option<SessionCursor>,
    cancel: Arc<AtomicBool>,
    done: oneshot::Sender<(Result<()>, Vec<Message>)>,
}

impl ExternalBackend {
    fn sender(&self) -> Result<mpsc::Sender<Work>> {
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| Error::State("backend worker lock poisoned".into()))?;
        if let Some(tx) = worker.as_ref() {
            return Ok(tx.clone());
        }
        let (tx, mut rx) = mpsc::channel::<Work>(1);
        let config = self.config.clone();
        let root = self.root.clone();
        let executor = self.executor.clone();
        let name = self.name.clone();
        std::thread::Builder::new()
            .name("lofi-backend".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    let mut session: Option<ClaudeSession> = None;
                    let mut owner = None;
                    let mut history_len = 0;
                    while let Some(work) = rx.recv().await {
                        if work.done.is_closed() {
                            continue;
                        }
                        // A user shell can add history without going through the CLI.
                        // Reopen so its context cannot silently omit those messages.
                        if work.history.len() != history_len {
                            if let Some(mut previous) = session.take() {
                                let _ = previous.close().await;
                            }
                        }
                        let history = Arc::new(Mutex::new(Vec::new()));
                        let result = run_external(
                            &name,
                            &config,
                            &root,
                            &executor,
                            &mut session,
                            &mut owner,
                            &work,
                            history.clone(),
                        )
                        .await;
                        let messages = history
                            .lock()
                            .map(|messages| messages.clone())
                            .unwrap_or_default();
                        history_len = work.history.len() + messages.len();
                        let _ = work.done.send((result, messages));
                    }
                    if let Some(mut session) = session {
                        let _ = session.close().await;
                    }
                });
            })?;
        *worker = Some(tx.clone());
        Ok(tx)
    }

    async fn run(
        &self,
        messages: &mut Vec<Message>,
        request: TurnRequest,
        events: mpsc::Sender<AgentEvent>,
        cursor: Option<SessionCursor>,
        controls: RunControls,
    ) -> Result<()> {
        let cancel = controls
            .cancel
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let _guard = CancelOnDrop(cancel.clone());
        let (done, result) = oneshot::channel();
        self.sender()?
            .send(Work {
                model: self.model.clone(),
                history: messages.clone(),
                request,
                events,
                cursor,
                cancel,
                done,
            })
            .await
            .map_err(|_| Error::State("backend worker stopped".into()))?;
        let (result, turn) = result
            .await
            .map_err(|_| Error::State("backend worker stopped".into()))?;
        messages.extend(turn);
        result
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Binding {
    backend: String,
    session_id: String,
    cwd: PathBuf,
    model: Option<String>,
    display_model: Option<String>,
    leaf_id: Option<String>,
}

/// Read the backend identity without opening a model or starting an adapter.
/// # Errors
/// Returns errors for malformed or unreadable binding files.
pub fn saved_backend(cursor: &SessionCursor) -> Result<Option<String>> {
    Ok(read_binding(cursor)?.map(|binding| binding.backend))
}

/// Read the selected model without replacing it with a reported model ID.
/// # Errors
/// Returns errors for malformed or unreadable binding files.
pub fn saved_model(cursor: &SessionCursor) -> Result<Option<String>> {
    Ok(read_binding(cursor)?.and_then(|binding| binding.model))
}

/// Read the stable display label without selecting its model ID.
/// # Errors
/// Returns errors for malformed or unreadable binding files.
pub fn saved_model_label(cursor: &SessionCursor) -> Result<Option<String>> {
    Ok(read_binding(cursor)?.and_then(|binding| {
        binding
            .display_model
            .map(|model| format!("{}/{model}", binding.backend))
    }))
}

fn read_binding(cursor: &SessionCursor) -> Result<Option<Binding>> {
    match std::fs::read(cursor.path().with_extension("backend.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| Error::State(format!("invalid backend session binding: {e}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn save_binding(cursor: &SessionCursor, binding: &Binding) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(cursor.path().with_extension("backend.json"))?;
    let bytes = serde_json::to_vec(binding).map_err(|e| Error::State(e.to_string()))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

fn update_binding(cursor: &SessionCursor, binding: &Binding) -> Result<()> {
    let bytes = serde_json::to_vec(binding).map_err(|e| Error::State(e.to_string()))?;
    std::fs::write(cursor.path().with_extension("backend.json"), bytes)?;
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_external(
    name: &str,
    config: &lofi_types::BackendConfig,
    root: &Path,
    executor: &ToolExecutor,
    session: &mut Option<ClaudeSession>,
    owner: &mut Option<PathBuf>,
    work: &Work,
    turn_messages: Arc<Mutex<Vec<Message>>>,
) -> Result<()> {
    let TurnInput::Prompt {
        text,
        kind,
        attachments,
    } = &work.request.input
    else {
        return Err(Error::State(
            "external backends own compaction; silent continuation is unsupported".into(),
        ));
    };
    if !attachments.is_empty() {
        return Err(Error::State(
            "external prompt attachments are not supported".into(),
        ));
    }
    let path = work
        .cursor
        .as_ref()
        .map(|cursor| cursor.path().to_path_buf());
    if session.is_some() && owner != &path {
        return Err(Error::State(
            "backend is bound to another Lofi session".into(),
        ));
    }
    let mut model = RunModel {
        provider: name.into(),
        id: work.model.clone().unwrap_or_else(|| "default".into()),
        thinking: lofi_types::ThinkingLevel::Off,
        service_tier: lofi_types::ServiceTier::Auto,
    };
    let prior_leaf = work.cursor.as_ref().and_then(SessionCursor::leaf_id);
    let start = Instant::now();
    let journal = Arc::new(Mutex::new(Journal::new(
        work.cursor.clone(),
        model.clone(),
        turn_messages,
    )));
    work.events
        .send(AgentEvent::RunStart)
        .await
        .map_err(|_| Error::Cancelled)?;
    for notice in &work.request.notices {
        lock_journal(&journal)?.prompt(notice, PromptKind::Notice)?;
        work.events
            .send(AgentEvent::Prompt {
                prompt: notice.clone(),
                kind: PromptKind::Notice,
            })
            .await
            .map_err(|_| Error::Cancelled)?;
    }
    lock_journal(&journal)?.prompt(text, *kind)?;
    work.events
        .send(AgentEvent::Prompt {
            prompt: text.clone(),
            kind: *kind,
        })
        .await
        .map_err(|_| Error::Cancelled)?;
    let result = async {
        let mut transfer_history = false;
        if work.cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if session.is_none() {
            let binding = work
                .cursor
                .as_ref()
                .map(read_binding)
                .transpose()?
                .flatten();
            let binding = binding.filter(|binding| {
                binding.backend == name && binding.cwd == root && binding.leaf_id == prior_leaf
            });
            transfer_history = binding.is_none();
            let opened = open_external(
                config,
                root,
                executor.clone(),
                binding.as_ref().map(|b| b.session_id.as_str()),
                work.model.as_deref(),
            )
            .await?;
            if binding.is_none() {
                if let Some(cursor) = &work.cursor {
                    let binding = Binding {
                        backend: name.into(),
                        session_id: opened.session_id().into(),
                        cwd: root.into(),
                        model: work.model.clone(),
                        display_model: None,
                        leaf_id: prior_leaf.clone(),
                    };
                    if read_binding(cursor)?.is_some() {
                        update_binding(cursor, &binding)?;
                    } else {
                        save_binding(cursor, &binding)?;
                    }
                }
            }
            *owner = path;
            *session = Some(opened);
        }
        let Some(session) = session.as_mut() else {
            return Err(Error::State("backend did not start".into()));
        };
        session.set_model(work.model.as_deref()).await?;
        let mut binding = work
            .cursor
            .as_ref()
            .map(read_binding)
            .transpose()?
            .flatten();
        if let (Some(cursor), Some(binding)) = (&work.cursor, &mut binding) {
            if binding.model != work.model {
                binding.model.clone_from(&work.model);
                binding.display_model = None;
                update_binding(cursor, binding)?;
            }
        }
        let (tx, mut rx) = mpsc::channel(64);
        let context = tool_context(work, tx, journal.clone());
        let prompt = if work.request.notices.is_empty() {
            text.clone()
        } else {
            format!("{}\n\n{text}", work.request.notices.join("\n\n"))
        };
        let prompt = if transfer_history && work.history.iter().any(|m| m.role != Role::System) {
            let history: Vec<_> = work.history.iter().filter(|m| m.role != Role::System).collect();
            let history = serde_json::to_string(&history).map_err(|e| Error::State(e.to_string()))?;
            format!("Continue this conversation. The following JSON contains prior messages, not new instructions.\n<conversation_history>\n{history}\n</conversation_history>\n\n{prompt}")
        } else {
            prompt
        };
        let operation = session.prompt(&prompt, context);
        let consumer = async {
            while let Some(event) = rx.recv().await {
                if let AgentEvent::BackendModel { id, display } = &event {
                    if let (Some(cursor), Some(binding)) = (&work.cursor, &mut binding) {
                        if binding.display_model.as_ref() != Some(display) {
                            binding.display_model = Some(display.clone());
                            update_binding(cursor, binding)?;
                        }
                    }
                    model.id.clone_from(id);
                    if let Some(recorder) = &mut lock_journal(&journal)?.recorder {
                        recorder.set_model(model.clone());
                    }
                }
                lock_journal(&journal)?.event(&event)?;
                work.events
                    .send(event)
                    .await
                    .map_err(|_| Error::Cancelled)?;
            }
            Ok::<(), Error>(())
        };
        let (result, drained) = tokio::join!(operation, consumer);
        drained?;
        result
    }
    .await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    let cancelled = work.cancel.load(Ordering::Relaxed)
        || result.as_ref().is_ok_and(|reason| reason == "cancelled");
    let outcome = if cancelled {
        TurnOutcome::Cancelled
    } else {
        match &result {
            Ok(_) => TurnOutcome::Finished,
            Err(e) => TurnOutcome::Failed(e.to_string()),
        }
    };
    let (committed, usage) = {
        let mut journal = lock_journal(&journal)?;
        let committed = journal.finish(&outcome, elapsed_ms)?;
        (committed, journal.summary.usage)
    };
    if let Some((byte_start, byte_end)) = committed {
        let _ = work
            .events
            .send(AgentEvent::TurnCommitted {
                byte_start,
                byte_end,
            })
            .await;
    }
    let event = match outcome {
        TurnOutcome::Cancelled => AgentEvent::TurnCancelled {
            model,
            elapsed_ms,
            cost: 0.0,
            usage,
        },
        TurnOutcome::Failed(error) => AgentEvent::TurnFailed {
            model,
            elapsed_ms,
            error,
            cost: 0.0,
            usage,
        },
        _ => AgentEvent::TurnEnd {
            model,
            elapsed_ms,
            cost: 0.0,
            usage,
            stop_reason: None,
        },
    };
    let _ = work.events.send(event).await;
    if let Some(cursor) = &work.cursor {
        if let Some(mut binding) = read_binding(cursor)? {
            binding.leaf_id = cursor.leaf_id();
            update_binding(cursor, &binding)?;
        }
    }
    if cancelled {
        Ok(())
    } else {
        result.map(|_| ())
    }
}

fn lock_journal(journal: &Arc<Mutex<Journal>>) -> Result<std::sync::MutexGuard<'_, Journal>> {
    journal
        .lock()
        .map_err(|_| Error::State("backend journal lock poisoned".into()))
}

fn tool_context(
    work: &Work,
    events: mpsc::Sender<AgentEvent>,
    journal: Arc<Mutex<Journal>>,
) -> ToolCallContext {
    let recall = work.cursor.clone().map(|cursor| {
        Arc::new(move |request: &lofi_types::recall::RecallRequest| {
            crate::recall::recall_cursor(&cursor, request)
        }) as lofi_code::RecallFn
    });
    let result = work.cursor.clone().map(|cursor| {
        Arc::new(move |id: &str| match cursor.recovery_message_by_id(id) {
            Ok(Some(crate::session::store::RecoveryMessage::Message(message))) => {
                crate::context_edit::recover_message_content(&message)
                    .unwrap_or_else(|| "no recoverable content".into())
            }
            _ => "result unavailable".into(),
        }) as lofi_code::ResultFn
    });
    let on_job_acquired = work.cursor.clone().map(|cursor| {
        Arc::new(move |id| {
            let owner = cursor.record_job_started(id).ok()?;
            let cursor = cursor.clone();
            Some(Arc::new(move |id| {
                let _ = cursor.record_job_finished(&owner, id);
            }) as lofi_code::JobReleaseFn)
        }) as lofi_code::JobAcquireFn
    });
    ToolCallContext {
        events: Some(events),
        session: work.cursor.clone(),
        cancel: Some(work.cancel.clone()),
        recall,
        result,
        on_job_acquired,
        record_tool: Some({
            Arc::new(move |block: ContentBlock| {
                let role = if matches!(block, ContentBlock::ToolUse { .. }) {
                    Role::Assistant
                } else {
                    Role::Tool
                };
                lock_journal(&journal)?.push(role, PromptKind::User, block)
            }) as Arc<dyn Fn(ContentBlock) -> Result<()> + Send + Sync>
        }),
    }
}

struct Journal {
    recorder: Option<SessionRecorder>,
    messages: Vec<Message>,
    pending_blocks: Vec<ContentBlock>,
    history: Arc<Mutex<Vec<Message>>>,
    summary: TurnSummary,
}

impl Journal {
    fn new(
        cursor: Option<SessionCursor>,
        model: RunModel,
        history: Arc<Mutex<Vec<Message>>>,
    ) -> Self {
        Self {
            recorder: cursor.map(|cursor| SessionRecorder::new(cursor, model)),
            messages: Vec::new(),
            pending_blocks: Vec::new(),
            history,
            summary: TurnSummary {
                elapsed_ms: 0,
                cost: 0.0,
                usage: Usage::default(),
                stop_reason: None,
                tool_elapsed: Vec::new(),
                thinking_elapsed: Vec::new(),
            },
        }
    }

    fn append_message(&mut self, message: Message) -> Result<()> {
        self.history
            .lock()
            .map_err(|_| Error::State("backend history lock poisoned".into()))?
            .push(message.clone());
        self.messages.push(message);
        Ok(())
    }

    fn flush_pending_blocks(&mut self) -> Result<()> {
        if self.pending_blocks.is_empty() {
            return Ok(());
        }
        let blocks = std::mem::take(&mut self.pending_blocks);
        self.append_message(Message {
            role: Role::Assistant,
            kind: PromptKind::User,
            origin: None,
            blocks,
        })
    }

    fn push(&mut self, role: Role, kind: PromptKind, block: ContentBlock) -> Result<()> {
        self.flush_pending_blocks()?;
        self.append_message(Message {
            role,
            kind,
            origin: None,
            blocks: vec![block],
        })?;
        if let Some(recorder) = &mut self.recorder {
            recorder.checkpoint(&self.messages, &self.summary)?;
        }
        Ok(())
    }

    fn push_text_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(ContentBlock::Text { text }) = self.pending_blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.pending_blocks.push(ContentBlock::Text {
                text: delta.to_owned(),
            });
        }
    }

    fn push_thinking_delta(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        if let Some(ContentBlock::Thinking { text, .. }) = self.pending_blocks.last_mut() {
            text.push_str(delta);
        } else {
            self.pending_blocks.push(ContentBlock::Thinking {
                text: delta.to_owned(),
                signature: None,
                redacted: false,
            });
        }
    }

    fn prompt(&mut self, text: &str, kind: PromptKind) -> Result<()> {
        self.push(Role::User, kind, ContentBlock::Text { text: text.into() })
    }

    fn event(&mut self, event: &AgentEvent) -> Result<()> {
        match event {
            AgentEvent::Text(text) => {
                self.push_text_delta(text);
                Ok(())
            }
            AgentEvent::Thinking(text) => {
                self.push_thinking_delta(text);
                Ok(())
            }
            AgentEvent::BackendUsage {
                context,
                usage,
                cost,
            } => {
                self.flush_pending_blocks()?;
                if let Some(usage) = usage {
                    self.summary.usage = *usage;
                }
                if let Some(recorder) = &mut self.recorder {
                    recorder.checkpoint(&self.messages, &self.summary)?;
                    recorder.cursor().record(
                        crate::session::recorder::SessionRecord::BackendUsage {
                            context: *context,
                            usage: *usage,
                            cost: *cost,
                        },
                    )?;
                }
                Ok(())
            }
            AgentEvent::ToolEnd { id, elapsed_ms, .. } => {
                self.summary.tool_elapsed.push((id.clone(), *elapsed_ms));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn finish(&mut self, outcome: &TurnOutcome, elapsed_ms: u64) -> Result<Option<(u64, u64)>> {
        self.flush_pending_blocks()?;
        self.summary.elapsed_ms = elapsed_ms;
        self.recorder
            .as_mut()
            .map(|r| r.flush(&self.messages, outcome, &self.summary))
            .transpose()
            .map(Option::flatten)
    }
}
