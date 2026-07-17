//! lofi process entry point.

#[tokio::main]
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
