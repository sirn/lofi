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

pub mod tui;

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
    /// Optional `--provider` selection.
    pub provider: Option<String>,
    /// Optional `--model` id / qualifier / pattern.
    pub model: Option<String>,
    /// Optional literal `--api-key` override for the selected provider.
    pub api_key: Option<String>,
}

impl InteractiveOptions {
    /// Construct an `InteractiveOptions` with a workspace root, leaving the
    /// flag-style fields at their defaults.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            config_path: None,
            provider: None,
            model: None,
            api_key: None,
        }
    }

    /// Override the config file path.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Override the selected provider.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Override the model id / pattern.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the API key for the selected provider.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// Options for the non-interactive `--print` path.
///
/// `config_path` defaults to the user config file
/// ([`lofi_core::config_loader::user_config_path`]) when `None`. `provider`
/// and `model` mirror the `--provider` / `--model` flags; `api_key` is the
/// literal `--api-key` override applied to the selected provider after config
/// load.
#[derive(Debug, Clone)]
pub struct PrintOptions {
    /// The user prompt to send.
    pub prompt: String,
    /// Workspace root file operations are confined to.
    pub root: PathBuf,
    /// Optional override for the config file path.
    pub config_path: Option<PathBuf>,
    /// Optional `--provider` selection.
    pub provider: Option<String>,
    /// Optional `--model` id / qualifier / pattern.
    pub model: Option<String>,
    /// Optional literal `--api-key` override for the selected provider.
    pub api_key: Option<String>,
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
            provider: None,
            model: None,
            api_key: None,
        }
    }

    /// Override the config file path.
    #[must_use]
    pub fn with_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.config_path = Some(path.into());
        self
    }

    /// Override the selected provider.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = Some(provider.into());
        self
    }

    /// Override the model id / pattern.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Override the API key for the selected provider.
    #[must_use]
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// Run the interactive TUI session.
///
/// Builds the agent via [`lofi_core::build_agent`] and hands it to [`tui::run`].
/// Terminal setup/teardown is owned by the TUI's `Drop` guard; this function
/// returns after the user quits.
///
/// # Errors
/// Propagates [`Error`] from agent construction or the terminal session.
pub async fn run_interactive(opts: InteractiveOptions) -> Result<()> {
    let (agent, model) = build_agent(
        opts.config_path.as_deref(),
        opts.provider.as_deref(),
        opts.model.as_deref(),
        opts.api_key.as_deref(),
        opts.root.as_path(),
    )
    .await?;
    let label = format!("{}/{}", model.provider, model.id);
    tui::run(agent, label).await
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
    let (agent, _model) = build_agent(
        opts.config_path.as_deref(),
        opts.provider.as_deref(),
        opts.model.as_deref(),
        opts.api_key.as_deref(),
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
                AgentEvent::Error(msg) => writeln!(stderr, "error: {msg}"),
                AgentEvent::ToolStart { name, .. } => writeln!(stderr, "[{name}]"),
                AgentEvent::ToolInput { code, .. } => writeln!(stderr, "{code}"),
                AgentEvent::ToolEnd { .. } => Ok(()),
                AgentEvent::Done(_) => stdout.write_all(b"\n"),
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
            .with_provider("openai")
            .with_model("gpt-4o")
            .with_api_key("sk-override")
            .with_config_path("/tmp/cfg.toml");
        assert_eq!(opts.prompt, "hello");
        assert_eq!(opts.root, PathBuf::from("/tmp/ws"));
        assert_eq!(opts.provider.as_deref(), Some("openai"));
        assert_eq!(opts.model.as_deref(), Some("gpt-4o"));
        assert_eq!(opts.api_key.as_deref(), Some("sk-override"));
        assert_eq!(
            opts.config_path.as_deref(),
            Some(std::path::Path::new("/tmp/cfg.toml"))
        );
    }

    #[test]
    fn interactive_options_builder() {
        let opts = InteractiveOptions::new("/tmp/ws")
            .with_provider("openai")
            .with_model("gpt-4o")
            .with_api_key("sk-override")
            .with_config_path("/tmp/cfg.toml");
        assert_eq!(opts.root, PathBuf::from("/tmp/ws"));
        assert_eq!(opts.provider.as_deref(), Some("openai"));
        assert_eq!(opts.model.as_deref(), Some("gpt-4o"));
        assert_eq!(opts.api_key.as_deref(), Some("sk-override"));
        assert_eq!(
            opts.config_path.as_deref(),
            Some(std::path::Path::new("/tmp/cfg.toml"))
        );
    }
}
