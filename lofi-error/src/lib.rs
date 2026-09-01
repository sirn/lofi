use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderPhase {
    ResponseStart,
    ResponseBody,
    ErrorResponseBody,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderTransportKind {
    Network,
    RequestSetup,
    Redirect,
}

impl std::fmt::Display for ProviderTransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network => f.write_str("network transport"),
            Self::RequestSetup => f.write_str("request setup"),
            Self::Redirect => f.write_str("redirect"),
        }
    }
}

impl std::fmt::Display for ProviderPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ResponseStart => f.write_str("waiting for connection and response headers"),
            Self::ResponseBody => f.write_str("waiting for the next response body chunk"),
            Self::ErrorResponseBody => {
                f.write_str("waiting for the next provider error body chunk")
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http error: {0}")]
    Http(String),

    /// A config load, parse, or value-resolution failure. Carries a
    /// human-readable message rather than a typed cause so the same variant
    /// covers missing files, bad TOML, and unresolved `$VAR`/`!cmd` values.
    #[error("config error: {0}")]
    Config(String),

    /// No model is available — no provider has credentials and none is
    /// `no_auth`. Distinct from [`Config`](Self::Config) so the interactive
    /// UI can surface a friendly "no models configured" message instead of
    /// a hard error, while `--print` still exits non-zero.
    #[error("{0}")]
    NoModels(String),

    #[error("state error: {0}")]
    State(String),

    #[error("sandbox error: {0}")]
    Sandbox(String),

    #[error("provider error: {0}")]
    Provider(String),

    #[error("provider timeout after {timeout_ms}ms while {phase}")]
    ProviderTimeout {
        phase: ProviderPhase,
        timeout_ms: u64,
    },

    #[error("provider {kind} failed while {phase}: {detail}")]
    ProviderTransport {
        kind: ProviderTransportKind,
        phase: ProviderPhase,
        detail: String,
    },

    #[error("provider HTTP {status} from {endpoint}: {detail}")]
    ProviderStatus {
        status: u16,
        endpoint: String,
        detail: String,
    },

    #[error("tool error: {0}")]
    Tool(String),

    /// The operation was cancelled because the event consumer went away
    /// (receiver dropped or closed). Translated to a graceful exit at the
    /// public `Agent::run` boundary.
    #[error("cancelled: consumer closed")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, Error>;
