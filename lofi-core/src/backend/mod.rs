//! Agent backends own model context. Tool execution remains session-scoped in Lofi.
mod claude;
mod mcp;
mod rpc;
mod session;
pub use session::{saved_backend, saved_model, saved_model_label, Backend};

pub use mcp::run_stdio_proxy;
