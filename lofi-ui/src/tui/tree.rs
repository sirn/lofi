//! Tree projection lives in `lofi_core::session::tree`; this shim keeps the
//! TUI's historical import sites working without duplicating the logic.

#[cfg(test)]
pub(super) use lofi_core::session::tree::hydrate_tree_window as hydrate_tree_entry_window;
pub(super) use lofi_core::session::tree::{
    build_tree_row_skeletons as build_tree_entry_skeletons, build_tree_rows as build_tree_entries,
    hydrate_tree_rows as hydrate_tree_entry_rows, load_prompt_text,
};
