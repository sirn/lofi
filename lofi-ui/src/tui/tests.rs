#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::wildcard_imports)]
#![allow(clippy::many_single_char_names)]
#![allow(clippy::float_cmp)]

use super::*;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use lofi_providers::{Provider, ToolSchema};
use lofi_types::{ContentBlock, Message, Model, Role, StreamingEvent, Usage};
use ratatui::backend::TestBackend;

mod support;
use support::*;

mod compaction_resume;
mod input_and_modals;
mod jobs_and_theme;
mod navigation;
mod pickers;
mod reliability;
mod rendering;
mod resize;
mod session_lifecycle;
mod session_tree;
mod status_and_replay;
mod streaming;
