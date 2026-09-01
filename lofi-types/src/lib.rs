#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub mod recall;
pub mod text;

pub use text::clip;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Api {
    #[serde(rename = "openai_completions", alias = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai_responses", alias = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic_messages", alias = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "google_generative_ai", alias = "google-generative-ai")]
    GoogleGenerativeAi,
}

impl Api {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GoogleGenerativeAi => "google_generative_ai",
        }
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerativeAi => "google-generative-ai",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "OpenAI Chat Completions",
            Self::OpenAiResponses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
            Self::GoogleGenerativeAi => "Google Generative AI",
        }
    }

    /// Returns `None` for unrecognized strings rather than erroring, so callers
    /// can attach their own diagnostic context.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai_completions" | "openai-completions" => Some(Self::OpenAiCompletions),
            "openai_responses" | "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic_messages" | "anthropic-messages" => Some(Self::AnthropicMessages),
            "google_generative_ai" | "google-generative-ai" => Some(Self::GoogleGenerativeAi),
            _ => None,
        }
    }

    /// Default API **host root** for this protocol, used when a provider
    /// entry omits `base_url`. The full endpoint URL is built by joining
    /// [`Self::default_path`] onto this root, so the root never carries an
    /// API-version segment — that lives in the path. Kept here so the config
    /// loader can fill it in without a hardcoded table elsewhere.
    #[must_use]
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiCompletions | Self::OpenAiResponses => "https://api.openai.com",
            Self::AnthropicMessages => "https://api.anthropic.com",
            Self::GoogleGenerativeAi => "https://generativelanguage.googleapis.com",
        }
    }

    #[must_use]
    pub fn default_path(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
            Self::AnthropicMessages => "/v1/messages",
            Self::GoogleGenerativeAi => "/v1beta",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ThinkingLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
    XHigh,
    /// Provider-defined effort retained verbatim for forward compatibility.
    Custom(String),
}

impl ThinkingLevel {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Custom(value) => value,
        }
    }
}

impl Serialize for ThinkingLevel {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ThinkingLevel {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() {
            Err(serde::de::Error::custom("thinking level cannot be empty"))
        } else {
            Ok(Self::parse(&value).unwrap_or(Self::Custom(value)))
        }
    }
}

/// Provider service tier for a request (e.g. `OpenAI`'s `flex` or `priority`).
/// `Auto` omits the field so the provider uses its default. `Custom` retains
/// provider-defined tiers verbatim for forward compatibility.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ServiceTier {
    #[default]
    Auto,
    Flex,
    Priority,
    /// Provider-defined tier retained verbatim for forward compatibility.
    Custom(String),
}

impl ServiceTier {
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "flex" => Some(Self::Flex),
            "priority" => Some(Self::Priority),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        match self {
            Self::Auto => "auto",
            Self::Flex => "flex",
            Self::Priority => "priority",
            Self::Custom(value) => value,
        }
    }
}

impl Serialize for ServiceTier {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ServiceTier {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value.is_empty() {
            Err(serde::de::Error::custom("service tier cannot be empty"))
        } else {
            Ok(Self::parse(&value).unwrap_or(Self::Custom(value)))
        }
    }
}

/// Where a turn's prompt came from. Typed input is the default; anything
/// else is an app-injected notice (background-job completions now, more
/// automation later). Consumers use it to style externally-triggered turns
/// distinctly from user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    #[default]
    User,
    Notice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

/// Internally tagged by `type` so each variant round-trips through JSON without
/// ambiguity between text, tool calls, tool results, and reasoning traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
        /// Images attached to this tool result (e.g. a `read` of an image
        /// file). Each entry is `(bytes, media_type)`, base64-encoded at the
        /// serde and per-provider IR boundaries like the standalone `Image`
        /// block. Carrying images here (rather than on a separate user
        /// message) lets the model see the image in the same round that
        /// produced the result, on every provider. Empty for text-only
        /// results; omitted from the wire form when empty.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<ToolResultImage>,
    },
    /// Chain-of-thought / reasoning trace (where the API exposes it).
    /// `signature` is provider-specific replay metadata: Anthropic's thinking
    /// signature, Responses `encrypted_content`, or — on chat-completions
    /// plaintext reasoning streams — the delta field name the trace arrived
    /// under so the next request can replay it to the same key. `redacted`
    /// marks Anthropic redacted thinking: text is a placeholder, signature
    /// is the opaque blob returned as `redacted_thinking.data`.
    Thinking {
        text: String,
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        redacted: bool,
    },
    /// Opaque metadata for the preceding provider part. Keeping it adjacent
    /// lets each provider IR restore the signature to the exact wire part.
    PartSignature {
        provider: String,
        model: String,
        format: PartSignatureFormat,
        signature: String,
    },
    /// An attached image, held as raw bytes plus its media type and only
    /// base64-encoded at the serde and per-provider IR boundaries. Encoding
    /// on demand keeps the in-memory block and the durable transcript free
    /// of a duplicated 33%-larger base64 copy.
    Image {
        #[serde(with = "base64_bytes")]
        bytes: Vec<u8>,
        media_type: String,
    },
}

/// An image attached to a [`ContentBlock::ToolResult`]. Raw bytes plus media
/// type, base64-encoded in serde via the shared `base64_bytes` adapter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResultImage {
    #[serde(with = "base64_bytes")]
    pub bytes: Vec<u8>,
    pub media_type: String,
}

/// Serde adapter that encodes a byte buffer as a base64 string in JSON, so
/// [`ContentBlock::Image`] round-trips through the tagged-union transcript
/// format alongside the text variants.
mod base64_bytes {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    /// Origin of a user-role prompt. Distinguishes typed input from
    /// app-injected notices so replay does not need a separate marker
    /// event. Defaults to `User` so older transcripts remain loadable.
    #[serde(default, skip_serializing_if = "is_default_prompt_kind")]
    pub kind: PromptKind,
}

// `skip_serializing_if` requires a `&T` signature; `PromptKind` is `Copy` but
// serde's contract fixes the parameter form.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_default_prompt_kind(kind: &PromptKind) -> bool {
    *kind == PromptKind::User
}

