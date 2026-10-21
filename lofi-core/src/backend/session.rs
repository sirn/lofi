use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use lofi_error::{Error, Result};
use lofi_types::{ContentBlock, Message, ModelChoice, Role, RunModel};
use tokio::sync::{mpsc, oneshot};

use super::claude::ClaudeSession;
use super::claude_engine::{reply, run_exchange, ClaudeEngine};
use super::contract::{Backend, BackendRef, Completion, CompletionRequest, ModelSelection};
use crate::agent::turn_session::{Exchange, ExchangeDriver, ExchangeFailure, TurnIo, TurnPolicy};
use crate::agent::ToolExecutor;
use crate::lifecycle::{lineage_catch_up, CatchUp};
use crate::session::store::SessionCursor;

/// Model choices of one CLI backend, in picker order: the configured model
/// first, then the configured list, then "default" when nothing is named.
#[must_use]
pub fn configured_models(
    name: &str,
    config: &lofi_types::BackendConfig,
    selected: Option<&str>,
) -> Vec<ModelChoice> {
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
        .map(|id| ModelChoice {
            provider: name.into(),
            name: id.clone(),
            id,
            thinking_levels: super::claude::effort_levels(),
            service_tiers: Vec::new(),
            supports_image: true,
            // Learned from the child at turn end; the choice reads the cell.
            context_window: None,
        })
        .collect()
}

#[derive(Clone)]
pub struct ExternalBackend {
    pub(super) name: String,
    pub(super) model: Option<String>,
    pub(super) config: lofi_types::BackendConfig,
    pub(super) root: PathBuf,
    pub(super) system_prompt: String,
    pub(super) executor: ToolExecutor,
    pub(super) thinking: lofi_types::ThinkingLevel,
    worker: Arc<Mutex<Option<mpsc::Sender<Work>>>>,
    /// The context window the CLI reported for the current model. Shared by
    /// every clone, so the picker and the gauge learn it.
    pub(super) context_window: Arc<Mutex<Option<u64>>>,
    /// The selection the live worker serves. The subagent hook is installed
    /// once, when the worker starts, so a child forks from this cell instead
    /// of the fields its template was copied with: a model switch reuses the
    /// worker and must reach the subagents it spawns.
    live: Arc<Mutex<(Option<String>, lofi_types::ThinkingLevel)>>,
    /// The compaction config, so this transport stops at the same fill the
    /// provider path does. Lofi owns the cap on this transport too, so the child
    /// is never asked to decide when the context is full.
    pub(super) compaction: lofi_types::CompactionConfig,
    /// The same retry ladder the native path applies to a failed request.
    pub(super) retry: crate::retry::RetryPolicy,
}

