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

/// Shared handle to the session-scoped approval mode picked in `/policy`.
/// `None` is the startup default: `AskAuto` when auto mode is configured,
/// otherwise `AskManual`. The override never persists.
#[derive(Debug, Clone, Default)]
pub struct PolicyOverride(std::sync::Arc<std::sync::Mutex<Option<lofi_types::BashApprovalMode>>>);

impl PolicyOverride {
    /// Apply a dialog choice. `None` restores the startup default.
    pub fn set(&self, mode: Option<lofi_types::BashApprovalMode>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = mode;
    }

    #[must_use]
    pub fn current(&self) -> Option<lofi_types::BashApprovalMode> {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
