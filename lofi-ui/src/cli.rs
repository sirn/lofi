use std::io::{IsTerminal, Write};

use crate::{InteractiveOptions, PrintOptions};
use anyhow::Context;
use clap::Parser;
use lofi_core::ModelRegistry;

#[derive(Parser, Debug)]
#[allow(clippy::struct_excessive_bools)] // CLI flag struct
#[command(
    name = "lofi",
    version,
    about = "A minimal coding agent harness with a code-mode sandbox"
)]
struct Cli {
    /// Run one non-interactive turn and stream the assistant text
    #[arg(short = 'p', long, value_name = "PROMPT")]
    print: Option<String>,
    /// Set an environment variable for this run; repeatable
    #[arg(short = 'e', long = "env", value_name = "NAME[=VALUE]")]
    env: Vec<String>,
    /// Select the model as `provider/model[:level][@tier]`
    #[arg(long, value_name = "SPEC")]
    model: Option<String>,
    /// Run on this CLI agent backend
    #[arg(long, value_name = "NAME")]
    backend: Option<String>,
    /// Resume the most recent session of this workspace
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a session by id prefix
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Do not persist a transcript
    #[arg(long)]
    no_session: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once, then matched by value
enum Command {
    /// Print the configured models and exit.
    Models,
    /// List the saved sessions of this workspace, or prune saved
    /// sessions.
    Sessions {
        /// Consider every workspace, not just this directory's sessions
        #[arg(long)]
        all: bool,
        #[command(subcommand)]
        cmd: Option<SessionCommand>,
    },
    /// Work with the shell policy.
    Policy {
        #[command(subcommand)]
        cmd: PolicyCommand,
    },
    /// Print the embedded API reference index, or search it.
    Docs {
        #[command(subcommand)]
        cmd: Option<DocsCommand>,
    },
    /// Serve the exec bridge for a spawned agent backend.
    Mcp {
        #[command(subcommand)]
        cmd: McpCommand,
    },
}

#[derive(clap::Subcommand, Debug)]
enum SessionCommand {
    /// Delete saved sessions last active before a cutoff.
    ///
    /// Without `--all`, only this directory's sessions are considered. A
    /// subagent transcript is a saved session too, so it ages out with the
    /// rest.
    Prune {
        /// Delete sessions last active before this age: `s`, `m`, `h`, `d`, or `w`
        #[arg(
            long,
            value_name = "AGE",
            default_value = "7d",
            value_parser = parse_age
        )]
        delete_older_than: chrono::Duration,
        /// Report what would be deleted without deleting it
        #[arg(long)]
        dry_run: bool,
        /// Consider every workspace, not just this directory's sessions
        #[arg(long)]
        all: bool,
    },
}

#[derive(clap::Subcommand, Debug)]
enum McpCommand {
    /// Speak the lofi exec bridge protocol on stdio.
    Serve,
}

#[derive(clap::Subcommand, Debug)]
enum DocsCommand {
    /// Search the embedded API reference.
    Search {
        /// The text to search for
        #[arg(value_name = "QUERY")]
        query: String,
    },
}

#[derive(clap::Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // parsed once, then matched by value
enum PolicyCommand {
    /// Evaluate a command against the shell policy without running it.
    Explain {
        /// The command to evaluate
        #[arg(value_name = "COMMAND")]
        command: String,
    },
    /// Calibrate the decision gate against commands or records.
    ///
    /// Replays the gate's evaluation log, judges a corpus, or judges
    /// saved records under the configured thresholds, and reports every
    /// outcome and miss.
    Calibrate {
        /// JSONL evaluation archives to rejudge, no network call; the
        /// default with no other input is every session's live log
        #[arg(long, value_name = "FILE")]
        replay: Vec<std::path::PathBuf>,
        /// A labeled corpus to send through the live decision model
        #[arg(long, value_name = "FILE")]
        probe: Option<std::path::PathBuf>,
        /// Saved JSONL records to judge offline
        #[arg(long, value_name = "FILE")]
        records: Vec<std::path::PathBuf>,
        /// Grid-search thresholds over the records
        #[arg(long)]
        sweep: bool,
        /// The decision model query; defaults to `auto_mode.decision_model`
        #[arg(long, value_name = "QUERY")]
        model: Option<String>,
        /// The request text the corpus commands answer
        #[arg(long, value_name = "TEXT")]
        request: Option<String>,
        /// The intent text `build_state` reads from the call
        #[arg(long, value_name = "TEXT")]
        intent: Option<String>,
        /// Write the judged records as JSONL
        #[arg(long, value_name = "FILE")]
        out: Option<std::path::PathBuf>,
        /// Send each corpus line this many times
        #[arg(long, value_name = "N", default_value_t = 1)]
        repeat: usize,
        /// Override the wide-scope-mass threshold
        #[arg(long, value_name = "F")]
        wide_scope_mass: Option<f64>,
        /// Override the destructive threshold
        #[arg(long, value_name = "F")]
        destructive: Option<f64>,
        /// Override the exposure threshold
        #[arg(long, value_name = "F")]
        exposure: Option<f64>,
        /// Override the authorized threshold
        #[arg(long, value_name = "F")]
        authorized: Option<f64>,
        /// Override the known threshold
        #[arg(long, value_name = "F")]
        known: Option<f64>,
    },
}

