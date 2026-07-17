//! Tool bundle. The code-mode sandbox binds [`builtins::BuiltinTools`]
//! methods directly onto the guest `pi` object; no trait dispatch needed.

pub mod builtins;

pub use builtins::BuiltinTools;
