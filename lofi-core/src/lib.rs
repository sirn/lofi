pub mod agent;
mod bash_env;

pub use lofi_code::docs;
pub mod compact;
pub mod config_loader;
pub mod context_edit;
pub mod image;
mod lifecycle;
pub mod malloc_trim;
pub mod models;
pub mod recall;
pub mod retry;
pub mod session;
mod shell;
pub mod state;

pub use agent::{
    build_agent, exec_input_code_and_label, exec_result_display, rebuild_agent, select_model,
    Agent, AgentEvent, ConfirmRequest,
};
pub use compact::{compact, compacted_history, CompactOptions, Compaction, HANDOFF_PREAMBLE};
pub use lifecycle::{AgentLifecycle, HardCompactOutcome, LineageJobReconciliation};
pub use lofi_code::compact_hook::CodeCompactionHook;
pub use lofi_code::tools::{
    JobAttributes, JobColor, JobInfo, JobRegistry, JobScreen, JobScreenLine, JobSpan, JobStyle,
};
pub use lofi_code::ConfirmReason;
pub use lofi_error::{Error, Result};
pub use lofi_types::{CompactBlock, CompactionHook, SummarySection};
pub use lofi_types::{Config, SessionEvent};
pub use models::ModelRegistry;
pub use retry::{is_retryable_error, RetryPolicy};
pub use shell::{cancelled_user_shell, run_user_shell_command, UserShellResult};
