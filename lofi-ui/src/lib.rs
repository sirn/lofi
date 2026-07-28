//! `lofi-ui`: the presentation layer.
//!
//! Owns both user-facing drivers — the interactive TUI ([`tui`]) and the
//! non-interactive `--print` stdio loop ([`run_print`]) — and the option
//! bundles they share. Both build the agent through [`lofi_core::build_agent`]
//! (the Service Layer) and never reach into providers or the sandbox
//! directly, so the dependency direction stays Presentation → Domain.

use std::io::Write;
use std::path::PathBuf;

use lofi_core::{build_agent, AgentEvent};
use lofi_error::{Error, Result};
use lofi_types::ThinkingLevel;

mod cli;
pub mod tui;

pub use cli::run_cli;

/// Options for the interactive TUI session.
///
/// Mirrors the `--print` flags' resolution fields but with no prompt — the
/// prompts come from the TUI input box at runtime.
#[derive(Debug, Clone)]
pub struct InteractiveOptions {
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Optional override for the config file path.
    pub config_path: Option<PathBuf>,
    /// Optional `--model provider/model[:level]` selection.
    pub model: Option<String>,
    /// Resume the most recent session for this workspace (`-c`/`--continue`).
    pub continue_last: bool,
    /// Resume a specific session by id prefix (`--resume <id>`).
    pub resume: Option<String>,
    /// Disable transcript persistence (`--no-session`).
    pub no_session: bool,
}

impl InteractiveOptions {
    /// Construct an `InteractiveOptions` with a workspace root, leaving the
    /// flag-style fields at their defaults.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            config_path: None,
            model: None,
            continue_last: false,
            resume: None,
            no_session: false,
        }
    }

    /// Override the config file path.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Override the model (`provider/model[:level]`).
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Resume the most recent session (`--continue`).
    #[must_use]
    pub fn with_continue(mut self) -> Self {
        self.continue_last = true;
        self
    }

    /// Resume a specific session by id prefix (`--resume`).
    #[must_use]
    pub fn with_resume(mut self, id: impl Into<String>) -> Self {
        self.resume = Some(id.into());
        self
    }

    /// Disable transcript persistence (`--no-session`).
    #[must_use]
    pub fn with_no_session(mut self) -> Self {
        self.no_session = true;
        self
    }
}

/// Options for the non-interactive `--print` path.
///
/// `config_path` defaults to the user config file
/// ([`lofi_core::config_loader::user_config_path`]) when `None`; `model`
/// mirrors the `--model` flag.
#[derive(Debug, Clone)]
pub struct PrintOptions {
    /// The user prompt to send.
    pub prompt: String,
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Optional override for the config file path.
    pub config_path: Option<PathBuf>,
    /// Optional `--model provider/model[:level]` selection.
    pub model: Option<String>,
}

impl PrintOptions {
    /// Construct a `PrintOptions` with a prompt and workspace root, leaving
    /// all flag-style fields at their defaults.
    #[must_use]
    pub fn new(prompt: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            prompt: prompt.into(),
            root: root.into(),
            config_path: None,
            model: None,
        }
    }

    /// Override the config file path.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Override the model (`provider/model[:level]`).
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