// Same serde contract for boolean defaults: the predicate must take a
// reference, so the body inverts.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PartSignatureFormat {
    Google,
    OpenAiExtraContent { namespace: String },
    OpenAiReasoningDetail,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    Message(Message),
    UserShell {
        command: String,
        output: String,
        exit_code: Option<i32>,
        #[serde(default)]
        signal: Option<i32>,
        duration_ms: u64,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        cancelled: bool,
        #[serde(default)]
        exclude_from_context: bool,
    },
    /// Wall-clock duration of a completed tool call within a turn, so the
    /// exec block's `took Ns` marker survives resume. `tool_call_id` is the
    /// provider tool-call id; named `tool_call_id` (not `id`) so it does not
    /// collide with the tree-level `id` on [`SessionEvent`] when flattened.
    ToolTiming {
        tool_call_id: String,
        elapsed_ms: u64,
    },
    /// Wall-clock duration of a completed thinking block within a turn, so the
    /// "Thought for Ns" marker survives freeze/resume. Emitted in order, one
    /// per assistant thinking block.
    ThinkingTiming {
        elapsed_ms: u64,
    },
    TurnEnd {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
        /// Stop reason reported for the final round of the turn — the round
        /// that actually ended it. Persisted so finished transcripts
        /// accumulate stop-reason evidence (e.g. how often turns end on a
        /// clean stop versus a token cap).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },
    /// A mid-turn round the engine dropped from the model history (loop
    /// detection, retry re-roll). Its messages were already streamed, so
    /// they stay visible while context rebuild skips the assistant
    /// message(s) this marker ends with.
    RoundDiscarded {
        detail: String,
    },
    /// A turn that ended in a non-retryable provider/runtime failure.
    /// Its partial messages stay on the visible lineage, but context rebuilds
    /// skip them so retrying begins from the preceding successful checkpoint.
    TurnFailed {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        error: String,
        cost: f64,
        usage: Usage,
    },
    /// A turn explicitly interrupted by the user.
    /// message, completed rounds and the partial current response remain both
    /// visible and available to subsequent model turns.
    TurnCancelled {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
    },
    /// A native tool call that ran inside an `exec` block, so the nested
    /// `lofi.<tool>` call list survives resume.
    NativeTool(NativeToolRecord),
    Cursor {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        leaf_id: Option<String>,
    },
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        summarized_range: [String; 2],
        /// True when the kept tail was copied immediately before this marker
        /// as a durable, context-edited checkpoint. UI replay suppresses those
        /// copies (the original turns remain visible); model-history replay
        /// reads them as the authoritative post-compaction tail. Older marker
        /// layouts default to false.
        #[serde(default)]
        checkpointed_tail: bool,
        summarized: usize,
        #[serde(default)]
        represented: usize,
        kept: usize,
    },
    /// Lineage marker for `jobSpawn`. Invisible to the model and
    /// transcript renderer.
    JobStarted {
        job_id: u64,
    },
    /// Paired with [`Self::JobStarted`] by id; a start with no matching
    /// finish on the visible lineage was still running when the session
    /// last ended.
    JobFinished {
        job_id: u64,
    },
    /// An event variant this build does not know about — typically a marker
    /// written by a newer (or older, pre-release) binary. The transcript
    /// stays loadable; consumers ignore these.
    #[serde(other)]
    Unknown,
}

/// One append-only line in a session transcript log.
/// The first line of a session file is the session header (written by the
/// store); every subsequent line is a `SessionEvent`. Events form a tree via
/// `id`/`parent_id`: each entry points at its parent, the root entry's
/// `parent_id` is `None`, and the "active leaf" is the current position in
/// the tree. Branching appends a new child to an earlier entry instead of to
/// the previous line, so alternatives coexist in one file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(flatten)]
    pub kind: SessionEventKind,
}

#[derive(Debug, Clone)]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub thinking_levels: Vec<ThinkingLevel>,
    pub service_tiers: Vec<ServiceTier>,
    pub supports_image: bool,
    pub context_window: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub api: Api,
    /// Whether the model exposes a reasoning/Thinking trace.
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default)]
    pub service_tier: ServiceTier,
    #[serde(default)]
    pub supports_image: bool,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Per-model endpoint base, set by auto-discovery when an api-type mapping
    /// carries a path (e.g. an OpenAI-compatible proxy routing one base URL to several
    /// upstream APIs). When `None`, the provider's `base_url` is used.
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub input_price: Option<f64>,
    #[serde(default)]
    pub output_price: Option<f64>,
    /// Cache-read price per 1M tokens (USD). When set, cached prompt tokens
    /// are billed at this rate instead of the input rate.
    #[serde(default)]
    pub cache_read_price: Option<f64>,
    /// Cache-write price per 1M tokens (USD). When set, cache-creation tokens
    /// are billed at this rate instead of the input rate.
    #[serde(default)]
    pub cache_write_price: Option<f64>,
    #[serde(default)]
    pub per_request_price: Option<f64>,
}

/// The model identity that ran a turn (or started a session): raw data, no
/// presentation. Persisted on turn-end markers and the session header so a
/// resumed session renders the turn's original model rather than the
/// (possibly switched) active one. Rendered to `provider/id:level` only at
/// display time — never stored as a formatted string, so a later UI change
/// can't strand stale text in old session files.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RunModel {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default)]
    pub service_tier: ServiceTier,
}

