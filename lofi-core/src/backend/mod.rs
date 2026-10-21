//! Backends own the model loop. Tool execution stays session-scoped in Lofi,
//! so an HTTP provider and a CLI child run through the same interface.
mod claude;
mod claude_engine;
mod contract;
mod mcp;
mod native;
mod oneshot;
mod resolve;
mod rpc;
mod session;

pub use contract::{Backend, BackendRef, Completion, CompletionRequest, ModelSelection};
pub use native::NativeSource;
pub(crate) use resolve::parse_query;
pub use resolve::BackendFactory;
pub use session::configured_models;

pub use mcp::run_stdio_proxy;