impl ExternalBackend {
    /// Configure a CLI backend without starting it or reading credentials.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn configure(
        name: String,
        config: lofi_types::BackendConfig,
        root: PathBuf,
        executor: ToolExecutor,
        model: Option<String>,
        thinking: lofi_types::ThinkingLevel,
        system_prompt: String,
        compaction: lofi_types::CompactionConfig,
    ) -> Self {
        Self {
            live: Arc::new(Mutex::new((model.clone(), thinking.clone()))),
            // None means the backend default: the caller resolved the query.
            model,
            name,
            config,
            root,
            system_prompt,
            executor,
            thinking,
            worker: Arc::new(Mutex::new(None)),
            context_window: Arc::new(Mutex::new(None)),
            compaction,
            retry: crate::retry::RetryPolicy::default(),
        }
    }

    /// Apply the session retry ladder. The factory holds the config that
    /// declares it; the backend only carries the resolved policy.
    #[must_use]
    pub(crate) fn with_retry(mut self, retry: crate::retry::RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// A copy that opens its own child. The worker cell holds the sender of
    /// the thread it started, so sharing it would keep another thread's
    /// channel open, and a running child keeps the system prompt and the
    /// executor it was opened with. The selection cell is per thread too:
    /// each worker writes the model it serves, and a copy must not inherit
    /// another thread's.
    fn detached(&self) -> Self {
        Self {
            worker: Arc::new(Mutex::new(None)),
            live: Arc::new(Mutex::new((self.model.clone(), self.thinking.clone()))),
            ..self.clone()
        }
    }

    /// The template the worker's executor carries. It opens its own child and
    /// its own turn slot, but reads the selection cell of the worker it was
    /// installed in, which survives a model switch.
    fn worker_template(&self) -> Self {
        Self {
            worker: Arc::new(Mutex::new(None)),
            ..self.clone()
        }
    }

    /// The fill above which the turn stops and the UI compacts, on the rule
    /// the provider path applies. A turn decides it before it runs: the window
    /// the child reported, or the one the config declares before the first
    /// turn.
    fn hard_compact_threshold(&self) -> Option<u64> {
        self.context_window()
            .and_then(|window| self.compaction.hard_threshold(window))
    }

    /// The context window the child reported, if any turn has finished.
    fn learned_window(&self) -> Option<u64> {
        self.context_window.lock().ok().and_then(|cell| *cell)
    }

    /// The model and effort the live worker serves, falling back to the
    /// configured pair when the cell is unreadable.
    fn learned_selection(&self) -> (Option<String>, lofi_types::ThinkingLevel) {
        self.live.lock().map_or_else(
            |_| (self.model.clone(), self.thinking.clone()),
            |cell| cell.clone(),
        )
    }

    /// Start the worker thread, or hand back the one already running.
    pub(super) fn open_worker(&self) -> Result<mpsc::Sender<Work>> {
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
        // The bridge reports lofi.subagent availability from the hook it
        // carries, so install the run-management hook here where the backend
        // exists.
        let executor = self
            .executor
            .clone()
            .with_subagent_hook(self.subagent_hook());
        let name = self.name.clone();
        // The child's system prompt comes from the backend, not from the
        // executor: a fork carries the role prompt its subagent was given.
        let system_prompt = self.system_prompt.clone();
        let live = self.live.clone();
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
                    let mut effort = lofi_types::ThinkingLevel::Off;
                    while let Some(work) = rx.recv().await {
                        if work.done.is_closed() {
                            continue;
                        }
                        // Reuse the child only when it stays in the same
                        // session and effort, and only when the CLI context is
                        // still a prefix of the lineage. A rewritten history
                        // (/compact, a new system prompt) or another session
                        // makes the child stale. A user shell does not: its
                        // output travels as a delta.
                        let path = work
                            .cursor
                            .as_ref()
                            .map(|cursor| cursor.path().to_path_buf());
                        let sync = match exchange_catch_up(&name, &root, &work) {
                            Ok(sync) => sync,
                            Err(error) => {
                                let _ = work.done.send(reply(
                                    Err(ExchangeFailure::failed(error)),
                                    &work.copy,
                                    Facts::default(),
                                    &mut session,
                                    &work.reported,
                                ));
                                continue;
                            }
                        };
                        // Publish the selection this turn runs with, so a
                        // subagent started by it forks on the same model even
                        // when the backend that sent the work was replaced.
                        if let Ok(mut cell) = live.lock() {
                            *cell = (work.model.clone(), work.effort.clone());
                        }
                        let fresh = child_is_fresh(
                            &work,
                            sync.as_ref(),
                            path.as_deref(),
                            owner.as_deref(),
                            history_len,
                        );
                        // A cancelled or failed turn closes the child; it
                        // cannot take another prompt, so the next turn must
                        // reopen the session from the checkpoint.
                        let live = session.as_ref().is_none_or(|s| !s.is_closed());
                        if session.is_some() && (!fresh || !live || work.effort != effort) {
                            if let Some(mut previous) = session.take() {
                                let _ = previous.close().await;
                            }
                            owner = None;
                        }
                        effort = work.effort.clone();
                        if let Ok(mut copy) = work.copy.lock() {
                            copy.clone_from(&work.history);
                        }
                        let out = run_exchange(
                            &name,
                            &config,
                            &root,
                            &executor,
                            &system_prompt,
                            &mut session,
                            &mut owner,
                            &work,
                            sync.as_ref(),
                        )
                        .await;
                        // The driver dropped mid-exchange: the child took output
                        // no transcript will hold, so a checkpoint that named
                        // this turn's leaf would gate the next resume on a delta
                        // the disk lacks.
                        if work.abandoned.load(Ordering::Acquire) {
                            abandon_exchange(&work, &mut session, &mut owner).await;
                        }
                        history_len = out.messages.len();
                        let _ = work.done.send(out);
                    }
                    if let Some(mut session) = session {
                        let _ = session.close().await;
                    }
                });
            })?;
        *worker = Some(tx.clone());
        Ok(tx)
    }
}

