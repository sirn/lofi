pub mod agent;
mod bash_env;

pub use lofi_code::docs;
pub mod compact;
pub mod config_loader;
pub mod context_edit;
mod lifecycle;
pub mod models;
pub mod recall;
pub mod retry;
pub mod session;
pub mod state;
mod user_bash;

pub use agent::{
    build_agent, exec_input_code_and_label, exec_result_display, rebuild_agent, select_model,
    Agent, AgentEvent, ConfirmRequest,
};
pub use compact::{compact, compacted_history, CompactOptions, Compaction, HANDOFF_PREAMBLE};
pub use lifecycle::{AgentLifecycle, HardCompactOutcome};
pub use lofi_code::compact_hook::CodeCompactionHook;
pub use lofi_code::ConfirmReason;
pub use lofi_error::{Error, Result};
pub use lofi_types::{CompactBlock, CompactionHook, SummarySection};
pub use lofi_types::{Config, SessionEvent};
pub use models::ModelRegistry;
pub use retry::{is_retryable_error, RetryPolicy};
pub use user_bash::{cancelled_user_bash, run_user_bash, UserBashResult};
