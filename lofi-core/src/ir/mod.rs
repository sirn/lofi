//! Translation layer between `lofi_types` content and each provider's wire
//! shape.
//!
//! Mirrors fabric's `ir/` split: HTTP transport lives in [`crate::providers`],
//! request/response mapping lives here. All functions in this module are pure
//! — no HTTP, no async.

pub mod anthropic_messages;
pub mod block;
pub mod chat;
pub mod codec;
pub mod openai_completions;
pub mod openai_responses;

pub use anthropic_messages::{build_anthropic_request, map_anthropic_event};
pub use block::{to_anthropic_request_parts, to_openai_chat_messages, to_openai_responses_input};
pub use chat::{build_request, ToolSchema};
pub use codec::{assemble_message, is_done_marker, parse_sse, parse_sse_lines, SseEvent};
pub use openai_completions::{build_openai_chat_request, map_openai_chat_event};
pub use openai_responses::{build_openai_responses_request, map_openai_responses_event};
