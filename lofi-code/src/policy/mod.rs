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

/// The startup approval mode in effect when no `/policy` pick was made:
/// `AskAuto` when auto mode is configured, otherwise `AskManual`.
#[must_use]
pub fn default_mode(auto_available: bool) -> lofi_types::BashApprovalMode {
    if auto_available {
        lofi_types::BashApprovalMode::AskAuto
    } else {
        lofi_types::BashApprovalMode::AskManual
    }
}

/// Shared handle to the session-scoped approval mode picked in `/policy`.
/// The override never persists.
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

    /// The picked mode, or the startup default when no pick was made.
    #[must_use]
    pub fn effective(&self, auto_available: bool) -> lofi_types::BashApprovalMode {
        self.current()
            .unwrap_or_else(|| default_mode(auto_available))
    }
}
