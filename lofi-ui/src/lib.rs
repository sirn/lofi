use std::io::Write;
use std::path::PathBuf;

use lofi_core::AgentEvent;
use lofi_error::{Error, Result};
use lofi_types::{ServiceTier, ThinkingLevel};

mod cli;
mod print_summary;
pub mod tui;

pub use cli::run_cli;

#[derive(Debug, Clone)]
pub struct InteractiveOptions {
    pub root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub env: Vec<String>,
    pub continue_last: bool,
    pub resume: Option<String>,
    pub no_session: bool,
}

impl InteractiveOptions {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            config_path: None,
            model: None,
            backend: None,
            env: Vec::new(),
            continue_last: false,
            resume: None,
            no_session: false,
        }
    }

    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub fn with_envs<'a>(mut self, envs: impl IntoIterator<Item = &'a str>) -> Self {
        self.env = envs.into_iter().map(str::to_string).collect();
        self
    }

    #[must_use]
    pub fn with_continue(mut self) -> Self {
        self.continue_last = true;
        self
    }

    #[must_use]
    pub fn with_resume(mut self, id: impl Into<String>) -> Self {
        self.resume = Some(id.into());
        self
    }

    #[must_use]
    pub fn with_no_session(mut self) -> Self {
        self.no_session = true;
        self
    }
}

/// Where a print-mode run keeps its transcript.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PrintSession {
    /// Write no transcript; the default.
    #[default]
    None,
    /// Start a new session for this run.
    Fresh,
    /// Continue the most recent session of this workspace.
    Continue,
    /// Resume the session whose id starts with this prefix.
    Resume(String),
}

#[derive(Debug, Clone)]
pub struct PrintOptions {
    pub prompt: String,
    pub root: PathBuf,
    pub config_path: Option<PathBuf>,
    pub model: Option<String>,
    pub backend: Option<String>,
    pub env: Vec<String>,
    /// Where to write one JSON record of the run; `None` writes nothing.
    pub summary: Option<PathBuf>,
    /// Where the run keeps its transcript, so a killed run can be resumed.
    pub session: PrintSession,
}

impl PrintOptions {
    #[must_use]
    pub fn new(prompt: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            root: root.into(),
            config_path: None,
            model: None,
            backend: None,
            env: Vec::new(),
            summary: None,
            session: PrintSession::None,
        }
    }

    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub fn with_envs<'a>(mut self, envs: impl IntoIterator<Item = &'a str>) -> Self {
        self.env = envs.into_iter().map(str::to_string).collect();
        self
    }

    /// Write one JSON record of the run to `path` when it ends.
    #[must_use]
    pub fn with_summary(mut self, path: impl Into<PathBuf>) -> Self {
        self.summary = Some(path.into());
        self
    }

    /// Start a new session for this run.
    #[must_use]
    pub fn with_session(mut self) -> Self {
        self.session = PrintSession::Fresh;
        self
    }

    /// Continue the most recent session of this workspace.
    #[must_use]
    pub fn with_continue(mut self) -> Self {
        self.session = PrintSession::Continue;
        self
    }

    /// Resume the session whose id starts with `id`.
    #[must_use]
    pub fn with_resume(mut self, id: impl Into<String>) -> Self {
        self.session = PrintSession::Resume(id.into());
        self
    }
}

