//! lofi binary entry point.
//!
//! A thin clap CLI that dispatches into the lofi crates:
//! - `--list-models` prints `provider/id — name` lines (with a `·img` marker for image-capable models) and exits;
//! - `--list-sessions` prints saved sessions for the workspace and exits;
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
#[allow(clippy::struct_excessive_bools)] // CLI flag struct
#[command(
    name = "lofi",
    version,
    about = "A minimal coding agent harness with a code-mode sandbox"
)]
struct Cli {
    /// Run one non-interactive turn and stream the assistant text to stdout.
    #[arg(short = 'p', long, value_name = "PROMPT")]
    print: Option<String>,
    /// List configured models as `provider/id — name` (with a `·img` marker for image-capable models) and exit.
    #[arg(long)]
    list_models: bool,
    /// List saved sessions for this workspace and exit.
    #[arg(long)]
    list_sessions: bool,
    /// Select the model as `provider/model[:level]`.
    #[arg(long, value_name = "SPEC")]
    model: Option<String>,
    /// Continue the most recent session for this workspace.
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a specific session by id prefix.
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Do not persist a transcript.
    #[arg(long)]
    no_session: bool,
    /// Print the embedded API reference index and exit.
    #[arg(long)]
    docs: bool,
    /// Search the embedded API reference and print matching entries.
    #[arg(long, value_name = "QUERY")]
    docs_search: Option<String>,
    /// Dry-run: evaluate a command against the shell policy and print the
    /// decision without executing anything.
    #[arg(long, value_name = "COMMAND")]
    policy_explain: Option<String>,
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
    } else if cli.list_sessions {
        list_sessions(&root)?;
    } else if cli.docs {
        print_docs_index()?;
    } else if let Some(query) = cli.docs_search.clone() {
        print_docs_search(&query)?;
    } else if let Some(cmd) = cli.policy_explain.clone() {
        policy_explain(&root, &cmd).await?;
    } else if let Some(prompt) = cli.print.clone() {
        let opts = build_print_opts(&cli, prompt, root);
        lofi_ui::run_print(opts).await?;
    } else {
        let opts = build_interactive_opts(&cli, root);
        lofi_ui::run_interactive(opts).await?;
    }
    Ok(())
}

/// Evaluate a command against the resolved shell policy and print the
/// decision. Used by `--policy-explain` for dry-run diagnostics.
async fn policy_explain(root: &std::path::Path, cmd: &str) -> anyhow::Result<()> {
    let _ = root;
    let config_path = lofi_core::config_loader::user_config_path()
        .context("resolve user config path")?;
    let cfg = lofi_core::config_loader::load_config_or_default(&config_path).await?;
    let policy = lofi_code::policy::defaults::resolve(&cfg.shell_policy);
    let decision = policy.evaluate(cmd);
    let action_str = match decision.action {
        lofi_types::PolicyAction::Allow => "allow",
        lofi_types::PolicyAction::Ask => "ask",
        lofi_types::PolicyAction::Deny => "deny",
    };
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "action: {action_str}")?;
    if !decision.reason.is_empty() {
        writeln!(out, "reason: {}", decision.reason)?;
    }
    if let Some(matched) = &decision.matched_command {
        writeln!(out, "matched: {matched}")?;
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

/// List saved sessions for `root`, newest-first, as `id  created  model  (n msgs)`.
fn list_sessions(root: &std::path::Path) -> anyhow::Result<()> {
    let store = lofi_core::session::store::SessionStore::open()
        .context("open session store")?;
    let entries = store.list_for_cwd(root).context("list sessions")?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if entries.is_empty() {
        handle.write_all(b"(no sessions)\n")?;
    } else {
        for e in &entries {
            let line = format!(
                "{}  {}  {}  ({} msgs)\n",
                e.id(),
                format_ts(e.meta.created),
                e.meta.model.label(),
                e.message_count,
            );
            handle.write_all(line.as_bytes())?;
        }
    }
    if std::io::stdout().is_terminal() {
        handle.flush()?;
    }
    Ok(())
}

/// Print the API reference index (`name — summary` per line) to stdout.
fn print_docs_index() -> anyhow::Result<()> {
    let idx = lofi_core::docs::docs_index();
    let entries = idx["entries"].as_array().context("malformed docs index")?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for e in entries {
        let name = e["name"].as_str().unwrap_or("?");
        let summary = e["summary"].as_str().unwrap_or("");
        let line = format!("{name} — {summary}\n");
        handle.write_all(line.as_bytes())?;
    }
    if std::io::stdout().is_terminal() {
        handle.flush()?;
    }
    Ok(())
}

/// Print search results (`name [score] — excerpt` per line) to stdout.
fn print_docs_search(query: &str) -> anyhow::Result<()> {
    let res = lofi_core::docs::docs_search(query);
    let results = res["results"].as_array().context("malformed search results")?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if results.is_empty() {
        handle.write_all(b"(no matches)\n")?;
    } else {
        for r in results {
            let name = r["name"].as_str().unwrap_or("?");
            let score = r["score"].as_u64().unwrap_or(0);
            let excerpt = r["excerpt"].as_str().unwrap_or("");
            let line = format!("{name} [{score}] — {excerpt}\n");
            handle.write_all(line.as_bytes())?;
        }
    }
    if std::io::stdout().is_terminal() {
        handle.flush()?;
    }
    Ok(())
}

/// Format a wall-clock millisecond timestamp as UTC `YYYY-MM-DD HH:MM`.
#[allow(clippy::many_single_char_names)] // mirrors Howard Hinnant's civil calendar algorithm
fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let day = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
    // Civil calendar conversion (Howard Hinnant's algorithm), days -> date.
    let z = day + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!("{year:04}-{month:02}-{d:02} {h:02}:{m:02}")
}

/// Load config, build the registry (with remote discovery + static fallback),
/// and write `provider/id — name` lines (with a `·img` marker for image-capable models) to stdout.
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
