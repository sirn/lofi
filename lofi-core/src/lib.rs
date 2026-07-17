//! `lofi-core`: the agent domain layer.
//!
//! Owns the agent loop, subagent orchestration, model registry, config
//! loading, and session/state bookkeeping. The provider transports live in
//! [`lofi_providers`] and the code-mode sandbox in [`lofi_code`]; this crate
//! coordinates them as a Service Layer. Presentation (TUI + `--print`) lives
//! in `lofi-ui`.

pub mod agent;
pub mod compact;
pub mod context_edit;
pub mod recall;
pub mod config_loader;
pub mod models;
pub mod retry;
pub mod session;
pub mod state;
pub mod subagent;

pub use agent::{build_agent, exec_input_code_and_label, exec_result_display, rebuild_agent, select_model, Agent, AgentEvent, SessionCommit};
pub use compact::{compact, compacted_history, Compaction, CompactOptions, HANDOFF_PREAMBLE};
pub use lofi_error::{Error, Result};
pub use lofi_types::{Config, SessionEvent};
pub use models::ModelRegistry;
pub use retry::{is_retryable_error, RetryPolicy};