impl RunModel {
    /// A model query that round-trips through `parse_model_query`: unlike
    /// `label` it always emits the thinking level so a restored `off` level
    /// is not silently replaced by the model's default.
    #[must_use]
    pub fn query(&self) -> String {
        format!(
            "{}/{}:{}{}",
            self.provider,
            self.id,
            self.thinking.as_str(),
            if self.service_tier == ServiceTier::Auto {
                String::new()
            } else {
                format!("@{}", self.service_tier.as_str())
            }
        )
    }

    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{}/{}{}{}",
            self.provider,
            self.id,
            if self.thinking == ThinkingLevel::Off {
                String::new()
            } else {
                format!(":{}", self.thinking.as_str())
            },
            if self.service_tier == ServiceTier::Auto {
                String::new()
            } else {
                format!("@{}", self.service_tier.as_str())
            }
        )
    }

    #[must_use]
    pub fn parse(s: &str) -> Self {
        let (provider, rest) = match s.split_once('/') {
            Some((p, r)) => (p.to_string(), r),
            None => (String::new(), s),
        };
        // Optional trailing `@tier`, mirroring core's model query: any
        // non-empty tail is a tier (unknown values stay provider-defined
        // Custom tiers so labels round-trip).
        let (core, tier) = match rest.rsplit_once('@') {
            Some((head, tail)) => {
                let tail = tail.trim();
                if tail.is_empty() {
                    (rest.to_string(), ServiceTier::Auto)
                } else {
                    (
                        head.to_string(),
                        ServiceTier::parse(tail)
                            .unwrap_or_else(|| ServiceTier::Custom(tail.to_string())),
                    )
                }
            }
            None => (rest.to_string(), ServiceTier::Auto),
        };
        let (id, thinking) = if let Some((i, lvl)) = core.split_once(" · ") {
            (
                i.to_string(),
                ThinkingLevel::parse(lvl.trim()).unwrap_or_default(),
            )
        } else if let Some((i, lvl)) = core.rsplit_once(':') {
            // Only treat the suffix as a thinking level when it parses;
            // otherwise the colon is part of the model id (core's model
            // query allows colons in ids) and must be preserved.
            match ThinkingLevel::parse(lvl.trim()) {
                Some(t) => (i.to_string(), t),
                None => (core.clone(), ThinkingLevel::Off),
            }
        } else {
            (core.clone(), ThinkingLevel::Off)
        };
        Self {
            provider,
            id,
            thinking,
            service_tier: tier,
        }
    }
}

impl From<&str> for RunModel {
    fn from(s: &str) -> Self {
        Self::parse(s)
    }
}

impl From<String> for RunModel {
    fn from(s: String) -> Self {
        Self::parse(&s)
    }
}

impl<'de> serde::Deserialize<'de> for RunModel {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::Deserialize;
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Obj {
                provider: String,
                id: String,
                #[serde(default)]
                thinking: ThinkingLevel,
                #[serde(default)]
                service_tier: ServiceTier,
            },
            Str(String),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Obj {
                provider,
                id,
                thinking,
                service_tier,
            } => Self {
                provider,
                id,
                thinking,
                service_tier,
            },
            Repr::Str(s) => Self::parse(&s),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub api_type: Option<String>,
    /// Whether the model exposes a reasoning trace.
    #[serde(default)]
    pub reasoning: Option<bool>,
    #[serde(default)]
    pub supports_image: Option<bool>,
    #[serde(default)]
    pub context_window: Option<u64>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
    #[serde(default)]
    pub service_tier: Option<ServiceTier>,
    #[serde(default)]
    pub auto_continue: AutoContinueConfig,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub input_price: Option<f64>,
    #[serde(default)]
    pub output_price: Option<f64>,
    /// Cache-read price per 1M tokens (USD). Billed against
    /// `Usage.cache_read_tokens` when set; otherwise cache reads fall through
    /// to the input rate.
    #[serde(default)]
    pub cache_read_price: Option<f64>,
    /// Cache-write price per 1M tokens (USD). Billed against
    /// `Usage.cache_write_tokens` when set; otherwise cache writes fall
    /// through to the input rate.
    #[serde(default)]
    pub cache_write_price: Option<f64>,
    #[serde(default)]
    pub per_request_price: Option<f64>,
}

fn default_auto_models_path() -> String {
    "data".to_string()
}

fn default_true() -> bool {
    true
}

/// Per-api-type endpoint configuration: the endpoint `path` joined onto the
/// provider `base_url`, and optional pricing-field overrides. Keyed by the
/// internal [`Api`] identifier (e.g. `openai-completions`), so the `api`
/// itself is derived from the key — the mapping only carries the bits that
/// vary per endpoint within one provider. Lives on the provider and applies
/// to **both** static and auto-discovered models.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiTypeMapping {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub pricing_field_mappings: Option<PricingFieldMappings>,
}

/// Maps a remote api-type vocabulary (the strings a `/v1/models` endpoint
/// reports, e.g. `chat_completions`, `messages`) to lofi's internal
/// [`Api`] identifiers. Lives on [`AutoModelsConfig`] because only
/// auto-discovery needs to translate the endpoint's own strings; static
/// models use the internal id directly.
pub type ApiTypeMappings = HashMap<String, Api>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldMappings {
    #[serde(default = "default_field_name")]
    pub name: String,
    #[serde(default = "default_field_context_window")]
    pub context_window: String,
    #[serde(default = "default_field_max_tokens")]
    pub max_tokens: String,
}

fn default_field_name() -> String {
    "name".to_string()
}

fn default_field_context_window() -> String {
    "context_length".to_string()
}

fn default_field_max_tokens() -> String {
    "top_provider.max_completion_tokens".to_string()
}

impl Default for FieldMappings {
    fn default() -> Self {
        Self {
            name: default_field_name(),
            context_window: default_field_context_window(),
            max_tokens: default_field_max_tokens(),
        }
    }
}

/// When `enabled`, the provider's model list is fetched at startup from
/// `models_url` (default `{base_url}/v1/models`), mapped into [`ModelConfig`]
/// entries, and merged with the provider's static `models` (static wins on
/// `id` collision). lofi has no built-in model catalog, so discovered models
/// inherit thinking levels from this config rather than from a base model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoModelsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub auth: bool,
    #[serde(default)]
    pub models_url: Option<String>,
    #[serde(default = "default_auto_models_path")]
    pub path: String,
    #[serde(default)]
    pub api_type_field: Option<String>,
    #[serde(default)]
    pub api_type_mappings: ApiTypeMappings,
    #[serde(default)]
    pub field_mappings: FieldMappings,
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
    #[serde(default)]
    pub service_tier: Option<ServiceTier>,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingConvention {
    #[default]
    PerToken,
    PerMillion,
}

/// Configurable so a proxy whose pricing lives under non-standard keys can
/// be mapped without code changes. An empty path means no remote source for
/// that cost dimension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingFieldMappings {
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub cache_read: Option<String>,
    #[serde(default)]
    pub cache_write: Option<String>,
    #[serde(default)]
    pub per_request: Option<String>,
}

