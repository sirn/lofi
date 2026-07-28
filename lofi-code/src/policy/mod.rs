//! The engine parses bash command strings structurally (never string-matching
//! the raw input), evaluates every sub-command against an allow/ask/deny
//! policy table, and returns a Decision. Fails closed to ask on parse
//! errors or unmatched commands.

pub mod auto_mode;
pub mod defaults;
pub mod engine;
pub mod extract;
pub mod token;

pub use engine::{Decision, ResolvedPolicy};
pub use extract::WrapperRuleMap;