/// A missing-models outcome is *not* fatal: the TUI still launches so the
/// user can read transcripts, browse `/resume`, and quit. A hint explaining
/// how to configure a model is shown in the log, and submitting a prompt
/// re-surfaces that hint instead of running.
/// # Errors
/// Propagates [`Error`] from config/session resolution or the terminal
/// session. Model-resolution failures that reduce to "no active model" are
/// swallowed into the no-model TUI mode described above.
#[allow(clippy::too_many_lines)]
pub async fn run_interactive(opts: InteractiveOptions) -> Result<()> {
    let env = env_pairs(&opts.env)?;
    let session = resolve_session(&opts)?;
    // Restore the model, effort and tier the user last ran with unless
    // --model was given: the resumed session's final turn marker carries
    // them, so a fresh session falls through to the configured default.
    let restored = opts
        .model
        .is_none()
        .then(|| session.last_run_model())
        .flatten();
    let query = opts
        .model
        .clone()
        .or_else(|| restored.as_ref().map(lofi_types::RunModel::query));
    let started = match startup(&opts, query.as_deref(), &env).await {
        Ok(started) => Ok(started),
        // A restored model that no longer resolves must not stop a resume.
        Err(_) if restored.is_some() => startup(&opts, None, &env).await,
        Err(error) => Err(error),
    };
    let hint = match started {
        Ok(started) => started,
        Err(error) => return no_model_tui(&opts, error, session, env).await,
    };
    let Startup {
        backend,
        factory,
        compaction,
        theme,
    } = hint;
    let selection = backend.selection();
    let choice = backend.choice();
    // A resumed session shows the label its last turn recorded, so a backend
    // that reports a longer name (a CLI reports the context window it runs)
    // keeps the header the user left.
    let label = match restored.filter(|model| model.provider == backend.name()) {
        Some(model) => format!("{}/{}", backend.name(), model.display_or_selected()),
        None => format!("{}/{}", backend.name(), selection.model),
    };
    let ctx_limit = choice.context_window.filter(|&l| l > 0).unwrap_or(0);
    let system_prompt = backend.system_prompt().to_string();
    let switcher = tui::ModelSwitcher::new(factory).with_subagent_persistence(!opts.no_session);
    Box::pin(tui::run(
        Some(backend),
        label,
        selection.thinking,
        selection.service_tier,
        theme,
        session,
        None,
        ctx_limit,
        compaction,
        Some(switcher),
        system_prompt,
    ))
    .await
}

/// What one open backend needs from its config.
struct Startup {
    backend: lofi_core::backend::BackendRef,
    factory: lofi_core::backend::BackendFactory,
    compaction: lofi_types::CompactionConfig,
    theme: lofi_types::ThemeMode,
}

async fn startup(
    opts: &InteractiveOptions,
    query: Option<&str>,
    env: &[(String, String)],
) -> Result<Startup> {
    let path = opts
        .config_path
        .clone()
        .map_or_else(lofi_core::config_loader::user_config_path, Ok)?;
    let config = lofi_core::config_loader::load_backend_config(&path)?;
    let factory =
        lofi_core::backend::BackendFactory::new(config.clone(), &opts.root, env.to_vec())?
            .with_skills_dir(path.parent().map(|dir| dir.join("skills")))
            .with_subagent_persistence(!opts.no_session);
    let executor = factory
        .executor()?
        .with_skills_dir(path.parent().map(|dir| dir.join("skills")))
        .with_auto_mode_config(&config, env)
        .await?;
    let query = constrain(&config, opts, query)?;
    let backend = factory.open(query.as_deref(), executor).await?;
    Ok(Startup {
        backend,
        factory,
        compaction: config.compaction,
        theme: config.ui.theme,
    })
}

/// Honour --backend: the flag constrains the selection instead of silently
/// switching the provider a qualified query named.
fn constrain(
    config: &lofi_types::Config,
    opts: &InteractiveOptions,
    query: Option<&str>,
) -> Result<Option<String>> {
    let Some(name) = opts.backend.as_deref() else {
        return Ok(query.map(str::to_owned));
    };
    if let Some(query) = query {
        return match query.split_once('/') {
            Some((prefix, _)) if prefix == name => Ok(Some(query.to_owned())),
            _ => Err(Error::Config(format!(
                "model \"{query}\" conflicts with --backend \"{name}\""
            ))),
        };
    }
    // A backend that names its own default model leads with it.
    let model = config
        .backends
        .get(name)
        .and_then(|backend| {
            backend
                .model()
                .or_else(|| backend.models().first().map(String::as_str))
        })
        .unwrap_or("default");
    Ok(Some(format!("{name}/{model}")))
}

