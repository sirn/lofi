//! `lofi-core`: all behavior for the lofi coding agent.
//!
//! This crate wires the agent loop, the code-mode `TypeScript` sandbox, the
//! provider HTTP transports, and the terminal UI. It depends on [`lofi_types`]
//! for the shared data shapes. Only the module tree and the public surface are
//! established here; each submodule is stubbed for now and filled in by later
//! steps.

pub mod agent;
pub mod code;
pub mod config_loader;
pub mod error;
pub mod ir;
pub mod models;
pub mod providers;
pub mod session;
pub mod state;
pub mod subagent;
pub mod tools;
pub mod tui;

pub use agent::{run_interactive, run_print, Agent, AgentEvent, InteractiveOptions, PrintOptions};
pub use lofi_types::Config;
pub use models::ModelRegistry;
