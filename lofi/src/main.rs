//! lofi binary entry point.
//!
//! A thin clap CLI that dispatches into the lofi crates:
//! - `--list-models` prints `provider/id — name` lines and exits;
//! - `--print <prompt>` runs one non-interactive turn via [`lofi_ui::run_print`];
//! - otherwise the interactive TUI is launched via [`lofi_ui::run_interactive`].
//!
//! `tracing` is only initialized when `RUST_LOG` is set, so the TUI never has
//! log output interleaved into the alternate screen. All stdout writes go
//! through `std::io::stdout().write_all` (`println!` is denied workspace-wide).

use std::io::{IsTerminal, Write};

use anyhow::Context;
use clap::Parser;
use lofi_core::ModelRegistry;
use lofi_ui::{InteractiveOptions, PrintOptions};

/// Command-line interface for lofi.
#[derive(Parser, Debug)]
#[command(
    name = "lofi",
    version,
    about = "A minimal coding agent harness with a code-mode sandbox"
)]
struct Cli {
    /// Run one non-interactive turn and stream the assistant text to stdout.
    #[arg(long, value_name = "PROMPT")]
    print: Option<String>,
    /// List configured models as `provider/id — name` and exit.
    #[arg(long)]
    list_models: bool,
    /// Select the model as `provider/model[:level]` (e.g.
    /// `openai/gpt-5.6-sol:xhigh` or `anthropic/claude-sonnet-5`).
    #[arg(long, value_name = "SPEC")]
    model: Option<String>,
    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long)]
    continue_last: bool,
    /// Resume a specific session by id prefix.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Do not persist a transcript.
    #[arg(long)]
    no_session: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Only wire up tracing when the user opts in via RUST_LOG; the TUI owns
    // the alternate screen and log spam there would corrupt the frame.
    if std::env::var_os("RUST_LOG").is_some() {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }

    let cli = Cli::parse();
    let root = std::env::current_dir().context("determine working directory")?;

    if cli.list_models {
        list_models(&cli).await?;
    } else if let Some(prompt) = cli.print.clone() {
        let opts = build_print_opts(&cli, prompt, root);
        lofi_ui::run_print(opts).await?;
    } else {
        let opts = build_interactive_opts(&cli, root);
        lofi_ui::run_interactive(opts).await?;
    }
    Ok(())
}

/// Build [`PrintOptions`] from the parsed flags.
fn build_print_opts(cli: &Cli, prompt: String, root: std::path::PathBuf) -> PrintOptions {
    let mut opts = PrintOptions::new(prompt, root);
    if let Some(m) = &cli.model {
        opts = opts.with_model(m);
    }
    opts
}

/// Build [`InteractiveOptions`] from the parsed flags.
fn build_interactive_opts(cli: &Cli, root: std::path::PathBuf) -> InteractiveOptions {
    let mut opts = InteractiveOptions::new(root);
    if let Some(m) = &cli.model {
        opts = opts.with_model(m);
    }
    if cli.continue_last {
        opts = opts.with_continue();
    }
    if let Some(id) = &cli.resume {
        opts = opts.with_resume(id);
    }
    if cli.no_session {
        opts = opts.with_no_session();
    }
    opts
}

/// Load config, build the registry (with remote discovery + static fallback),
/// and write `provider/id — name` lines to stdout.
async fn list_models(_cli: &Cli) -> anyhow::Result<()> {
    let config_path =
        lofi_core::config_loader::user_config_path().context("resolve user config path")?;
    let config = lofi_core::config_loader::load_config_or_default(&config_path)
        .await
        .context("load user config")?;

    let registry = match ModelRegistry::load_async(&config).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "async model load failed; falling back to static");
            ModelRegistry::load(&config).context("build static model registry")?
        }
    };

    let out = registry.list_models_print();
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    handle.write_all(out.as_bytes())?;
    handle.write_all(b"\n")?;
    if std::io::stdout().is_terminal() {
        handle.flush()?;
    }
    Ok(())
}