/// A missing-model launch is not fatal: the TUI still opens so the user can
/// read transcripts, browse /resume and quit.
async fn no_model_tui(
    opts: &InteractiveOptions,
    error: Error,
    session: tui::SessionConfig,
    env: Vec<(String, String)>,
) -> Result<()> {
    let Error::NoModels(hint) = error else {
        return Err(error);
    };
    let path = opts
        .config_path
        .clone()
        .map_or_else(lofi_core::config_loader::user_config_path, Ok)?;
    let config = lofi_core::config_loader::load_backend_config(&path)?;
    let factory = lofi_core::backend::BackendFactory::new(config.clone(), &opts.root, env.clone())?
        .with_skills_dir(path.parent().map(|dir| dir.join("skills")))
        .with_subagent_persistence(!opts.no_session);
    let executor = factory
        .executor()?
        .with_skills_dir(path.parent().map(|dir| dir.join("skills")))
        .with_auto_mode_config(&config, &env)
        .await?;
    let switcher = tui::ModelSwitcher::new(factory).with_tool_executor(executor);
    Box::pin(tui::run(
        None,
        "(no model)".to_string(),
        ThinkingLevel::Off,
        ServiceTier::Auto,
        config.ui.theme,
        session,
        Some(hint),
        0,
        config.compaction,
        Some(switcher),
        String::new(),
    ))
    .await
}

/// Parse the `-e`/`--env` specs into (name, value) pairs, resolving bare
/// names against the parent environment. A bare name that is unset, or an
/// empty name, is a config error surfaced before model resolution starts.
/// # Errors
/// Returns `Error::Config` from `config_loader::parse_env_spec`.
fn env_pairs(specs: &[String]) -> Result<Vec<(String, String)>> {
    specs
        .iter()
        .map(|spec| lofi_core::config_loader::parse_env_spec(spec))
        .collect()
}

/// Independent of model resolution so the no-model launch path still gets a
/// session (and thus `/resume`, transcript browsing, etc.).
/// # Errors
/// Returns `Error::State` when `--resume <id>` matches no session, or
/// propagates `Error::Io` from the store.
fn resolve_session(opts: &InteractiveOptions) -> Result<tui::SessionConfig> {
    if opts.no_session {
        return Ok(tui::SessionConfig::ephemeral(opts.root.clone()));
    }
    // The UI never opens a session store: `--resume`/`--continue` resolve
    // entirely inside core, returning a sink (which owns the store) plus the
    // selected cursor and snapshot.
    if let Some(id) = &opts.resume {
        let (sink, cursor, snapshot) =
            lofi_core::session::sink::SessionSink::resume_by_id(&opts.root, id)?;
        return Ok(tui::SessionConfig::resumed(
            sink,
            cursor,
            snapshot.index,
            snapshot.file_size,
            snapshot.history_start,
            snapshot.contiguous,
            opts.root.clone(),
        ));
    }
    if opts.continue_last {
        if let Some((sink, cursor, snapshot)) =
            lofi_core::session::sink::SessionSink::resume_last(&opts.root)?
        {
            return Ok(tui::SessionConfig::resumed(
                sink,
                cursor,
                snapshot.index,
                snapshot.file_size,
                snapshot.history_start,
                snapshot.contiguous,
                opts.root.clone(),
            ));
        }
    }
    Ok(tui::SessionConfig::fresh(
        lofi_core::session::sink::SessionSink::open(&opts.root)?,
        opts.root.clone(),
    ))
}