/// Run the interactive TUI session.
///
/// Builds the agent via [`lofi_core::build_agent`] and hands it to [`tui::run`].
/// Terminal setup/teardown is owned by the TUI's `Drop` guard; this function
/// returns after the user quits.
///
/// A missing-models outcome is *not* fatal: the TUI still launches so the
/// user can read transcripts, browse `/resume`, and quit. A hint explaining
/// how to configure a model is shown in the log, and submitting a prompt
/// re-surfaces that hint instead of running.
///
/// # Errors
/// Propagates [`Error`] from config/session resolution or the terminal
/// session. Model-resolution failures that reduce to "no active model" are
/// swallowed into the no-model TUI mode described above.
pub async fn run_interactive(opts: InteractiveOptions) -> Result<()> {
    // Resolve the session up front so it is available in both the normal and
    // the no-model launch paths: --resume <id>, else --continue (most recent),
    // else a fresh session created lazily on the first prompt. --no-session
    // skips persistence entirely. A missing most-recent falls back to fresh.
    let session = resolve_session(&opts)?;

    // Restore the model+thinking the user last ran with unless --model was
    // given: the resumed session's final turn marker carries it (raw, on the
    // active path), so a fresh/ephemeral session with no turns falls through
    // to the config default. `resolve_startup_agent` falls back to that
    // default when the restored model is no longer resolvable.
    let restored = opts
        .model
        .is_none()
        .then(|| session.last_run_model())
        .flatten()
        .map(|m| format!("{}/{}:{}", m.provider, m.id, m.thinking.as_str()));

    let (agent, label, thinking, hint, ctx_limit, compaction, switcher) =
        match resolve_startup_agent(&opts, restored.as_deref()).await? {
            StartupAgent::Ready(built) => {
                let (agent, model, thinking, config, registry) = *built;
                let root = opts.root.clone();
                let compaction = config.compaction.clone();
                let switcher = tui::ModelSwitcher::new(registry, config, root);
                (
                    Some(agent),
                    format!("{}/{}", model.provider, model.id),
                    thinking,
                    None,
                    model.context_window.unwrap_or(0),
                    compaction,
                    Some(switcher),
                )
            }
            // No provider has credentials (or the selected provider is
            // disabled): launch the TUI anyway and show the hint in the log.
            StartupAgent::NoModel(hint) => (
                None,
                "(no model)".to_string(),
                ThinkingLevel::Off,
                Some(hint),
                0,
                lofi_types::CompactionConfig::default(),
                None,
            ),
        };
    tui::run(
        agent, label, thinking, session, hint, ctx_limit, compaction, switcher,
    )
    .await
}

/// Startup agent resolution: a built agent, or the model-less launch path.
///
/// `Ready`'s payload is boxed: it holds the agent, config, and model
/// registry (a KB+ together), and the variant is built once and unpacked
/// immediately, so a single allocation keeps the enum off the stack.
enum StartupAgent {
    Ready(
        Box<(
            lofi_core::Agent,
            lofi_types::Model,
            ThinkingLevel,
            lofi_types::Config,
            lofi_core::ModelRegistry,
        )>,
    ),
    /// No provider has credentials — launch the TUI model-less with `hint`.
    NoModel(String),
}

/// Build the startup agent. `--model` takes precedence over a restored
/// session model; with neither, `build_agent` falls back to the config
/// default. If the restored model can no longer be resolved (removed from
/// the config since the session ran, or its provider lost its API key), fall
/// back to the default so resume still opens the transcript instead of
/// aborting. An explicit `--model` that fails to resolve is not silently
/// replaced — it surfaces via the error arms below rather than masked by
/// the default.
async fn resolve_startup_agent(
    opts: &InteractiveOptions,
    restored: Option<&str>,
) -> Result<StartupAgent> {
    let requested = opts.model.as_deref().or(restored);
    match build_agent(opts.config_path.as_deref(), requested, opts.root.as_path()).await {
        Ok(built) => Ok(StartupAgent::Ready(Box::new(built))),
        // The restored model is gone from the registry (config changed since
        // the session ran, or its provider lost its API key): fall back to
        // the default so the transcript is still readable instead of aborting.
        Err(_) if restored.is_some() => {
            match build_agent(opts.config_path.as_deref(), None, opts.root.as_path()).await {
                Ok(built) => Ok(StartupAgent::Ready(Box::new(built))),
                Err(Error::NoModels(hint)) => Ok(StartupAgent::NoModel(hint)),
                Err(e) => Err(e),
            }
        }
        Err(Error::NoModels(hint)) => Ok(StartupAgent::NoModel(hint)),
        Err(e) => Err(e),
    }
}

/// Resolve the [`tui::SessionConfig`] for an interactive run from the flags.
///
/// Independent of model resolution so the no-model launch path still gets a
/// session (and thus `/resume`, transcript browsing, etc.).
///
/// # Errors
/// Returns [`Error::State`] when `--resume <id>` matches no session, or
/// propagates [`Error::Io`] from the store.
fn resolve_session(opts: &InteractiveOptions) -> Result<tui::SessionConfig> {
    if opts.no_session {
        return Ok(tui::SessionConfig::ephemeral(opts.root.clone()));
    }
    let store = lofi_core::session::store::SessionStore::open()?;
    if let Some(id) = &opts.resume {
        let entry = store.find(&opts.root, id)?.ok_or_else(|| {
            Error::State(format!("no session matching id '{id}' for this workspace"))
        })?;
        let (cursor, snapshot) =
            lofi_core::session::store::SessionCursor::open_snapshot(entry.path)?;
        return Ok(tui::SessionConfig::resumed(
            store,
            cursor,
            snapshot.index,
            snapshot.file_size,
            opts.root.clone(),
        ));
    }
    if opts.continue_last {
        if let Some(entry) = store.most_recent(&opts.root)? {
            let (cursor, snapshot) =
                lofi_core::session::store::SessionCursor::open_snapshot(entry.path)?;
            return Ok(tui::SessionConfig::resumed(
                store,
                cursor,
                snapshot.index,
                snapshot.file_size,
                opts.root.clone(),
            ));
        }
    }
    Ok(tui::SessionConfig::fresh(store, opts.root.clone()))
}