/// Drop what a turn that lost its driver left behind: the checkpoint names a
/// leaf no transcript holds, and the child carries the context it built.
async fn abandon_exchange(
    work: &Work,
    session: &mut Option<ClaudeSession>,
    owner: &mut Option<PathBuf>,
) {
    if let Some(cursor) = work.cursor.as_ref() {
        remove_checkpoint(cursor);
    }
    if let Some(mut previous) = session.take() {
        let _ = previous.close().await;
    }
    *owner = None;
}

pub(super) async fn open_external(
    config: &lofi_types::BackendConfig,
    root: &Path,
    executor: ToolExecutor,
    resume: Option<&str>,
    selected_model: Option<&str>,
    effort: lofi_types::ThinkingLevel,
    system_prompt: &str,
) -> Result<ClaudeSession> {
    let executable = std::env::current_exe()?;
    match config {
        lofi_types::BackendConfig::ClaudeCode { command, .. } => {
            ClaudeSession::open(
                super::claude::Launch {
                    command,
                    model: selected_model,
                    effort,
                    resume,
                    system_prompt,
                },
                root,
                &executable,
                executor,
            )
            .await
        }
    }
}

/// The catch-up the exchange's child needs.
///
/// Read after the previous turn settled: a checkpoint captured earlier can
/// name a leaf it already moved past, and the delta would resend the child its
/// own reply. Only the turn's first exchange compares the child with the
/// transcript; a settle is what moves the leaf, so every later exchange of the
/// same turn sees a child that is ahead of it by design and must continue it
/// rather than resync it.
fn exchange_catch_up(name: &str, root: &Path, work: &Work) -> Result<Option<CatchUp>> {
    let Some(cursor) = work.cursor.as_ref().filter(|_| work.first) else {
        return Ok(None);
    };
    catch_up(name, root, cursor, work.io.frontier().frontier.as_deref()).map(Some)
}

/// Whether the live child still holds this exchange's context.
fn child_is_fresh(
    work: &Work,
    sync: Option<&CatchUp>,
    path: Option<&Path>,
    owner: Option<&Path>,
    history_len: usize,
) -> bool {
    if !work.first {
        // A later exchange of the same turn continues the child the previous
        // one left: the transcript cannot show that child's own work.
        return path == owner;
    }
    match sync {
        Some(CatchUp::Diverged) => false,
        Some(_) => path == owner,
        // No cursor: the history length is the only signal.
        None => work.history.len() == history_len,
    }
}

/// One exchange handed to the worker thread.
pub(super) struct Work {
    pub(super) model: Option<String>,
    pub(super) effort: lofi_types::ThinkingLevel,
    /// The caller's history at the start of this exchange.
    pub(super) history: Vec<Message>,
    /// The copy the worker appends to, shared with the tool recorder so a
    /// block the child's tool produced lands in the same order.
    pub(super) copy: Arc<Mutex<Vec<Message>>>,
    pub(super) io: TurnIo,
    pub(super) cursor: Option<SessionCursor>,
    /// The identity the child reported for itself, so the terminal marker and
    /// the next resume carry the same label.
    pub(super) reported: Arc<Mutex<Option<RunModel>>>,
    /// The fold of this turn's child events. The tool recorder writes into the
    /// same one, so a block the child's tool produced keeps its order.
    pub(super) fold: Arc<Mutex<Fold>>,
    /// First exchange of its turn: the child's spend counter restarts.
    pub(super) first: bool,
    /// The driver dropped without settling. This thread writes the binding, so
    /// the checkpoint is removed here and not on the caller's.
    pub(super) abandoned: Arc<AtomicBool>,
    pub(super) done: oneshot::Sender<Reply>,
}

