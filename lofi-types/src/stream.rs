use serde::{Deserialize, Serialize};

use crate::PartSignatureFormat;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingEvent {
    TextDelta(String),
    ToolUseStart {
        id: String,
        name: String,
    },
    ToolUseInputDelta {
        id: String,
        delta: String,
    },
    /// Canonical complete tool input from a terminal provider event.
    /// Replaces any input previously accumulated from deltas for this call.
    ToolUseInputComplete {
        id: String,
        input: String,
    },
    ToolUseEnd {
        id: String,
    },
    ThinkingDelta(String),
    ThinkingSignature(String),
    /// Anthropic `redacted_thinking`: the server withheld the chain-of-thought
    /// body and returned an opaque blob instead. `data` is the value of
    /// `content_block.data` and echoes back as `redacted_thinking` on later
    /// turns. Emitted on `content_block_start`; no deltas follow for this
    /// block.
    ThinkingRedacted {
        data: String,
    },
    /// Opaque signature attached to a streamed provider part. Tool-call
    /// signatures carry a target because compatible streams can deliver
    /// parallel call metadata after a different call became current.
    PartSignature {
        provider: String,
        model: String,
        format: PartSignatureFormat,
        target: Option<String>,
        signature: String,
    },
    Done {
        usage: Usage,
        /// Why the provider stopped generating, when it reported one.
        /// `None` for providers or frames that carry no stop metadata.
        #[serde(default)]
        stop_reason: Option<StopReason>,
    },
    Error(String),
}

/// Provider-reported reason generation stopped, normalized across APIs.
/// Anthropic `stop_reason`, `OpenAI` `finish_reason`/`status`, and Google
/// `finishReason` all map onto these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished on its own (`end_turn`, `stop`, `STOP`).
    EndTurn,
    /// Output was cut off by a token limit (`max_tokens`, `length`,
    /// `MAX_TOKENS`, Responses `incomplete` with `max_output_tokens`).
    MaxTokens,
    /// Generation stopped to run tools (`tool_use`, `tool_calls`).
    ToolUse,
    /// Any other provider-specific reason, kept opaque.
    Other,
}

/// Backend-reported context state, separate from billed token totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextUsage {
    pub used: u64,
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    /// Context-window fill from the just-finished round: everything the next
    /// request must carry — fresh input plus cache reads and writes, then
    /// this round's output. Cache writes count like reads: the written
    /// prefix occupies the window (and is re-sent or re-billed) from the
    /// next request on, so gauges and compaction thresholds must see it.
    #[must_use]
    pub fn context_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}