impl Default for PricingFieldMappings {
    fn default() -> Self {
        Self {
            input: Some("pricing.prompt".to_string()),
            output: Some("pricing.completion".to_string()),
            cache_read: Some("pricing.input_cache_read".to_string()),
            cache_write: Some("pricing.input_cache_write".to_string()),
            per_request: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub api_type: Option<Api>,
    #[serde(default)]
    pub api_types: IndexMap<String, ApiTypeMapping>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub pricing_convention: PricingConvention,
    #[serde(default)]
    pub pricing_field_mappings: PricingFieldMappings,
    #[serde(default)]
    pub env_name: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    #[serde(default)]
    pub models: IndexMap<String, ModelConfig>,
    #[serde(default)]
    pub auto_models: Option<AutoModelsConfig>,
    #[serde(default)]
    pub no_auth: bool,
    #[serde(default = "default_response_start_timeout_ms")]
    pub response_start_timeout_ms: u64,
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    #[serde(default)]
    pub service_tier: Option<ServiceTier>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
    #[serde(default)]
    pub auto_continue: AutoContinueConfig,
}

fn default_response_start_timeout_ms() -> u64 {
    90_000
}

impl ProviderConfig {
    #[must_use]
    pub fn default_api(&self) -> Api {
        self.api_type.unwrap_or(Api::OpenAiCompletions)
    }

    #[must_use]
    pub fn default_api_type_key(&self) -> String {
        self.default_api().id().to_string()
    }

    #[must_use]
    pub fn resolve_api(&self, api_type: Option<&str>) -> Api {
        api_type
            .and_then(Api::parse)
            .unwrap_or_else(|| self.default_api())
    }

    #[must_use]
    pub fn resolve_path(&self, api_type: Option<&str>) -> String {
        let default_key = self.default_api_type_key();
        let key = api_type.unwrap_or(&default_key);
        self.api_types
            .get(key)
            .and_then(|m| m.path.clone())
            .unwrap_or_else(|| self.resolve_api(api_type).default_path().to_string())
    }

    #[must_use]
    pub fn resolve_pricing_fields(&self, api_type: Option<&str>) -> &PricingFieldMappings {
        let default_key = self.default_api_type_key();
        let key = api_type.unwrap_or(&default_key);
        self.api_types
            .get(key)
            .and_then(|m| m.pricing_field_mappings.as_ref())
            .unwrap_or(&self.pricing_field_mappings)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    #[serde(default)]
    pub service_tier: Option<ServiceTier>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
    #[serde(default)]
    pub auto_continue: AutoContinueConfig,
}

/// Partial auto-continuation policy. Each configured level overlays the
/// preceding global or provider level field by field. Static models can add
/// one final override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AutoContinueConfig {
    /// Recover when the provider reports a tool stop but emits no tool call.
    #[serde(default)]
    pub lost_tool_call: Option<bool>,
    /// Recover a clean stop whose final sentence announces an immediate tool action.
    #[serde(default)]
    pub intent: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoContinuePolicy {
    pub lost_tool_call: bool,
    pub intent: bool,
}

impl Default for AutoContinuePolicy {
    fn default() -> Self {
        Self {
            lost_tool_call: true,
            intent: false,
        }
    }
}

impl AutoContinuePolicy {
    #[must_use]
    pub fn resolve(configs: &[AutoContinueConfig]) -> Self {
        let mut policy = Self::default();
        for config in configs {
            if let Some(enabled) = config.lost_tool_call {
                policy.lost_tool_call = enabled;
            }
            if let Some(enabled) = config.intent {
                policy.intent = enabled;
            }
        }
        policy
    }
}

/// The optional `[compaction.auto]` **soft caps** are speculative: a run may
/// cross them with no interruption, and when it reaches `agent_settled` with
/// context above the soft threshold, it compacts. Soft compaction only runs
/// when at least one soft cap is set; with defaults (reserved only) compaction
/// is hard-cap-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_reserved_context_tokens")]
    pub reserved_context_tokens: u64,
    /// Minimum agent messages that must elapse between two force-compacts.
    /// If a continued run crosses the hard cap again within this many
    /// messages of the last force-compact, it errors out instead of
    /// compacting again. Defaults to `6`.
    #[serde(default = "default_min_messages_between_hard_compacts")]
    pub min_messages_between_hard_compacts: usize,
    #[serde(default)]
    pub auto: AutoCompactConfig,
    #[serde(default)]
    pub edit: EditConfig,
}

fn default_reserved_context_tokens() -> u64 {
    20_000
}

fn default_min_messages_between_hard_compacts() -> usize {
    6
}

/// By default the child env is *stripped* to a minimal baseline (`PATH`,
/// `HOME`, locale, …) so inherited credentials never reach a model-run
/// shell. `pass_env` and `env_file` opt specific variables back in; their
/// values are redacted (`[redacted]`) from captured stdout/stderr before the
/// model sees them, so a command may *use* a secret without it leaking into
/// the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashConfig {
    #[serde(default = "default_true")]
    pub strip_env: bool,
    #[serde(default)]
    pub pass_env: Vec<String>,
    /// Path to a `KEY=VALUE` file whose entries are loaded into the child
    /// environment, overriding `pass_env`. `~` is expanded. The file should
    /// live outside the workspace so `lofi.read`/`edit`/`write` cannot reach
    /// it. Values are redacted from output. `None` by default.
    #[serde(default)]
    pub env_file: Option<PathBuf>,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            strip_env: default_true(),
            pass_env: Vec::new(),
            env_file: None,
        }
    }
}

/// Visible-output caps for tool results. Both file reads and bash output
/// apply this cap head- or tail-first: content within the limits is returned
/// verbatim, and overflow is replaced by a pointer to the full output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncateConfig {
    /// Maximum number of lines returned before truncation. Defaults to
    /// `2000`.
    #[serde(default = "default_truncate_max_lines")]
    pub max_lines: usize,
    /// Maximum number of bytes returned before truncation. Defaults to
    /// `51200` (50 KiB).
    #[serde(default = "default_truncate_max_bytes")]
    pub max_bytes: usize,
}

impl Default for TruncateConfig {
    fn default() -> Self {
        Self {
            max_lines: default_truncate_max_lines(),
            max_bytes: default_truncate_max_bytes(),
        }
    }
}

