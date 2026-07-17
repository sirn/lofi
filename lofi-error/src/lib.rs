//! Error type for `lofi-core`.
//!
//! A single enum covers all fallible surfaces in the crate. Provider/tool/state
//! failures carry a free-form message; the I/O, HTTP, and config-decode failures
//! use `#[from]` for ergonomic `?` propagation. A boxed `dyn Error` fallback
//! keeps room for one-off causes without bloating the enum.

use thiserror::Error;

/// The error type returned by `lofi-core` operations.
#[derive(Debug, Error)]
pub enum Error {
    /// A filesystem or std I/O failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An HTTP transport failure.
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    /// A config load, parse, or value-resolution failure. Carries a
    /// human-readable message rather than a typed cause so the same variant
    /// covers missing files, bad TOML, and unresolved `$VAR`/`!cmd` values.
    #[error("config error: {0}")]
    Config(String),

    /// No model is available — no provider has credentials and none is
    /// `no_auth`. Distinct from [`Config`](Self::Config) so the interactive
    /// UI can surface a friendly "no models configured" message instead of
    /// a hard error, while `--print` still exits non-zero.
    #[error("no models configured: {0}")]
    NoModels(String),

    /// An agent-owned state-tree failure.
    #[error("state error: {0}")]
    State(String),

    /// A sandbox compilation or runtime failure.
    #[error("sandbox error: {0}")]
    Sandbox(String),

    /// A provider-level failure (bad status, malformed stream, etc.).
    #[error("provider error: {0}")]
    Provider(String),

    /// A tool execution failure.
    #[error("tool error: {0}")]
    Tool(String),

    /// The operation was cancelled because the event consumer went away
    /// (receiver dropped or closed). Translated to a graceful exit at the
    /// public `Agent::run` boundary.
    #[error("cancelled: consumer closed")]
    Cancelled,

    /// A fallback for errors that don't merit their own variant.
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Convenience `Result` alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
