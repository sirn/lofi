#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Cap glibc malloc arenas. Background work (transcript snapshots,
    // compaction, the sandbox runtime) runs on spawned threads, and glibc
    // lazily creates a 64 MB-reserved arena per allocating thread that it
    // never returns. Under the default 8-arenas-per-core cap a single agent
    // turn or compaction can retain tens of MB in idle arenas even though the
    // application's measured footprint is only a few MB. Limiting to two
    // arenas keeps the burst transient without materially serializing the
    // mostly-I/O-bound background tasks. This must run before any worker
    // thread allocates, hence first in main; glibc reads it lazily at arena
    // creation. Safe here because main is still single-threaded.
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        std::env::set_var("MALLOC_ARENA_MAX", "2");
    }

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