/// What one exchange produced.
pub(super) struct Reply {
    pub(super) exchange: std::result::Result<Exchange, ExchangeFailure>,
    /// The history the exchange ended with: the caller's history followed by
    /// what this exchange appended, in order.
    pub(super) messages: Vec<Message>,
    pub(super) facts: Facts,
    pub(super) window: Option<u64>,
    pub(super) reported: Option<RunModel>,
}

/// What the settle step needs to know about the child.
#[derive(Default, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Facts {
    /// This turn wrote the binding.
    pub(super) created: bool,
    /// The child reported init, so the session exists.
    pub(super) initialized: bool,
    /// The child took a prompt this turn and may hold context the transcript
    /// does not.
    pub(super) child_holds_turn: bool,
    /// The exchange failed: the child's state may be ahead of the transcript,
    /// so the binding may not name this turn's leaf.
    pub(super) failed: bool,
}

impl Facts {
    /// Keep what an earlier exchange of the turn learned. A later exchange
    /// that returns early reports nothing, and its defaults must not erase a
    /// prompt the turn already wrote.
    pub(super) fn merge(&mut self, later: Self) {
        self.created |= later.created;
        self.initialized |= later.initialized;
        self.child_holds_turn |= later.child_holds_turn;
        self.failed |= later.failed;
    }
}

