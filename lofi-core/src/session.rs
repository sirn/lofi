//! Conversation persistence and the in-memory log.
//!
//! [`store`] owns the JSONL transcript files under the agent state dir, used
//! by the interactive TUI to auto-save and restore conversations.
//!
//! The transcript format is intentionally simple: the first line is a JSON
//! header ([`store::Header`]) carrying version/cwd/model/name metadata, and
//! every subsequent line is a serialized [`SessionEvent`] — a conversation
//! message, a tool-call timing, or a turn-end marker with cost/usage. The
//! engine appends these as a turn commits; the UI replays them, so live and
//! resumed state share one code path. Append-only writes make auto-save a
//! single `write_all` after each turn.

pub mod recorder;
pub mod store;