/// Drives one turn of the selected backend and writes its events to stdio.
/// - `AgentEvent::Text` deltas go to stdout;
/// - errors and tool markers go to stderr, so stdout stays pipeable.
/// # Errors
/// Propagates config, model resolution and turn errors. A run that fails
/// still writes its summary record.
pub async fn run_print(opts: PrintOptions) -> Result<()> {
    let started = std::time::Instant::now();
    // Open the record before the run: a path that cannot be written must
    // fail before the run costs anything.
    let mut file = match &opts.summary {
        Some(path) => Some(std::fs::File::create(path).map_err(|error| {
            Error::State(format!("open the run summary {}: {error}", path.display()))
        })?),
        None => None,
    };
    // The consumer owns the only view of the run's events, and a setup
    // failure has no events at all, so the record lives outside the turn.
    // Both run on this task, so a plain cell suffices.
    let summary = std::cell::RefCell::new(print_summary::PrintSummary::default());
    let result = run_print_turn(&opts, &summary).await;
    if let Some(file) = &mut file {
        let wall_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut record = summary.into_inner();
        // The record is written for a failed run too: its spend and stop
        // reason are what the caller most needs.
        if let Err(error) = &result {
            record.note_error(&error.to_string());
        }
        if let Err(error) = record.write(file, wall_ms) {
            // A write failure must not hide the run's own error.
            if result.is_ok() {
                return Err(error);
            }
            eprintln!("error: {error}");
        }
    }
    result
}

/// The transcript one print run writes, and the history it resumed.
struct PrintTranscript {
    /// Holds the store and the session file open for the whole run.
    _sink: lofi_core::session::sink::SessionSink,
    cursor: lofi_core::session::store::SessionCursor,
    /// Messages a resumed session already holds, oldest first.
    history: Vec<lofi_types::Message>,
}

impl PrintTranscript {
    fn cursor(&self) -> &lofi_core::session::store::SessionCursor {
        &self.cursor
    }

    fn id(&self) -> String {
        self.cursor.id()
    }

    /// Hand the resumed history to the turn exactly once.
    fn take_history(&mut self) -> Vec<lofi_types::Message> {
        std::mem::take(&mut self.history)
    }
}

/// Open the transcript a print run asked for. A run without one writes
/// nothing, the way every print run used to behave.
fn open_transcript(
    opts: &PrintOptions,
    model: &lofi_types::RunModel,
    system_prompt: &str,
) -> Result<Option<PrintTranscript>> {
    use lofi_core::session::{replay, sink::SessionSink};
    match &opts.session {
        PrintSession::None => Ok(None),
        PrintSession::Resume(id) => {
            let (sink, cursor, snapshot) = SessionSink::resume_by_id(&opts.root, id)?;
            let history =
                replay::history_from_index(&cursor, &snapshot.index, snapshot.history_start)?;
            Ok(Some(PrintTranscript {
                _sink: sink,
                cursor,
                history,
            }))
        }
        PrintSession::Continue => match SessionSink::resume_last(&opts.root)? {
            Some((sink, cursor, snapshot)) => {
                let history =
                    replay::history_from_index(&cursor, &snapshot.index, snapshot.history_start)?;
                Ok(Some(PrintTranscript {
                    _sink: sink,
                    cursor,
                    history,
                }))
            }
            // --continue with nothing to continue starts a session, the way
            // it starts the TUI on a fresh transcript.
            None => Ok(Some(fresh_transcript(&opts.root, model, system_prompt)?)),
        },
        PrintSession::Fresh => Ok(Some(fresh_transcript(&opts.root, model, system_prompt)?)),
    }
}

fn fresh_transcript(
    root: &std::path::Path,
    model: &lofi_types::RunModel,
    system_prompt: &str,
) -> Result<PrintTranscript> {
    let mut sink = lofi_core::session::sink::SessionSink::open(root)?;
    let cursor = sink.cursor_or_create(model, system_prompt)?;
    Ok(PrintTranscript {
        _sink: sink,
        cursor,
        history: Vec::new(),
    })
}

