use serde::{Deserialize, Serialize};

/// `call_id` avoids colliding with the flattened tree-level event `id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeToolRecord {
    pub parent: String,
    pub call_id: u64,
    pub name: String,
    pub args: String,
    pub result: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub enum CompactBlock {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    /// An exec tool call. `code` is the TypeScript source; `label` is the
    /// optional display name. The native tool calls that ran inside it are
    /// attached here so the brief transcript can show the real actions.
    ToolCall {
        id: String,
        code: String,
        label: Option<String>,
        native: Vec<NativeToolRecord>,
    },
    ToolResult {
        id: String,
        text: String,
        is_error: bool,
    },
}

impl CompactBlock {
    #[must_use]
    pub fn native_records(&self) -> &[NativeToolRecord] {
        match self {
            Self::ToolCall { native, .. } => native,
            _ => &[],
        }
    }
}

#[derive(Debug, Clone)]
pub struct SummarySection {
    pub title: String,
    pub items: Vec<String>,
}

pub trait CompactionHook: Send + Sync {
    fn sections(&self, blocks: &[CompactBlock]) -> Vec<SummarySection>;

    fn file_changes(&self, _blocks: &[CompactBlock]) -> Vec<String> {
        Vec::new()
    }

    fn commits(&self, _blocks: &[CompactBlock]) -> Vec<String> {
        Vec::new()
    }

    /// Hooks that know the structure of their tool results (e.g. JSON
    /// format) can extract meaningful fields instead of just taking the
    /// first line. Default returns `None` (caller falls back to its own
    /// generic compression).
    fn compress_tool_result(&self, _text: &str, _max: usize) -> Option<String> {
        None
    }

    /// Hooks that produce truncation notices (e.g. bash output redirected to
    /// a temp file) can return the path so the brief transcript can reference
    /// it. Default returns None.
    fn full_output_path(&self, _text: &str) -> Option<String> {
        None
    }
}
