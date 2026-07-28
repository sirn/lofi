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
    #[arg(short = 'p', long, value_name = "PROMPT")]
    print: Option<String>,
    #[arg(long)]
    list_models: bool,
    #[arg(long)]
    list_sessions: bool,
    #[arg(long, value_name = "SPEC")]
    model: Option<String>,
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    #[arg(long)]
    no_session: bool,
    #[arg(long)]
    docs: bool,
    #[arg(long, value_name = "QUERY")]
    docs_search: Option<String>,
    #[arg(long, value_name = "COMMAND")]
    policy_explain: Option<String>,
}

///
/// # Errors
/// Returns command parsing, configuration, I/O, or agent startup failures.
pub async fn run_cli() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let root = std::env::current_dir().context("determine working directory")?;

    if cli.list_models {
        list_models().await?;
    } else if cli.list_sessions {
        list_sessions(&root)?;
    } else if cli.docs {
        print_docs_index()?;
    } else if let Some(query) = cli.docs_search.clone() {
        print_docs_search(&query)?;
    } else if let Some(cmd) = cli.policy_explain.clone() {
        policy_explain(&cmd)?;
    } else if let Some(prompt) = cli.print.clone() {
        let opts = build_print_opts(&cli, prompt, root);
        crate::run_print(opts).await?;
    } else {
        let opts = build_interactive_opts(&cli, root);
        crate::run_interactive(opts).await?;
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
    if let Some(m) = &cli.model {
        opts = opts.with_model(m);
    }
    opts
}

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

fn list_sessions(root: &std::path::Path) -> anyhow::Result<()> {
    let store = lofi_core::session::store::SessionStore::open().context("open session store")?;
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

/// Format a wall-clock millisecond timestamp as UTC `YYYY-MM-DD HH:MM`.
#[allow(clippy::many_single_char_names)] // mirrors Howard Hinnant's civil calendar algorithm
fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    let day = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem % 3600) / 60;
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