/// # Errors
/// Returns command parsing, configuration, I/O, or agent startup failures.
pub async fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = std::env::current_dir().context("determine working directory")?;

    // Apply --env specs to the process environment up front so every mode
    // (informational commands, print, interactive) observes them, and a bare
    // NAME that is unset in the parent errors before any work begins. The
    // guard is held for the whole process and restores prior values on exit.
    let env_pairs = cli
        .env
        .iter()
        .map(|spec| lofi_core::config_loader::parse_env_spec(spec))
        .collect::<Result<Vec<_>, lofi_error::Error>>()
        .map_err(anyhow::Error::new)?;
    let _env_guard = lofi_core::config_loader::apply_env_specs(&env_pairs);

    if let Some(command) = cli.command {
        run_command(command, &root).await?;
        return Ok(());
    }
    if let Some(prompt) = cli.print.clone() {
        let opts = build_print_opts(&cli, prompt, root);
        crate::run_print(opts).await?;
    } else {
        let opts = build_interactive_opts(&cli, root);
        Box::pin(crate::run_interactive(opts)).await?;
    }
    Ok(())
}

async fn run_command(command: Command, root: &std::path::Path) -> anyhow::Result<()> {
    match command {
        Command::Models => list_models().await?,
        Command::Sessions { all, cmd } => match cmd {
            None => list_sessions(root, all)?,
            Some(SessionCommand::Prune {
                delete_older_than,
                dry_run,
                all,
            }) => prune_sessions(root, delete_older_than, dry_run, all)?,
        },
        Command::Mcp {
            cmd: McpCommand::Serve,
        } => lofi_core::backend::run_stdio_proxy().await?,
        Command::Docs { cmd } => match cmd {
            None => print_docs_index()?,
            Some(DocsCommand::Search { query }) => print_docs_search(&query)?,
        },
        Command::Policy { cmd } => match cmd {
            PolicyCommand::Explain { command } => policy_explain(&command)?,
            PolicyCommand::Calibrate {
                replay,
                probe,
                records,
                sweep,
                model,
                request,
                intent,
                out,
                repeat,
                wide_scope_mass,
                destructive,
                exposure,
                authorized,
                known,
            } => {
                let args = lofi_core::calibrate::CalibrateArgs {
                    replay,
                    probe,
                    records,
                    sweep,
                    model,
                    request,
                    intent,
                    out,
                    repeat,
                    thresholds: lofi_types::DecisionThresholdsConfig {
                        wide_scope_mass,
                        destructive,
                        exposure,
                        authorized,
                        known,
                    },
                };
                let config_path = lofi_core::config_loader::user_config_path()
                    .context("resolve user config path")?;
                lofi_core::calibrate::calibrate(args, &config_path)
                    .await
                    .map_err(anyhow::Error::new)
                    .context("calibrate the decision gate")?;
            }
        },
    }
    Ok(())
}

