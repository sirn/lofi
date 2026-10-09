#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // glibc reads MALLOC_ARENA_MAX before main, so setting the environment
    // here cannot limit arenas. Apply the limit before spawning workers to
    // keep concurrent file reads from retaining an arena per thread.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: mallopt locks the allocator while changing its arena
            // limit. Existing allocations remain valid.
            nix::libc::mallopt(nix::libc::M_ARENA_MAX, 2);
        }
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
