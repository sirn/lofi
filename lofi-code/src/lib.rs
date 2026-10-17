//! Tool closures return an owned `ToolOutput`. Its `IntoJs` implementation
//! recursively converts successful JSON values and throws a genuine JavaScript
//! `Error` for failures. This avoids rquickjs's generic Rust-conversion error
//! wording while retaining the leak-free owned-value bridge. The IIFE wrapper
//! catches uncaught tool errors and resolves them as a structured
//! `{ __lofi_sandbox_error__ }` result; resolving avoids rquickjs 0.9 retaining
//! a rejected promise's value through runtime shutdown.

mod bind;
pub mod compact_hook;
mod compile;
mod contract;
mod convert;
mod ctx;
pub mod docs;
pub mod memory;
pub mod policy;
mod run;
mod skill_metadata;
pub mod tools;
pub mod user_shell;
mod worker;

#[cfg(test)]
mod tests;

pub use compile::compile_ts;
pub use contract::{exec_tool_input_schema, EXEC_TOOL_DESCRIPTION, EXEC_TOOL_NAME};
pub use ctx::{
    AutoModeFn, AutoModeOutcome, ConfirmFn, ConfirmPrompt, ConfirmReason, ExecCtx, ExecImage,
    ExecOptions, ExecResult, JobAcquireFn, JobReleaseFn, ModelsFn, RecallFn, ResultFn, SubagentFn,
    SubagentOp, ToolEvent, DEFAULT_GUEST_TIMEOUT,
};
pub use run::exec;
pub use tools::truncate::TruncatedCap;
pub use tools::BashEnv;
pub use worker::ExecWorker;