/// Compare the saved backend checkpoint with the session lineage.
///
/// The worker runs this per exchange, not the caller: an abandoned turn can
/// still be settling when the next one is enqueued, and a checkpoint read
/// before that settlement would name a leaf the turn already moved past.
fn catch_up(
    name: &str,
    root: &Path,
    cursor: &SessionCursor,
    frontier: Option<&str>,
) -> Result<CatchUp> {
    let binding = read_binding(cursor)?;
    let Some(binding) = binding.filter(|b| b.backend == name && b.cwd == root) else {
        return Ok(CatchUp::Diverged);
    };
    lineage_catch_up(cursor, frontier, binding.leaf_id.as_deref())
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Binding {
    pub(super) backend: String,
    pub(super) session_id: String,
    pub(super) cwd: PathBuf,
    pub(super) model: Option<String>,
    pub(super) leaf_id: Option<String>,
}

fn binding_path(cursor: &SessionCursor) -> PathBuf {
    cursor.path().with_extension("backend.json")
}

/// Drop a checkpoint the child cannot be resumed from. The file is kept when
/// it already names the current leaf: a failed directory sync can still leave
/// it correct, and removing it would resend the whole history.
pub(super) fn drop_checkpoint(cursor: &SessionCursor) {
    let leaf = cursor.leaf_id();
    match read_binding(cursor) {
        Ok(Some(binding)) if binding.leaf_id == leaf => {}
        Ok(_) | Err(_) => remove_checkpoint(cursor),
    }
}

pub(super) fn remove_checkpoint(cursor: &SessionCursor) {
    let path = binding_path(cursor);
    if !path.exists() {
        return;
    }
    if let Err(error) = std::fs::remove_file(&path) {
        tracing::warn!(error = %error, "backend checkpoint removal failed");
    }
}

pub(super) fn read_binding(cursor: &SessionCursor) -> Result<Option<Binding>> {
    match std::fs::read(binding_path(cursor)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn write_binding(cursor: &SessionCursor, binding: &Binding) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = binding_path(cursor);
    let dir = path
        .parent()
        .ok_or_else(|| Error::State("session path has no parent".into()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("backend.json")
    ));
    let bytes = serde_json::to_vec(binding).map_err(|e| Error::State(e.to_string()))?;
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    crate::state::ensure_private_file(&path)?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// The frame a silent continuation sends. A provider round continues from the
/// same history; a stdio child needs a message to produce output.
pub(super) const CONTINUE_PROMPT: &str = "Continue the task from where you left off.";

/// Render a history for a continuation frame: each image becomes a pointer,
/// and its bytes are appended to images in reference order. Base64 never
/// enters model-visible text.
pub(super) fn render_transfer(
    history: &[Message],
    images: &mut Vec<ContentBlock>,
) -> Result<String> {
    let mut budget = crate::agent::MAX_REQUEST_IMAGE_BYTES;
    let mut rendered = Vec::with_capacity(history.len());
    for message in history {
        let mut blocks = Vec::with_capacity(message.blocks.len());
        for block in &message.blocks {
            if let ContentBlock::Image { bytes, media_type } = block {
                let size = bytes.len() as u64 * 4 / 3 + 4;
                if size <= budget {
                    budget -= size;
                    images.push(ContentBlock::Image {
                        bytes: bytes.clone(),
                        media_type: media_type.clone(),
                    });
                    blocks.push(ContentBlock::Text {
                        text: format!("[image {}]", images.len()),
                    });
                } else {
                    blocks.push(ContentBlock::Text {
                        text: "[image omitted: transfer budget]".into(),
                    });
                }
            } else {
                blocks.push(block.clone());
            }
        }
        rendered.push(Message {
            blocks,
            ..message.clone()
        });
    }
    serde_json::to_string(&rendered).map_err(|e| Error::State(e.to_string()))
}

/// The identity the terminal marker carries before the child reports one.
pub(super) fn run_model_for(
    name: &str,
    model: Option<&str>,
    effort: &lofi_types::ThinkingLevel,
) -> RunModel {
    let selected = model.unwrap_or("default").to_owned();
    RunModel {
        provider: name.into(),
        id: selected.clone(),
        selected,
        display: None,
        thinking: effort.clone(),
        service_tier: lofi_types::ServiceTier::Auto,
    }
}

#[async_trait]
impl Backend for ExternalBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn selection(&self) -> ModelSelection {
        ModelSelection {
            model: self.model.clone().unwrap_or_else(|| "default".to_owned()),
            thinking: self.thinking.clone(),
            service_tier: lofi_types::ServiceTier::Auto,
        }
    }

    fn choice(&self) -> ModelChoice {
        let current = self.selection().model;
        let mut choice = configured_models(&self.name, &self.config, self.model.as_deref())
            .into_iter()
            .find(|choice| choice.id == current)
            .unwrap_or(ModelChoice {
                provider: self.name.clone(),
                id: current.clone(),
                name: current,
                thinking_levels: super::claude::effort_levels(),
                service_tiers: Vec::new(),
                supports_image: true,
                context_window: None,
            });
        // The child reports the window it runs; before the first turn the
        // config declares one, exactly as a provider model does.
        choice.context_window = self
            .learned_window()
            .or_else(|| self.config.context_window());
        choice
    }

    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn tool_executor(&self) -> ToolExecutor {
        self.executor.clone()
    }

    fn with_selection(&self, query: &str) -> Result<BackendRef> {
        let parsed = super::resolve::parse_query(query)?;
        if parsed.provider != self.name {
            return Err(Error::Config(format!(
                "unknown backend \"{}\"; this session runs on \"{}\"",
                parsed.provider, self.name
            )));
        }
        if !self.config.models().iter().any(|id| id == &parsed.model)
            && self.config.model() != Some(parsed.model.as_str())
            && parsed.model != "default"
        {
            let models = self.config.models().join(", ");
            return Err(Error::Config(format!(
                "no model \"{}\" for backend \"{}\"; configured: {models}",
                parsed.model, self.name
            )));
        }
        if parsed.tier != lofi_types::ServiceTier::Auto {
            return Err(Error::Config(format!(
                "service tier \"{}\" is not supported by \"{}\"",
                parsed.tier.as_str(),
                self.name
            )));
        }
        if let Some(level) = parsed.level.clone() {
            let allowed = self.choice().thinking_levels;
            if level != lofi_types::ThinkingLevel::Off && !allowed.contains(&level) {
                let names: Vec<&str> = allowed
                    .iter()
                    .map(lofi_types::ThinkingLevel::as_str)
                    .collect();
                return Err(Error::Config(format!(
                    "thinking level \"{}\" is not supported by \"{}\"; allowed: {}",
                    level.as_str(),
                    self.name,
                    names.join(", ")
                )));
            }
        }
        let mut next = self.clone();
        let model = (parsed.model != "default").then_some(parsed.model);
        // The learned window belongs to the model that reported it.
        if model != self.model {
            next.context_window = Arc::new(Mutex::new(None));
        }
        next.model = model;
        // A query without a level keeps the session's effort, as it does on
        // the native path.
        next.thinking = parsed
            .level
            .clone()
            .unwrap_or_else(|| self.thinking.clone());
        // The worker writes the cell when it takes this turn: a switch made
        // while another turn runs must not change the model that turn uses.
        Ok(Arc::new(next))
    }

    fn with_tool_executor(&self, executor: ToolExecutor) -> BackendRef {
        // A model switch keeps the live child and changes its model in place,
        // so the worker survives a new executor.
        let mut next = self.clone();
        next.executor = executor;
        Arc::new(next)
    }

    fn with_system_prompt(&self, prompt: String) -> BackendRef {
        // A live child keeps the prompt it was opened with, so the next turn
        // must open its own instead of reusing the running worker.
        let mut next = self.detached();
        next.system_prompt = prompt;
        Arc::new(next)
    }

    fn subagents(&self) -> crate::SubagentRegistry {
        // The CLI backend keeps its exec bridge, which carries the same
        // registry an HTTP backend exposes.
        self.executor.subagents()
    }

    fn subagent_hook(&self) -> Option<lofi_code::SubagentFn> {
        match self.executor.subagent_support() {
            // A child gets its own inbox from the spawner; a session builds
            // the run-management hook, so its children are ordinary backends.
            None => self.executor.subagent_hook(),
            // The hook travels inside the worker thread's executor. A template
            // that shared this backend's worker cell would keep that thread's
            // own sender alive, so the thread would never see its channel
            // close and its child would never stop.
            Some(support) => Some(crate::agent::backend_hook(
                Arc::new(self.worker_template()),
                support,
                self.executor.subagents(),
            )),
        }
    }

    fn fork(&self, executor: ToolExecutor) -> BackendRef {
        // The subagent hook is installed once, when the worker starts, so its
        // template outlives a model switch: take the selection the worker
        // serves, not the one the template was copied with.
        let (model, thinking) = self.learned_selection();
        Arc::new(Self {
            name: self.name.clone(),
            model: model.clone(),
            config: self.config.clone(),
            root: self.root.clone(),
            system_prompt: self.system_prompt.clone(),
            executor,
            thinking: thinking.clone(),
            // A child owns its own child process and its own turn slot, and
            // the window it reports belongs to its model, not the parent's.
            worker: Arc::new(Mutex::new(None)),
            context_window: Arc::new(Mutex::new(None)),
            live: Arc::new(Mutex::new((model, thinking))),
            compaction: self.compaction.clone(),
            retry: self.retry,
        })
    }

    async fn run_turn(
        &self,
        messages: &mut Vec<Message>,
        request: crate::agent::TurnRequest,
        events: mpsc::Sender<crate::agent::AgentEvent>,
        cursor: Option<&SessionCursor>,
        controls: crate::agent::RunControls,
    ) -> Result<crate::session::recorder::TurnOutcome> {
        crate::agent::turn_session::run(self, messages, request, events, cursor, controls).await
    }

    fn open_turn(&self) -> Result<Box<dyn ExchangeDriver>> {
        Ok(Box::new(ClaudeEngine::open(self.clone())?))
    }

    fn turn_policy(&self) -> TurnPolicy {
        TurnPolicy {
            auto_continue: lofi_types::AutoContinuePolicy::default(),
            retry: self.retry,
            hard_cap: self.hard_compact_threshold(),
        }
    }

    fn context_window(&self) -> Option<u64> {
        self.learned_window()
            .or_else(|| self.config.context_window())
    }

    async fn complete(&self, request: CompletionRequest) -> Result<Completion> {
        let lofi_types::BackendConfig::ClaudeCode { command, .. } = &self.config;
        super::oneshot::complete(
            command,
            &self.root,
            self.model.as_deref(),
            &self.thinking,
            &request,
        )
        .await
    }
}

/// The role a recorded block belongs to.
pub(super) fn block_role(block: &ContentBlock) -> Role {
    if matches!(block, ContentBlock::ToolUse { .. }) {
        Role::Assistant
    } else {
        Role::Tool
    }
}

/// The fold of a child's event stream into messages.
///
/// Text and thinking arrive as deltas and coalesce into blocks until the next
/// role boundary: one assistant message per CLI round, not one per token.
#[derive(Default)]
pub(super) struct Fold {
    pending: Vec<ContentBlock>,
}

impl Fold {
    pub(super) fn flush(&mut self, copy: &Arc<Mutex<Vec<Message>>>, io: &TurnIo) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let blocks = std::mem::take(&mut self.pending);
        Self::append(
            copy,
            io,
            Message {
                role: Role::Assistant,
                kind: lofi_types::PromptKind::User,
                origin: None,
                blocks,
            },
        )
    }

    fn append(copy: &Arc<Mutex<Vec<Message>>>, io: &TurnIo, message: Message) -> Result<()> {
        let history = {
            let mut history = copy
                .lock()
                .map_err(|_| Error::State("backend history lock poisoned".into()))?;
            history.push(message);
            history.clone()
        };
        // Best effort: the recorder keeps its counts, so the next commit
        // retries the same suffix. A byte this one wrote stays pending in the
        // turn and travels with that commit's range.
        if let Err(error) = io.record(&history) {
            tracing::warn!(error = %error, "backend checkpoint write failed");
        }
        Ok(())
    }

    pub(super) fn push(
        &mut self,
        copy: &Arc<Mutex<Vec<Message>>>,
        io: &TurnIo,
        role: Role,
        kind: lofi_types::PromptKind,
        block: ContentBlock,
    ) -> Result<()> {
        self.flush(copy, io)?;
        Self::append(
            copy,
            io,
            Message {
                role,
                kind,
                origin: None,
                blocks: vec![block],
            },
        )
    }

    pub(super) fn event(
        &mut self,
        copy: &Arc<Mutex<Vec<Message>>>,
        io: &TurnIo,
        event: &crate::agent::AgentEvent,
    ) -> Result<()> {
        match event {
            crate::agent::AgentEvent::Text(text) => {
                if text.is_empty() {
                    return Ok(());
                }
                if let Some(ContentBlock::Text { text: last }) = self.pending.last_mut() {
                    last.push_str(text);
                } else {
                    self.pending.push(ContentBlock::Text { text: text.clone() });
                }
                Ok(())
            }
            crate::agent::AgentEvent::Thinking(text) => {
                if text.is_empty() {
                    return Ok(());
                }
                if let Some(ContentBlock::Thinking { text: last, .. }) = self.pending.last_mut() {
                    last.push_str(text);
                } else {
                    self.pending.push(ContentBlock::Thinking {
                        text: text.clone(),
                        signature: None,
                        redacted: false,
                    });
                }
                Ok(())
            }
            // A usage or context report closes the open block first, so the
            // assistant message it describes is already in the transcript.
            crate::agent::AgentEvent::ContextReported { .. }
            | crate::agent::AgentEvent::RoundUsage { .. } => self.flush(copy, io),
            _ => Ok(()),
        }
    }
}