fn policy_explain(cmd: &str) -> anyhow::Result<()> {
    let decision =
        lofi_core::config_loader::evaluate_shell_policy(cmd).context("evaluate shell policy")?;
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

fn build_print_opts(cli: &Cli, prompt: String, root: std::path::PathBuf) -> PrintOptions {
    let mut opts = PrintOptions::new(prompt, root);
    opts.backend.clone_from(&cli.backend);
    if let Some(m) = &cli.model {
        opts = opts.with_model(m);
    }
    if !cli.env.is_empty() {
        opts = opts.with_envs(cli.env.iter().map(String::as_str));
    }
    opts
}

fn build_interactive_opts(cli: &Cli, root: std::path::PathBuf) -> InteractiveOptions {
    let mut opts = InteractiveOptions::new(root);
    opts.backend.clone_from(&cli.backend);
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
    if !cli.env.is_empty() {
        opts = opts.with_envs(cli.env.iter().map(String::as_str));
    }
    opts
}

fn list_sessions(root: &std::path::Path, all: bool) -> anyhow::Result<()> {
    let sink = lofi_core::session::sink::SessionSink::open(root).context("open session sink")?;
    let entries = if all {
        sink.all_entries()
    } else {
        sink.workspace_entries()
    }
    .context("list sessions")?;
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    if entries.is_empty() {
        handle.write_all(b"(no sessions)\n")?;
    } else {
        for e in &entries {
            let cwd = if all {
                format!("  {}", e.meta.cwd)
            } else {
                String::new()
            };
            let line = format!(
                "{}  {}  {}  ({} msgs){cwd}\n",
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

/// Parse an age with a unit: `30m`, `12h`, `7d`, or `2w`. A bare number is
/// rejected, because the unit decides whether the user meant hours or days.
fn parse_age(text: &str) -> Result<chrono::Duration, String> {
    let (count, unit) = match text.find(|c: char| c.is_ascii_alphabetic()) {
        Some(at) => text.split_at(at),
        None => return Err(format!("missing a unit in {text}: use s, m, h, d, or w")),
    };
    let count: i64 = count
        .parse()
        .map_err(|_| format!("invalid count in {text}"))?;
    if count < 0 {
        return Err(format!("negative count in {text}"));
    }
    let age = match unit {
        "s" => chrono::Duration::try_seconds(count),
        "m" => chrono::Duration::try_minutes(count),
        "h" => chrono::Duration::try_hours(count),
        "d" => chrono::Duration::try_days(count),
        "w" => chrono::Duration::try_weeks(count),
        _ => {
            return Err(format!(
                "unknown unit {unit} in {text}: use s, m, h, d, or w"
            ))
        }
    };
    age.ok_or_else(|| format!("age is too large in {text}"))
}

/// The age as the user would write it back: the largest unit that divides
/// it exactly, so `7d` prints as `7d` and not `168h`. Weeks stay out, so
/// the default never comes back as `1w`.
fn age_label(age: chrono::Duration) -> String {
    let seconds = age.num_seconds().unsigned_abs();
    for (unit, size) in [("d", 86_400), ("h", 3_600), ("m", 60)] {
        if seconds >= size && seconds.is_multiple_of(size) {
            return format!("{}{unit}", seconds / size);
        }
    }
    format!("{seconds}s")
}

fn prune_sessions(
    root: &std::path::Path,
    older_than: chrono::Duration,
    dry_run: bool,
    all: bool,
) -> anyhow::Result<()> {
    let store = lofi_core::session::store::SessionStore::open().context("open session store")?;
    let files = if all {
        store.list_all_files().context("list sessions")?
    } else {
        store.list_files_for_cwd(root).context("list sessions")?
    };
    let cutoff = chrono::Utc::now() - older_than;
    let stale: Vec<_> = files
        .into_iter()
        .filter(|file| chrono::DateTime::<chrono::Utc>::from(file.last_active()) < cutoff)
        .collect();
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    for file in &stale {
        let workspace = if all {
            format!("  {}", file.meta().cwd)
        } else {
            String::new()
        };
        let line = format!(
            "{}  {}  {}{workspace}\n",
            file.id(),
            format_ts(file.meta().created),
            file.meta().model.label(),
        );
        handle.write_all(line.as_bytes())?;
    }
    let age = age_label(older_than);
    if stale.is_empty() {
        handle.write_all(format!("(no sessions older than {age})\n").as_bytes())?;
    } else if dry_run {
        let line = format!("would delete {} session(s) older than {age}\n", stale.len());
        handle.write_all(line.as_bytes())?;
    } else {
        for file in &stale {
            file.remove()
                .with_context(|| format!("delete session {}", file.id()))?;
        }
        let line = format!("deleted {} session(s) older than {age}\n", stale.len());
        handle.write_all(line.as_bytes())?;
    }
    Ok(())
}

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

fn print_docs_search(query: &str) -> anyhow::Result<()> {
    let res = lofi_core::docs::docs_search(query);
    let results = res["results"]
        .as_array()
        .context("malformed search results")?;
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

fn format_ts(ms: u64) -> String {
    i64::try_from(ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|at| at.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

async fn list_models() -> anyhow::Result<()> {
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
