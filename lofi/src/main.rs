//! lofi process entry point.

// Rendering expanded tool output can create a large, short-lived allocation
// burst. glibc commonly keeps those freed pages in its arenas, making a
// `/verbose` toggle look like permanent application growth. mimalloc returns
// abandoned pages more eagerly and keeps settled RSS close to live state.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// The TUI and QuickJS sandbox already run on a LocalSet. A current-thread
// runtime also prevents parallel subagents from spreading large transient
// allocations across glibc's per-worker arenas, which otherwise retain tens
// of MiB after the subagents finish. Network and process I/O remain async;
// explicit spawn_blocking work still uses Tokio's blocking pool.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // The TUI owns the alternate screen, so tracing stays opt-in to avoid
    // interleaving log output with rendered frames.
    if std::env::var_os("RUST_LOG").is_some() {
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .init();
    }

    lofi_ui::run_cli().await
}