/// Run a single non-interactive prompt and stream assistant text to stdout.
///
/// Builds the agent via [`lofi_core::build_agent`], then drives
/// [`lofi_core::Agent::run`] concurrently with a stdio consumer:
/// - [`AgentEvent::Text`] deltas are written to stdout via
///   `std::io::stdout().write_all` (never `println!`, which is denied by the
///   workspace lints);
/// - [`AgentEvent::Error`] is written to stderr;
/// - [`AgentEvent::Done`] writes a trailing newline to stdout;
/// - `ToolStart` / `ToolEnd` are surfaced as short stderr markers so stdout
///   stays clean for piping.
///
/// # Errors
/// Propagates [`Error`] from config load, model resolution, provider
/// construction, or the agent run. The binary caller is responsible for
/// translating the returned error into a nonzero exit code.
pub async fn run_print(opts: PrintOptions) -> Result<()> {
    let (agent, _model, _thinking, _config, _registry) = build_agent(
        opts.config_path.as_deref(),
        opts.model.as_deref(),
        opts.root.as_path(),
    )
    .await?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);

    let prompt = opts.prompt.clone();
    let agent_run = agent.run(prompt, tx);

    // The agent future is not `Send` (the QuickJS `AsyncContext` is not
    // `Sync`), so it cannot be `tokio::spawn`'d. Drive it concurrently with
    // the event consumer on the same task: when the agent finishes it drops
    // `tx`, `rx.recv()` returns `None`, and the consumer drains.
    let consumer = async {
        let mut stdout = std::io::stdout().lock();
        let mut stderr = std::io::stderr().lock();
        loop {
            let Some(ev) = rx.recv().await else {
                break;
            };
            let write_res: std::io::Result<()> = match ev {
                AgentEvent::Text(delta) => stdout.write_all(delta.as_bytes()),
                // Reasoning is folded out of the piped stdout stream (the
                // interactive TUI surfaces it), and a completed tool call
                // writes nothing to stdout — both keep `--print` pipe-clean.
                AgentEvent::Thinking(_)
                | AgentEvent::ThinkingEnd { .. }
                | AgentEvent::ToolEnd { .. }
                | AgentEvent::NativeToolStart { .. }
                | AgentEvent::NativeToolEnd { .. }
                | AgentEvent::ToolInputDelta { .. }
                // A storage signal (turn appended to the transcript); the
                // `--print` pipe has no use for it.
                | AgentEvent::TurnCommitted { .. }
                // Per-round usage is a status signal for the TUI; the pipe
                // has no use for it (the final `TurnEnd` ends the run).
                | AgentEvent::RoundUsage { .. }
                // Retry lifecycle is invisible on the pipe; a transient
                // error that retries will surface as normal text if it
                // recovers, or as `Error` if it doesn't.
                | AgentEvent::RetryStart { .. }
                | AgentEvent::RetryEnd { .. }
                // Turn boundaries carry no piped output; the prompt itself
                // is not echoed (the user typed it).
                | AgentEvent::TurnStart { .. }
                // Live-only TUI signals; never produced in --print mode
                // (headless uses `Agent::run`, which has no hard-cap stop).
                | AgentEvent::TurnContinue
                | AgentEvent::ContextPressure { .. }
                // Compaction is a TUI-only marker; never produced in
                // --print mode.
                | AgentEvent::Compaction { .. }
                | AgentEvent::UserBash { .. } => Ok(()),
                AgentEvent::Error(msg) => writeln!(stderr, "error: {msg}"),
                AgentEvent::ToolStart { name, .. } => writeln!(stderr, "[{name}]"),
                AgentEvent::ToolInput { code, .. } => writeln!(stderr, "{code}"),
                AgentEvent::TurnEnd { .. } => stdout.write_all(b"\n"),
                AgentEvent::TurnFailed { error, .. } => {
                    let _ = stdout.write_all(b"\n");
                    writeln!(stderr, "error: {error}")
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
        Ok::<(), Error>(())
    };

    // Drive agent and consumer concurrently. The agent future is not `Send`,
    // so both run on this task. If the consumer fails (output pipe gone),
    // return its error immediately without waiting for the agent to time
    // out; the agent future is dropped and its in-flight tool (if any) is
    // left to the `spawn_blocking` cap. On normal agent completion, drain
    // the consumer so buffered events flush before returning.
    tokio::pin!(agent_run, consumer);
    tokio::select! {
        biased;
        consumer_res = &mut consumer => {
            match consumer_res {
                Ok(()) => {
                    // Consumer saw rx close (agent dropped tx); it is done.
                    (&mut agent_run).await?;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        agent_res = &mut agent_run => {
            // Agent finished and dropped tx; drain remaining events + flush.
            // Propagate the consumer result so a failed terminal write or
            // flush is not silently swallowed.
            (&mut consumer).await?;
            agent_res?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn print_options_builder() {
        let opts = PrintOptions::new("hello", "/tmp/ws")
            .with_model("openai/gpt-4o")
            .with_config_path("/tmp/cfg.toml");
        assert_eq!(opts.prompt, "hello");
        assert_eq!(opts.root, PathBuf::from("/tmp/ws"));
        assert_eq!(opts.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(
            opts.config_path.as_deref(),
            Some(std::path::Path::new("/tmp/cfg.toml"))
        );
    }

    #[test]
    fn interactive_options_builder() {
        let opts = InteractiveOptions::new("/tmp/ws")
            .with_model("openai/gpt-4o")
            .with_config_path("/tmp/cfg.toml");
        assert_eq!(opts.root, PathBuf::from("/tmp/ws"));
        assert_eq!(opts.model.as_deref(), Some("openai/gpt-4o"));
        assert_eq!(
            opts.config_path.as_deref(),
            Some(std::path::Path::new("/tmp/cfg.toml"))
        );
    }

    /// Temp config with two `no_auth` providers so model resolution needs no
    /// environment variables or network access. `alpha` is first and thus the
    /// default; `beta` is the non-default that `--model` must select.
    fn dual_provider_config(dir: &std::path::Path) -> std::path::PathBuf {
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "[providers.alpha]\nno_auth = true\n\n[providers.alpha.models]\na1 = {}\n\n[providers.beta]\nno_auth = true\n\n[providers.beta.models]\nb1 = {}\n").unwrap();
        cfg
    }

    /// `--model provider/model` must be honored at startup, not silently
    /// replaced by the config default. Regression for the model-restore
    /// refactor that passed `restored` (None when `--model` is set) to
    /// `build_agent` instead of `opts.model`.
    #[tokio::test]
    async fn startup_model_flag_honored() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = dual_provider_config(tmp.path());
        let opts = InteractiveOptions::new(tmp.path())
            .with_config_path(&cfg)
            .with_model("beta/b1");
        match resolve_startup_agent(&opts, None).await.unwrap() {
            StartupAgent::Ready(built) => {
                let (_agent, model, _thinking, _config, _registry) = *built;
                assert_eq!(model.provider, "beta");
                assert_eq!(model.id, "b1");
            }
            StartupAgent::NoModel(hint) => panic!("expected a built agent, got NoModel: {hint}"),
        }
    }

    /// With no `--model` and no restored session model, the config default
    /// (first available provider's first model) is selected.
    #[tokio::test]
    async fn startup_default_model_when_no_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = dual_provider_config(tmp.path());
        let opts = InteractiveOptions::new(tmp.path()).with_config_path(&cfg);
        match resolve_startup_agent(&opts, None).await.unwrap() {
            StartupAgent::Ready(built) => {
                let (_agent, model, _thinking, _config, _registry) = *built;
                assert_eq!(model.provider, "alpha");
                assert_eq!(model.id, "a1");
            }
            StartupAgent::NoModel(hint) => panic!("expected a built agent, got NoModel: {hint}"),
        }
    }
}