/// Runs one non-interactive turn and folds its events into `summary`.
#[allow(clippy::too_many_lines)]
async fn run_print_turn(
    opts: &PrintOptions,
    summary: &std::cell::RefCell<print_summary::PrintSummary>,
) -> Result<()> {
    let env = env_pairs(&opts.env)?;
    let path = opts
        .config_path
        .clone()
        .map_or_else(lofi_core::config_loader::user_config_path, Ok)?;
    let config = lofi_core::config_loader::load_backend_config(&path)?;
    // A subagent started by this run keeps no transcript of its own: the
    // parent's record carries its spend, and no one resumes a child.
    let factory = lofi_core::backend::BackendFactory::new(config.clone(), &opts.root, env.clone())?
        .with_skills_dir(path.parent().map(|dir| dir.join("skills")))
        .with_subagent_persistence(false);
    let executor = factory
        .executor()?
        .with_skills_dir(path.parent().map(|dir| dir.join("skills")))
        .with_auto_mode_config(&config, &env)
        .await?;
    let query = opts.model.clone().or_else(|| {
        opts.backend
            .as_deref()
            .map(|name| format!("{name}/default"))
    });
    let agent = factory.open(query.as_deref(), executor).await?;
    // The record names the model the agent settled on even when the run
    // fails before its first round reports one.
    summary.borrow_mut().set_model(agent.run_model());

    // A run in a session keeps a transcript, so a driver that kills it and
    // resumes the id gets every completed round back.
    let mut transcript = open_transcript(opts, &agent.run_model(), agent.system_prompt())?;
    let mut messages = transcript
        .as_mut()
        .map(PrintTranscript::take_history)
        .unwrap_or_default();
    if let Some(transcript) = &transcript {
        // The driver that kills this run needs the id to resume it, and the
        // record carries the same id for a driver that reads it afterwards.
        let id = transcript.id();
        eprintln!("session: {id}");
        summary.borrow_mut().set_session(id);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);

    let prompt = opts.prompt.clone();
    let agent_run = agent.run_turn(
        &mut messages,
        lofi_core::TurnRequest {
            input: lofi_core::TurnInput::Prompt {
                text: prompt,
                kind: lofi_types::PromptKind::User,
                attachments: Vec::new(),
            },
            notices: Vec::new(),
        },
        tx,
        transcript.as_ref().map(PrintTranscript::cursor),
        lofi_core::RunControls {
            cancel: None,
            preempt: None,
        },
    );

    // The agent future is not `Send` (the QuickJS `AsyncContext` is not
    // `Sync`), so it cannot be `tokio::spawn`'d. Drive it concurrently with
    // the event consumer on the same task: when the agent finishes it drops
    // `tx`, `rx.recv()` returns `None`, and the consumer drains.
    let consumer = async {
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        let mut context_pressure = false;
        loop {
            let Some(ev) = rx.recv().await else {
                break;
            };
            summary.borrow_mut().note(&ev);
            let write_res: std::io::Result<()> = match ev {
                AgentEvent::Text(delta) => stdout.write_all(delta.as_bytes()),
                AgentEvent::Thinking(_)
                | AgentEvent::ThinkingEnd { .. }
                | AgentEvent::ToolEnd { .. }
                | AgentEvent::NativeToolStart { .. }
                | AgentEvent::NativeToolEnd { .. }
                | AgentEvent::ToolInputDelta { .. }
                | AgentEvent::RoundCommitted { .. }
                | AgentEvent::TurnCommitted { .. }
                | AgentEvent::RoundUsage { .. }
                | AgentEvent::TurnUsage { .. }
                | AgentEvent::SubagentUsage { .. }
                | AgentEvent::ContextReported { .. }
                | AgentEvent::ModelReported { .. }
                | AgentEvent::RetryEnd { .. }
                | AgentEvent::RunStart
                | AgentEvent::Prompt { .. }
                | AgentEvent::TurnContinue
                | AgentEvent::Compaction { .. }
                | AgentEvent::UserShellStart { .. }
                | AgentEvent::UserShellDelta(_)
                | AgentEvent::UserShell { .. } => Ok(()),
                AgentEvent::ContextPressure { resume, .. } => {
                    if resume {
                        context_pressure = true;
                        let _ = stdout.write_all(b"\n");
                        writeln!(
                            stderr,
                            "error: context limit reached; use the interactive TUI to compact and continue"
                        )
                    } else {
                        // The turn already stopped on its own: a one-shot
                        // run has nothing to continue.
                        Ok(())
                    }
                }
                AgentEvent::RetryStart {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error,
                } => writeln!(
                    stderr,
                    "warning: provider retry {attempt}/{max_attempts} in {delay_ms}ms: {error}"
                ),
                AgentEvent::Notice(msg) => writeln!(stderr, "warning: {msg}"),
                AgentEvent::Error(msg) => writeln!(stderr, "error: {msg}"),
                AgentEvent::ToolStart { name, .. } => writeln!(stderr, "[{name}]"),
                AgentEvent::ToolInput { code, .. } => writeln!(stderr, "{code}"),
                AgentEvent::TurnEnd { .. } => stdout.write_all(b"\n"),
                AgentEvent::TurnFailed { error, .. } => {
                    let _ = stdout.write_all(b"\n");
                    writeln!(stderr, "error: {error}")
                }
                AgentEvent::TurnCancelled { .. } => {
                    let _ = stdout.write_all(b"\n");
                    writeln!(stderr, "Operation aborted")
                }
            };
            if let Err(e) = write_res {
                // Close the receive side so agent sends fail immediately
                // instead of filling the bounded channel and blocking
                // forever on a consumer whose output pipe is gone.
                rx.close();
                return Err(Error::Io(e));
            }
        }
        stdout.flush()?;
        stderr.flush()?;
        if context_pressure {
            return Err(Error::State(
                "context limit reached in non-interactive mode".to_string(),
            ));
        }
        Ok::<(), Error>(())
    };

    // Drive agent and consumer concurrently. The agent future is not `Send`,
    // so both run on this task. If the consumer fails (output pipe gone),
    // return its error immediately without waiting for the agent to time
    // out; the agent future is dropped and its in-flight tool (if any) is
    // left to the `spawn_blocking` cap. On normal agent completion, drain
    // the consumer so buffered events flush before returning.
    tokio::pin!(agent_run, consumer);
    // Neither branch returns early: a failed run still writes its record.
    let result = tokio::select! {
        biased;
        consumer_res = &mut consumer => match consumer_res {
            Ok(()) => (&mut agent_run).await.map(|_| ()),
            Err(e) => Err(e),
        },
        agent_res = &mut agent_run => {
            // Agent finished and dropped tx; drain remaining events + flush.
            // Propagate the consumer result so a failed terminal write or
            // flush is not silently swallowed.
            match (&mut consumer).await {
                Ok(()) => agent_res.map(|_| ()),
                Err(e) => Err(e),
            }
        }
    };
    result
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dual_provider_config(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("lofi.toml");
        std::fs::write(
            &path,
            r#"
default_model = "alpha/a1"

[providers.alpha]
api_type = "openai-completions"
api_key = "alpha-key"

[providers.alpha.models]
a1 = {}

[providers.beta]
api_type = "openai-completions"
api_key = "beta-key"

[providers.beta.models]
b1 = {}
"#,
        )
        .unwrap();
        path
    }

    #[tokio::test]
    async fn startup_model_flag_honored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = dual_provider_config(tmp.path());
        let opts = InteractiveOptions::new(tmp.path())
            .with_config_path(&cfg)
            .with_model("beta/b1");
        let env = env_pairs(&opts.env).unwrap();
        let started = startup(&opts, opts.model.as_deref(), &env).await.unwrap();
        assert_eq!(started.backend.name(), "beta");
        assert_eq!(started.backend.selection().model, "b1");
    }

    #[tokio::test]
    async fn startup_default_model_when_no_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = dual_provider_config(tmp.path());
        let opts = InteractiveOptions::new(tmp.path()).with_config_path(&cfg);
        let env = env_pairs(&opts.env).unwrap();
        let started = startup(&opts, None, &env).await.unwrap();
        assert_eq!(started.backend.name(), "alpha");
        assert_eq!(started.backend.selection().model, "a1");
    }
}
