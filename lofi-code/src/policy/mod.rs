//! Shell policy engine: tokenize, extract, evaluate.
//!
//! The engine parses bash command strings structurally (never string-matching
//! the raw input), evaluates every sub-command against an allow/ask/deny
//! policy table, and returns a Decision. Fails closed to ask on parse
//! errors or unmatched commands.
//!
//! See `defaults::resolve` to build a `ResolvedPolicy` from config,
//! then call policy.evaluate(cmd).

pub mod defaults;
pub mod engine;
pub mod extract;
pub mod token;

pub use engine::{Decision, ResolvedPolicy};
pub use extract::WrapperRuleMap;