fn default_truncate_max_lines() -> usize {
    2000
}

fn default_truncate_max_bytes() -> usize {
    50 * 1024
}

/// Limits applied when an image enters model context. An image is downscaled
/// to fit `max_width`×`max_height` and re-encoded as JPEG, sweeping quality
/// down until the payload fits `max_bytes`. Bounds the base64 payload sent to
/// the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    /// Maximum pixel width after downscaling. Defaults to `2000`.
    #[serde(default = "default_image_max_width")]
    pub max_width: u32,
    /// Maximum pixel height after downscaling. Defaults to `2000`.
    #[serde(default = "default_image_max_height")]
    pub max_height: u32,
    /// Maximum byte size of the re-encoded JPEG payload. Defaults to `1048576`
    /// (1 MiB).
    #[serde(default = "default_image_max_bytes")]
    pub max_bytes: usize,
}

/// Image file extensions recognized by the sandbox `read` tool.
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp"];

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            max_width: default_image_max_width(),
            max_height: default_image_max_height(),
            max_bytes: default_image_max_bytes(),
        }
    }
}

fn default_image_max_width() -> u32 {
    2000
}

fn default_image_max_height() -> u32 {
    2000
}

fn default_image_max_bytes() -> usize {
    1024 * 1024
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellPolicyMode {
    #[default]
    Confirm,
    ReadOnly,
    WorkspaceWrite,
    Unrestricted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    Exact,
    Prefix,
    Substring,
    Args,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEntry {
    #[serde(rename = "match")]
    pub match_str: String,
    pub mode: MatchMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapperKind {
    ShellC,
    UtilityOperand,
    Env,
    Xargs,
    DockerRun,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrapperRuleConfig {
    pub name: String,
    pub kind: WrapperKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectPolicy {
    #[serde(default = "default_redirect_action")]
    pub action: PolicyAction,
    #[serde(default)]
    pub safe_targets: Vec<String>,
    #[serde(default)]
    pub allow_fd_dup: bool,
}

fn default_redirect_action() -> PolicyAction {
    PolicyAction::Ask
}

impl Default for RedirectPolicy {
    fn default() -> Self {
        Self {
            action: default_redirect_action(),
            safe_targets: Vec::new(),
            allow_fd_dup: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeredocPolicy {
    #[serde(default = "default_heredoc_action")]
    pub action: PolicyAction,
}

fn default_heredoc_action() -> PolicyAction {
    PolicyAction::Ask
}

impl Default for HeredocPolicy {
    fn default() -> Self {
        Self {
            action: default_heredoc_action(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Allow,
    Ask,
    Deny,
}

/// Session-scoped bash approval mode, chosen in the `/policy` dialog. It
/// never persists: every session starts in `AskManual`, or `AskAuto` when
/// auto mode is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashApprovalMode {
    /// Auto-approve `allow` and `ask` decisions without prompting;
    /// explicit `deny` rules still block.
    AllowAll,
    /// Honor the policy; an `ask` decision prompts the user.
    AskManual,
    /// Honor the policy; an `ask` decision goes to auto-mode evaluation.
    AskAuto,
    /// Block every command, including ones the policy allows.
    DenyAll,
}

/// This is distinct from YOLO mode: YOLO blindly allows anything not
/// denied, while auto-mode makes a per-command safety judgment via an
/// LLM call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoModeConfig {
    #[serde(default)]
    pub enable: bool,
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShellPolicyConfig {
    #[serde(default)]
    pub mode: ShellPolicyMode,
    #[serde(default)]
    pub yolo: bool,
    #[serde(default)]
    pub allow: Vec<CommandEntry>,
    #[serde(default)]
    pub ask: Vec<CommandEntry>,
    #[serde(default)]
    pub deny: Vec<CommandEntry>,
    #[serde(default)]
    pub wrappers: Vec<WrapperRuleConfig>,
    #[serde(default)]
    pub redirects: RedirectPolicy,
    #[serde(default)]
    pub heredocs: HeredocPolicy,
    #[serde(default)]
    pub auto_mode: Option<AutoModeConfig>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryConfig {
    #[serde(default = "default_retry_max_retries")]
    pub max_retries: u32,
    #[serde(default = "default_retry_base_delay_ms")]
    pub base_delay_ms: u64,
    /// Per-retry delay ceiling (ms). Defaults to `60_000` (60s) so a long
    /// retry tail under persistent transient errors waits in bounded steps
    /// rather than doubling without limit.
    #[serde(default = "default_retry_max_delay_ms")]
    pub max_delay_ms: u64,
}

fn default_retry_max_retries() -> u32 {
    10
}

fn default_retry_base_delay_ms() -> u64 {
    2000
}

fn default_retry_max_delay_ms() -> u64 {
    60_000
}

/// Credential helper (`!cmd` value resolution) limits. The timeout default
/// is generous for interactive password managers; override it via
/// `[credential] timeout_ms` or `LOFI__CREDENTIAL__TIMEOUT_MS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialConfig {
    #[serde(default = "default_credential_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_credential_timeout_ms() -> u64 {
    30_000
}

impl Default for CredentialConfig {
    fn default() -> Self {
        Self {
            timeout_ms: default_credential_timeout_ms(),
        }
    }
}

/// Color scheme selection. `Auto` queries the terminal via OSC 11;
/// `Light` / `Dark` force the corresponding palette without probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

impl ThemeMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default)]
    pub theme: ThemeMode,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: default_retry_max_retries(),
            base_delay_ms: default_retry_base_delay_ms(),
            max_delay_ms: default_retry_max_delay_ms(),
        }
    }
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            reserved_context_tokens: default_reserved_context_tokens(),
            min_messages_between_hard_compacts: default_min_messages_between_hard_compacts(),
            auto: AutoCompactConfig::default(),
            edit: EditConfig::default(),
        }
    }
}

impl CompactionConfig {
    #[must_use]
    pub fn hard_threshold(&self, context_window: u64) -> Option<u64> {
        if context_window == 0 {
            return None;
        }
        let threshold = context_window.saturating_sub(self.reserved_context_tokens);
        (threshold > 0).then_some(threshold)
    }

    #[must_use]
    pub fn soft_threshold(&self, context_window: u64) -> Option<u64> {
        if !self.auto.enable {
            return None;
        }
        let mut threshold = None::<u64>;
        if let Some(cap) = self.auto.max_context_tokens {
            threshold = Some(threshold.map_or(cap, |t| t.min(cap)));
        }
        if let Some(ratio) = self.auto.context_ratio {
            if ratio > 0.0 && ratio <= 1.0 && context_window > 0 {
                let r = (ratio * context_window as f64) as u64;
                threshold = Some(threshold.map_or(r, |t| t.min(r)));
            }
        }
        threshold.filter(|&t| t > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoCompactConfig {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    #[serde(default)]
    pub context_ratio: Option<f64>,
}

impl Default for AutoCompactConfig {
    fn default() -> Self {
        Self {
            enable: true,
            max_context_tokens: None,
            context_ratio: None,
        }
    }
}

/// Tiered-retention context editing (`[compaction.edit]`). Applied to the
/// kept tail at each compaction so the new prefix is much lighter and the
/// next compaction fires far later. Runs at compaction boundaries only —
/// never mid-run — so prefix caching is preserved between compactions (the
/// tail is append-only there; the edit rides the cache break compaction
/// already pays).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_edit_keep_results")]
    pub keep_results: usize,
    #[serde(default = "default_edit_keep_thinking")]
    pub keep_thinking: usize,
    #[serde(default = "default_edit_keep_calls")]
    pub keep_calls: usize,
}

fn default_edit_keep_results() -> usize {
    6
}
fn default_edit_keep_thinking() -> usize {
    2
}
fn default_edit_keep_calls() -> usize {
    6
}

impl Default for EditConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_results: default_edit_keep_results(),
            keep_thinking: default_edit_keep_thinking(),
            keep_calls: default_edit_keep_calls(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub agent: AgentConfig,
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub bash: BashConfig,
    #[serde(default)]
    pub truncate: TruncateConfig,
    #[serde(default)]
    pub image: ImageConfig,
    #[serde(skip)]
    pub shell_policy: ShellPolicyConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub credential: CredentialConfig,
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
    pub providers: IndexMap<String, ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            agent: AgentConfig::default(),
            ui: UiConfig::default(),
            compaction: CompactionConfig::default(),
            bash: BashConfig::default(),
            truncate: TruncateConfig::default(),
            image: ImageConfig::default(),
            shell_policy: ShellPolicyConfig::default(),
            retry: RetryConfig::default(),
            credential: CredentialConfig::default(),
            default_provider: None,
            default_model: None,
            providers: IndexMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_block_native_records_only_from_tool_call() {
        let rec = NativeToolRecord {
            parent: "t1".into(),
            call_id: 1,
            name: "read".into(),
            args: "a.rs".into(),
            result: String::new(),
            is_error: false,
        };
        let call = CompactBlock::ToolCall {
            id: "t1".into(),
            code: String::new(),
            label: None,
            native: vec![rec.clone()],
        };
        assert_eq!(call.native_records().len(), 1);
        assert_eq!(call.native_records()[0].name, "read");
        assert!(CompactBlock::User { text: "hi".into() }
            .native_records()
            .is_empty());
        assert!(CompactBlock::Assistant { text: "ok".into() }
            .native_records()
            .is_empty());
        assert!(CompactBlock::ToolResult {
            id: "t1".into(),
            text: String::new(),
            is_error: false,
        }
        .native_records()
        .is_empty());
    }

    fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        value: &T,
    ) {
        let json = serde_json::to_string(value).unwrap();
        let back: T = serde_json::from_str(&json).unwrap();
        assert_eq!(value, &back);
    }

    #[test]
    fn api_as_str_parse_roundtrip() {
        for api in [
            Api::OpenAiCompletions,
            Api::OpenAiResponses,
            Api::AnthropicMessages,
        ] {
            assert_eq!(Api::parse(api.as_str()), Some(api));
        }
        assert_eq!(Api::parse("bogus"), None);
        assert_eq!(Api::OpenAiCompletions.label(), "OpenAI Chat Completions");
    }

    #[test]
    fn run_model_label_includes_service_tier() {
        let m = RunModel {
            provider: "openai".into(),
            id: "gpt-5.6-sol".into(),
            thinking: ThinkingLevel::High,
            service_tier: ServiceTier::Flex,
        };
        assert_eq!(m.label(), "openai/gpt-5.6-sol:high@flex");

        let auto = RunModel {
            service_tier: ServiceTier::Auto,
            ..m.clone()
        };
        assert_eq!(auto.label(), "openai/gpt-5.6-sol:high");

        let off = RunModel {
            thinking: ThinkingLevel::Off,
            service_tier: ServiceTier::Priority,
            ..m.clone()
        };
        assert_eq!(off.label(), "openai/gpt-5.6-sol@priority");
    }

    #[test]
    fn run_model_parse_round_trips_service_tier() {
        for label in [
            "openai/gpt-5.6-sol",
            "openai/gpt-5.6-sol:high",
            "openai/gpt-5.6-sol:high@flex",
            "openai/gpt-5.6-sol@priority",
        ] {
            let parsed: RunModel = label.into();
            assert_eq!(parsed.label(), label, "round trip failed for {label}");
        }
    }

    #[test]
    fn run_model_custom_tier_round_trips() {
        let parsed: RunModel = "openai/gpt-5.6-sol:high@vip".into();
        assert_eq!(parsed.id, "gpt-5.6-sol");
        assert_eq!(parsed.thinking, ThinkingLevel::High);
        assert_eq!(parsed.service_tier, ServiceTier::Custom("vip".into()));
        assert_eq!(parsed.label(), "openai/gpt-5.6-sol:high@vip");
    }

    #[test]
    fn service_tier_parse_and_round_trip() {
        for (s, tier) in [
            ("auto", ServiceTier::Auto),
            ("flex", ServiceTier::Flex),
            ("priority", ServiceTier::Priority),
        ] {
            assert_eq!(ServiceTier::parse(s), Some(tier.clone()));
            assert_eq!(tier.as_str(), s);
        }
        assert_eq!(ServiceTier::parse("bogus"), None);
        let custom = ServiceTier::Custom("vip".into());
        assert_eq!(custom.as_str(), "vip");
        round_trip(&custom);
        let json = serde_json::to_string(&custom).unwrap();
        assert_eq!(json, "\"vip\"");
    }

    #[test]
    fn api_serde_snake_case() {
        let json = serde_json::to_string(&Api::AnthropicMessages).unwrap();
        assert_eq!(json, "\"anthropic_messages\"");
        let back: Api = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Api::AnthropicMessages);
    }

    #[test]
    fn content_block_round_trips() {
        round_trip(&ContentBlock::Text {
            text: "hi".to_string(),
        });
        round_trip(&ContentBlock::ToolUse {
            id: "t1".to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": "1+1"}),
        });
        round_trip(&ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "2".to_string(),
            is_error: false,
            images: Vec::new(),
        });
        round_trip(&ContentBlock::ToolResult {
            tool_use_id: "t2".to_string(),
            content: "img".to_string(),
            is_error: false,
            images: vec![ToolResultImage {
                bytes: vec![0x89, 0x50],
                media_type: "image/png".to_string(),
            }],
        });
        round_trip(&ContentBlock::Thinking {
            text: "hmm".to_string(),
            signature: None,
            redacted: false,
        });
        round_trip(&ContentBlock::Image {
            bytes: vec![0xFF, 0xD8, 0xFF, 0xD9],
            media_type: "image/jpeg".to_string(),
        });
    }

    #[test]
    fn image_block_serializes_bytes_as_base64() {
        let json = serde_json::to_value(&ContentBlock::Image {
            bytes: vec![1, 2, 3],
            media_type: "image/jpeg".to_string(),
        })
        .unwrap();
        assert_eq!(json["type"], "image");
        assert_eq!(json["bytes"], "AQID");
        assert_eq!(json["media_type"], "image/jpeg");
        let back: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(
            back,
            ContentBlock::Image {
                bytes: vec![1, 2, 3],
                media_type: "image/jpeg".to_string(),
            }
        );
    }

    #[test]
    fn retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 10);
        assert_eq!(cfg.base_delay_ms, 2000);
        assert_eq!(cfg.max_delay_ms, 60_000);
    }

    #[test]
    fn retry_config_serde_omitted_uses_defaults() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            #[serde(default)]
            retry: RetryConfig,
        }
        let w: Wrap = serde_json::from_str("{}").unwrap();
        assert_eq!(w.retry, RetryConfig::default());
    }

    #[test]
    fn retry_config_serde_partial_override() {
        #[derive(serde::Deserialize)]
        struct Wrap {
            #[serde(default)]
            retry: RetryConfig,
        }
        let w: Wrap = serde_json::from_str("{\"retry\":{\"max_retries\":0}}").unwrap();
        assert_eq!(w.retry.max_retries, 0);
        assert_eq!(w.retry.base_delay_ms, 2000);
        assert_eq!(w.retry.max_delay_ms, 60_000);
    }

    #[test]
    fn content_block_text_tagged_shape() {
        let block = ContentBlock::Text {
            text: "hi".to_string(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hi");
    }

    #[test]
    fn message_round_trips() {
        round_trip(&Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text {
                text: "hello".to_string(),
            }],
            kind: PromptKind::User,
        });
        // Default-User kind must skip in the wire form; explicit-Notice must round-trip.
        let user_json = serde_json::to_value(&Message {
            role: Role::User,
            blocks: vec![],
            kind: PromptKind::User,
        })
        .unwrap();
        assert!(user_json.get("kind").is_none());
        let notice_json = serde_json::to_value(&Message {
            role: Role::User,
            blocks: vec![],
            kind: PromptKind::Notice,
        })
        .unwrap();
        assert_eq!(notice_json["kind"], "notice");
        let parsed: Message = serde_json::from_str(r#"{"role":"user","blocks":[]}"#).unwrap();
        assert_eq!(
            parsed.kind,
            PromptKind::User,
            "absent kind defaults to User"
        );
    }

    #[test]
    fn turn_end_without_stop_reason_still_loads() {
        // Transcripts written before stop reasons were recorded carry no
        // such field; keep them loading with None.
        let parsed: SessionEvent =
            serde_json::from_str(r#"{"type":"turn_end","elapsed_ms":1,"cost":0.0,"usage":{}}"#)
                .unwrap();
        match parsed.kind {
            SessionEventKind::TurnEnd { stop_reason, .. } => assert_eq!(stop_reason, None),
            other => panic!("expected turn_end, got {other:?}"),
        }
    }

    #[test]
    fn streaming_event_round_trips() {
        round_trip(&StreamingEvent::TextDelta("x".to_string()));
        round_trip(&StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        round_trip(&StreamingEvent::Done {
            usage: Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Usage::default()
            },
            stop_reason: Some(StopReason::EndTurn),
        });
        round_trip(&StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        });
        round_trip(&StreamingEvent::Error("boom".to_string()));
    }

    #[test]
    fn usage_defaults_to_zero() {
        let json = "{}";
        let usage: Usage = serde_json::from_str(json).unwrap();
        assert_eq!(usage, Usage::default());
    }

    #[test]
    fn config_round_trips() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_type: Some(Api::OpenAiCompletions),
                api_types: IndexMap::new(),
                base_url: Some("https://api.openai.com".to_string()),
                pricing_convention: PricingConvention::PerToken,
                pricing_field_mappings: PricingFieldMappings::default(),
                env_name: Some("OPENAI_API_KEY".to_string()),
                api_key: None,
                headers: None,
                models: {
                    let mut m = IndexMap::new();
                    m.insert(
                        "gpt-4o".to_string(),
                        ModelConfig {
                            name: None,
                            api_type: None,
                            reasoning: None,
                            supports_image: Some(true),
                            context_window: Some(128_000),
                            max_tokens: None,
                            thinking_levels: Vec::new(),
                            thinking_level: None,
                            service_tiers: Vec::new(),
                            service_tier: None,
                            auto_continue: AutoContinueConfig::default(),
                            base_url: None,
                            input_price: None,
                            output_price: None,
                            cache_read_price: None,
                            cache_write_price: None,
                            per_request_price: None,
                        },
                    );
                    m
                },
                auto_models: None,
                no_auth: false,
                response_start_timeout_ms: 90_000,
                thinking_level: None,
                thinking_levels: Vec::new(),
                service_tier: None,
                service_tiers: Vec::new(),
                auto_continue: AutoContinueConfig::default(),
            },
        );
        let cfg = Config {
            providers,
            ..Config::default()
        };
        round_trip(&cfg);
    }

    #[test]
    fn thinking_level_serde_lowercase() {
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::XHigh).unwrap(),
            concat!('"', "xhigh", '"')
        );
        assert_eq!(
            serde_json::to_string(&ThinkingLevel::Off).unwrap(),
            concat!('"', "off", '"')
        );
        let l: ThinkingLevel = serde_json::from_str(concat!('"', "medium", '"')).unwrap();
        assert_eq!(l, ThinkingLevel::Medium);

        let future: ThinkingLevel = serde_json::from_str(r#""minimal""#).unwrap();
        assert_eq!(future.as_str(), "minimal");
        assert_eq!(serde_json::to_string(&future).unwrap(), r#""minimal""#);
    }

    #[test]
    fn api_default_base_url_and_path() {
        assert_eq!(
            Api::OpenAiCompletions.default_base_url(),
            "https://api.openai.com"
        );
        assert_eq!(
            Api::OpenAiResponses.default_base_url(),
            "https://api.openai.com"
        );
        assert_eq!(
            Api::AnthropicMessages.default_base_url(),
            "https://api.anthropic.com"
        );
        assert_eq!(
            Api::OpenAiCompletions.default_path(),
            "/v1/chat/completions"
        );
        assert_eq!(Api::OpenAiResponses.default_path(), "/v1/responses");
        assert_eq!(Api::AnthropicMessages.default_path(), "/v1/messages");
    }

    #[test]
    fn api_type_accepts_kebab_case() {
        assert_eq!(Api::parse("openai-responses"), Some(Api::OpenAiResponses));
        assert_eq!(
            Api::parse("anthropic-messages"),
            Some(Api::AnthropicMessages)
        );
        assert_eq!(Api::parse("openai_responses"), Some(Api::OpenAiResponses));
    }

    #[test]
    fn compaction_hard_threshold_reserved() {
        let cfg = CompactionConfig::default();
        assert_eq!(cfg.hard_threshold(200_000), Some(180_000));
        let with_caps = CompactionConfig {
            auto: AutoCompactConfig {
                max_context_tokens: Some(100_000),
                context_ratio: Some(0.5),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(with_caps.hard_threshold(200_000), Some(180_000));
        assert_eq!(cfg.hard_threshold(10_000), None);
        assert_eq!(cfg.hard_threshold(0), None);
    }

    #[test]
    fn disabled_auto_compaction_has_no_soft_threshold() {
        let cfg = CompactionConfig {
            auto: AutoCompactConfig {
                enable: false,
                max_context_tokens: Some(10_000),
                context_ratio: Some(0.5),
            },
            ..CompactionConfig::default()
        };
        assert_eq!(cfg.soft_threshold(100_000), None);
    }

    #[test]
    fn compaction_soft_threshold_caps() {
        assert_eq!(CompactionConfig::default().soft_threshold(200_000), None);
        let with_cap = CompactionConfig {
            auto: AutoCompactConfig {
                max_context_tokens: Some(150_000),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(with_cap.soft_threshold(1_000_000), Some(150_000));
        assert_eq!(with_cap.soft_threshold(200_000), Some(150_000));
        let with_ratio = CompactionConfig {
            auto: AutoCompactConfig {
                context_ratio: Some(0.5),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(with_ratio.soft_threshold(200_000), Some(100_000));
        let bad_ratio = CompactionConfig {
            auto: AutoCompactConfig {
                context_ratio: Some(1.5),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(bad_ratio.soft_threshold(200_000), None);
        let both = CompactionConfig {
            auto: AutoCompactConfig {
                max_context_tokens: Some(150_000),
                context_ratio: Some(0.5),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(both.soft_threshold(400_000), Some(150_000));
    }

    #[test]
    fn compaction_config_serde_defaults() {
        let cfg: CompactionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.reserved_context_tokens, 20_000);
        assert_eq!(cfg.min_messages_between_hard_compacts, 6);
        assert!(cfg.auto.enable);
        assert!(cfg.auto.max_context_tokens.is_none());
        assert!(cfg.auto.context_ratio.is_none());
    }

    #[test]
    fn bash_config_serde_defaults() {
        let cfg: BashConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.strip_env);
        assert!(cfg.pass_env.is_empty());
        assert!(cfg.env_file.is_none());
    }

    #[test]
    fn shell_policy_defaults_require_confirmation() {
        let cfg: ShellPolicyConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.mode, ShellPolicyMode::Confirm);
        assert_eq!(cfg.redirects.action, PolicyAction::Ask);
    }

    #[test]
    fn shell_policy_presets_remain_deserializable() {
        for (raw, expected) in [
            ("read_only", ShellPolicyMode::ReadOnly),
            ("workspace_write", ShellPolicyMode::WorkspaceWrite),
            ("unrestricted", ShellPolicyMode::Unrestricted),
        ] {
            let json = format!(r#"{{"mode":"{raw}"}}"#);
            let cfg: ShellPolicyConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(cfg.mode, expected);
        }
    }

    #[test]
    fn bash_config_serde_round_trip() {
        let json = r#"{"strip_env":false,"pass_env":["GITHUB_TOKEN","NPM_TOKEN"],"env_file":"~/.config/lofi/secrets.env"}"#;
        let cfg: BashConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.strip_env);
        assert_eq!(cfg.pass_env, ["GITHUB_TOKEN", "NPM_TOKEN"]);
        assert_eq!(
            cfg.env_file.as_deref(),
            Some(std::path::Path::new("~/.config/lofi/secrets.env"))
        );
    }

    #[test]
    fn auto_compact_config_serde_defaults() {
        let cfg: AutoCompactConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enable);
        assert!(cfg.max_context_tokens.is_none());
        assert!(cfg.context_ratio.is_none());
    }

    #[test]
    fn auto_models_defaults() {
        let json = "{}";
        let am: AutoModelsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(am.path, "data");
        assert!(am.auth);
        assert!(!am.enabled);
        assert!(am.models_url.is_none());
    }
}
