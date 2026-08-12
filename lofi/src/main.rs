// Mimalloc is the global allocator. Glibc malloc lazily reserves a
// 64 MB arena per allocating thread and never returns those pages, so
// background bursts (transcript snapshots, compaction, sandbox runtime)
// balloon RSS. Mimalloc uses thread-local segments and is aggressive
// about releasing memory back to the OS. We opt out of the default
// "secure" feature to keep this a pure allocator swap.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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
