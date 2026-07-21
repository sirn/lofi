//! Pure data shapes shared across `lofi`.
//!
//! This crate holds only serde-friendly data types plus small helper methods,
//! with no behavior and no I/O. All enums use
//! `#[serde(rename_all = "snake_case")]`; `ContentBlock` is internally tagged so
//! it round-trips cleanly through JSON.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// Provider wire protocol used to talk to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Api {
    /// `OpenAI` Chat Completions (`/chat/completions`).
    #[serde(rename = "openai_completions", alias = "openai-completions")]
    OpenAiCompletions,
    /// `OpenAI` Responses API (`/responses`).
    #[serde(rename = "openai_responses", alias = "openai-responses")]
    OpenAiResponses,
    /// Anthropic Messages API (`/v1/messages`).
    #[serde(rename = "anthropic_messages", alias = "anthropic-messages")]
    AnthropicMessages,
}

impl Api {
    /// Return the `snake_case` identifier used on the wire.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    /// Return the `kebab-case` identifier used in config files and as the
    /// `api_types` table key. [`Self::parse`] accepts both this and
    /// [`Self::as_str`].
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    /// Return a short human-readable label for diagnostics and listings.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "OpenAI Chat Completions",
            Self::OpenAiResponses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
        }
    }

    /// Parse a config identifier back into an [`Api`]. Accepts both the
    /// `snake_case` form used on the wire and the `kebab-case` form used in
    /// config files (`openai-responses`).
    ///
    /// Returns `None` for unrecognized strings rather than erroring, so callers
    /// can attach their own diagnostic context.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai_completions" | "openai-completions" => Some(Self::OpenAiCompletions),
            "openai_responses" | "openai-responses" => Some(Self::OpenAiResponses),
            "anthropic_messages" | "anthropic-messages" => Some(Self::AnthropicMessages),
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
        }
    }

    /// Default full endpoint path for this protocol, used as the
    /// `api_type_mappings` entry's `path` when the user does not configure
    /// one. The path is joined onto the provider's `base_url` to form the
    /// model's request URL, and the provider POSTs to that URL verbatim
    /// (no further suffix is appended in code).
    #[must_use]
    pub fn default_path(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
            Self::AnthropicMessages => "/v1/messages",
        }
    }
}

/// A reasoning/"thinking" effort level.
///
/// Declared per-model as `thinking_levels` and selectable per-run via
/// `--model provider/model:level`. `Off` is a special level that disables
/// thinking entirely; the others scale the reasoning budget the provider
/// allocates. Serialization is lowercase (`off`, `low`, `medium`, `high`,
/// `xhigh`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    /// Thinking disabled.
    #[default]
    Off,
    /// Lowest reasoning effort.
    Low,
    /// Moderate reasoning effort (the agent default).
    Medium,
    /// High reasoning effort.
    High,
    /// Highest reasoning effort. Providers without a native `xhigh` step
    /// (`OpenAI`) clamp this down to their maximum.
    XHigh,
}

impl ThinkingLevel {
    /// Parse a level identifier; returns `None` for unrecognized strings.
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

    /// The lowercase identifier used in config files and `--model :level`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

/// Conversational role of a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    /// Return the `snake_case` identifier used on the wire.
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

/// A single block of content within a [`Message`].
///
/// Internally tagged by `type` so each variant round-trips through JSON without
/// ambiguity between text, tool calls, tool results, and reasoning traces.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// A plain text span.
    Text { text: String },
    /// A request from the assistant to invoke a tool.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The result of a tool invocation, fed back to the assistant.
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// Chain-of-thought / reasoning trace (where the API exposes it).
    ///
    /// `signature` carries Anthropic's thinking-block signature, which must be
    /// replayed verbatim on subsequent tool-use turns or the API rejects the
    /// request.
    Thinking {
        text: String,
        signature: Option<String>,
    },
}

/// A single message in the conversation log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who produced this message.
    pub role: Role,
    /// Ordered content blocks.
    pub blocks: Vec<ContentBlock>,
}

/// Incremental events emitted while streaming a model response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingEvent {
    /// A chunk of assistant text.
    TextDelta(String),
    /// A tool call has begun.
    ToolUseStart { id: String, name: String },
    /// Incremental JSON for a tool call's `input`.
    ToolUseInputDelta { id: String, delta: String },
    /// A tool call has finished.
    ToolUseEnd { id: String },
    /// A chunk of reasoning text.
    ThinkingDelta(String),
    /// A thinking-block signature (Anthropic). Must be replayed on tool-use
    /// turns, so it is captured onto the [`ContentBlock::Thinking`] block.
    ThinkingSignature(String),
    /// Stream complete with token usage.
    Done(Usage),
    /// The provider reported an error.
    Error(String),
}

/// Token accounting for a single model round-trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens consumed by the prompt.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens produced by the model.
    #[serde(default)]
    pub output_tokens: u64,
    /// Prompt tokens served from a cache.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Prompt tokens written to a cache.
    #[serde(default)]
    pub cache_write_tokens: u64,
}

/// A native tool call (`lofi.bash`/`lofi.read`/…) that ran inside an `exec`
/// block, captured so the nested call list survives resume. `parent` is the
/// enclosing exec tool-call id; `call_id` is a per-exec counter. Named
/// `call_id` (not `id`) so it does not collide with the tree-level `id` on
/// [`SessionEvent`] when flattened together.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NativeToolRecord {
    pub parent: String,
    pub call_id: u64,
    pub name: String,
    pub args: String,
    pub result: String,
    pub is_error: bool,
}

/// The payload of a [`SessionEvent`], exclusive of tree linkage. See
/// [`SessionEvent`] for the on-disk shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEventKind {
    /// A conversation message: a user prompt, an assistant turn, or a tool
    /// result. Serialized as `{"type":"message", <Message fields>}`.
    Message(Message),
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
    /// A completed turn: its run label, wall-clock duration, accumulated USD
    /// cost, and the final round's token usage. Rendered as the `◇ label done
    /// in Ns` block and folded into the status bar totals.
    TurnEnd {
        label: String,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
    },
    /// A turn that ended in failure (a non-retryable provider error or a
    /// user cancel): its run label, wall-clock duration, the error message,
    /// and the cost/usage accumulated by the rounds that did run. Rendered as
    /// a `◇ label failed in Ns · <error>` marker. Its `parent_id` points at
    /// the turn's checkpoint (the last event before the failed turn started),
    /// so the active-path walk excludes the failed turn's messages from the
    /// agent's history on resume while keeping them visible in the tree.
    TurnFailed {
        label: String,
        elapsed_ms: u64,
        error: String,
        cost: f64,
        usage: Usage,
    },
    /// A native tool call that ran inside an `exec` block, so the nested
    /// `lofi.<tool>` call list survives resume.
    NativeTool(NativeToolRecord),
    /// An offline compaction marker: `summary` replaces the summarized
    /// prefix (everything older than `first_kept_entry_id` on the active
    /// path) and is injected as a single user message at the head of the
    /// kept tail on resume. Appended to the active leaf by the `/compact`
    /// command (and the auto-trigger); a resumed session rebuilds the
    /// compacted history from it. Subsequent turns chain off this entry so
    /// the active path runs root -> kept tail -> Compaction -> new turns.
    Compaction {
        /// The full summary text (preamble + sections + brief transcript).
        summary: String,
        /// Event id of the first kept message on the active path. The
        /// agent-history walk on resume emits the summary, then the kept
        /// tail, and stops at this id — everything older is already folded
        /// into the summary. The empty string means compact-all (nothing
        /// kept).
        first_kept_entry_id: String,
        /// How many live messages were folded into the summary (for the
        /// visible marker on resume).
        summarized: usize,
        /// How many messages were kept in the tail.
        kept: usize,
    },
}

/// One append-only line in a session transcript log.
///
/// The first line of a session file is the session header (written by the
/// store); every subsequent line is a `SessionEvent`. Events form a tree via
/// `id`/`parent_id`: each entry points at its parent, the root entry's
/// `parent_id` is `None`, and the "active leaf" is the current position in
/// the tree. Branching appends a new child to an earlier entry instead of to
/// the previous line, so alternatives coexist in one file.
///
/// The engine appends events as a turn commits — the conversation messages
/// plus the run's own timing/cost metadata — and the UI replays them into its
/// view. Putting timings and cost in the same log as the messages (rather
/// than a sidecar) means a resumed session reconstructs identically to the
/// live one, through a single replayer.
///
/// `id`/`parent_id` are `#[serde(default)]` so legacy v1 files (which have
/// neither) still parse; the store migrates them by chaining each event to
/// the previous one on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// Short, low-entropy id unique within this file. Assigned by the store
    /// on append.
    #[serde(default)]
    pub id: String,
    /// Parent entry id, or `None` for the root entry (the first event after
    /// the header).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// The payload (message / timing / turn marker / native tool).
    #[serde(flatten)]
    pub kind: SessionEventKind,
}

/// A model entry as resolved by the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// Provider-local model identifier (e.g. `gpt-4o`).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Owning provider key in the config.
    pub provider: String,
    /// Wire protocol to use for this model.
    pub api: Api,
    /// Whether the model exposes a reasoning/Thinking trace.
    #[serde(default)]
    pub reasoning: bool,
    /// Effective reasoning/"thinking" level for this run. Resolved at agent
    /// construction from the CLI `:level`, the model/provider/agent defaults,
    /// and the model's declared `thinking_levels`. Encoded into the request
    /// by the per-API IR builders.
    #[serde(default)]
    pub thinking: ThinkingLevel,
    /// Whether the model accepts image inputs.
    #[serde(default)]
    pub supports_image: bool,
    /// Context window size in tokens, if known.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Max output tokens, if known.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Per-model endpoint base, set by auto-discovery when an api-type mapping
    /// carries a path (e.g. an OpenAI-compatible proxy routing one base URL to several
    /// upstream APIs). When `None`, the provider's `base_url` is used.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Input price per 1M tokens (USD), for the TUI cost estimate.
    #[serde(default)]
    pub input_price: Option<f64>,
    /// Output price per 1M tokens (USD).
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
    /// Flat per-request cost (USD). Billed once per turn regardless of token
    /// counts.
    #[serde(default)]
    pub per_request_price: Option<f64>,
}

/// A model entry declared statically in TOML.
///
/// The model id is the key of the `models` map in [`ProviderConfig`], so it
/// is not repeated here. Auto-discovery produces entries of the same shape,
/// filling every field it can read from the `/v1/models` endpoint and
/// leaving the rest `None`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Optional display name; defaults to the id when absent.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional per-model api-type key override (an internal [`Api`] id,
    /// e.g. `openai-responses`). Resolved against the provider's `api_types`
    /// table exactly like a discovered model's `preferred_api`. Defaults to
    /// the provider's `api_type`.
    #[serde(default)]
    pub api_type: Option<String>,
    /// Whether the model exposes a reasoning trace.
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// Whether the model accepts image inputs.
    #[serde(default)]
    pub supports_image: Option<bool>,
    /// Context window size in tokens.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Max output tokens.
    #[serde(default)]
    pub max_tokens: Option<u64>,
    /// Reasoning levels the model accepts, in priority order. The effective
    /// level (from CLI, model/provider/agent default) must be one of these or
    /// [`ThinkingLevel::Off`]; an empty list means the model does not support
    /// thinking and the effective level is forced to `Off`.
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    /// Optional per-model default thinking level.
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    /// Per-model endpoint URL override. When unset the URL is resolved by
    /// joining the provider `base_url` with the `api_types[key].path`.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Input price per 1M tokens (USD). When set, the TUI accumulates a cost
    /// estimate from each turn's `Usage`.
    #[serde(default)]
    pub input_price: Option<f64>,
    /// Output price per 1M tokens (USD).
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
    /// Flat per-request cost (USD). Billed once per turn regardless of token
    /// counts; most token-priced models leave this unset.
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
    /// Full endpoint path joined onto the provider `base_url` (e.g.
    /// `/v1/chat/completions`). Defaults to [`Api::default_path`] for the
    /// key's [`Api`] when unset. The provider POSTs to the joined URL
    /// verbatim; no further suffix is appended in code.
    #[serde(default)]
    pub path: Option<String>,
    /// Dot-notation paths to the per-model pricing fields in a discovered
    /// model entry for this endpoint. When unset, the provider-level
    /// `pricing_field_mappings` is used.
    #[serde(default)]
    pub pricing_field_mappings: Option<PricingFieldMappings>,
}

/// Maps a remote api-type vocabulary (the strings a `/v1/models` endpoint
/// reports, e.g. `chat_completions`, `messages`) to lofi's internal
/// [`Api`] identifiers. Lives on [`AutoModelsConfig`] because only
/// auto-discovery needs to translate the endpoint's own strings; static
/// models use the internal id directly.
pub type ApiTypeMappings = HashMap<String, Api>;

/// Where to read each non-pricing [`ModelConfig`] field from in a discovered
/// `/v1/models` entry. All paths are dot-notation JSON pointers into the
/// entry object. `reasoning` and `supports_image` are not mapped here — they
/// are inferred from the entry's `supported_parameters` array (a model
/// supports reasoning if the array contains `"reasoning"`, images if it
/// contains `"image"`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldMappings {
    /// Path to the display name. Defaults to `name`.
    #[serde(default = "default_field_name")]
    pub name: String,
    /// Path to the context window size in tokens (u64). Defaults to
    /// `context_length`.
    #[serde(default = "default_field_context_window")]
    pub context_window: String,
    /// Path to the max output tokens (u64). Defaults to
    /// `top_provider.max_completion_tokens`.
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

/// Auto-discovery of models from an OpenAI-style `/v1/models` endpoint.
///
/// When `enabled`, the provider's model list is fetched at startup from
/// `models_url` (default `{base_url}/v1/models`), mapped into [`ModelConfig`]
/// entries, and merged with the provider's static `models` (static wins on
/// `id` collision). lofi has no built-in model catalog, so discovered models
/// inherit thinking levels from this config rather than from a base model.
///
/// Endpoint paths and pricing-field mappings live on the provider
/// ([`ProviderConfig`]) and apply to both static and discovered models; this
/// block only governs the discovery fetch itself plus the translation from
/// the endpoint's own api-type vocabulary to lofi's internal ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoModelsConfig {
    /// Master switch; when `false` the block is ignored.
    #[serde(default)]
    pub enabled: bool,
    /// Send credentials on the models fetch. Defaults to `true`; set to
    /// `false` for public endpoints that reject auth headers.
    #[serde(default = "default_true")]
    pub auth: bool,
    /// Full models endpoint URL. When omitted, the fetch targets
    /// `{base_url}/v1/models`.
    #[serde(default)]
    pub models_url: Option<String>,
    /// JSON pointer-ish path (dot-separated) to the array of models in the
    /// response; defaults to `data`.
    #[serde(default = "default_auto_models_path")]
    pub path: String,
    /// Field name in each model entry naming the remote api-type (e.g.
    /// `preferred_api`). The value is translated through
    /// [`Self::api_type_mappings`] to an internal [`Api`] id and stored on
    /// the discovered [`ModelConfig`] as its `api_type` key. When unset,
    /// discovered models inherit the provider's default `api_type`.
    #[serde(default)]
    pub api_type_field: Option<String>,
    /// Remote api-type vocabulary → internal [`Api`] id. Only consulted when
    /// [`Self::api_type_field`] is set. A remote value with no mapping falls
    /// back to the provider's default `api_type`.
    #[serde(default)]
    pub api_type_mappings: ApiTypeMappings,
    /// Where to read each non-pricing [`ModelConfig`] field from in a
    /// discovered entry. Defaults to the OpenRouter-style layout.
    #[serde(default)]
    pub field_mappings: FieldMappings,
    /// Thinking levels exposed by discovered models.
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
    /// Optional per-model default thinking level for discovered models.
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    /// Cache freshness in seconds for the fetched model list. Defaults to
    /// 300 (5 minutes) when unset.
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
}

/// How a remote `/v1/models` endpoint reports per-token pricing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingConvention {
    /// Values are per-token; multiply by 1M for a per-1M-token cost.
    #[default]
    PerToken,
    /// Values are already per-1M-token; use as-is.
    PerMillion,
}

/// Dot-notation paths to the pricing fields in a remote model entry.
///
/// Configurable so a proxy whose pricing lives under non-standard keys can
/// be mapped without code changes. An empty path means no remote source for
/// that cost dimension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricingFieldMappings {
    /// Path to the input/prompt cost (e.g. `pricing.prompt`).
    #[serde(default)]
    pub input: Option<String>,
    /// Path to the output/completion cost (e.g. `pricing.completion`).
    #[serde(default)]
    pub output: Option<String>,
    /// Path to the cache-read cost (e.g. `pricing.input_cache_read`).
    #[serde(default)]
    pub cache_read: Option<String>,
    /// Path to the cache-write cost (e.g. `pricing.input_cache_write`).
    #[serde(default)]
    pub cache_write: Option<String>,
    /// Path to a flat per-request cost (e.g. `pricing.request`). Most
    /// token-priced models leave this unset.
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

/// A provider entry in the config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Default api-type key (an internal [`Api`] id, e.g.
    /// `openai-completions`) used by models that do not name their own.
    /// Defaults to [`Api::OpenAiCompletions`] (`openai-completions`) when
    /// unset. A per-model `api_type` override or an auto-discovered
    /// `preferred_api` resolves against [`Self::api_types`] using the same
    /// key.
    #[serde(default)]
    pub api_type: Option<Api>,
    /// Per-api-type endpoint routing table, keyed by internal [`Api`] id.
    /// Each entry carries the endpoint `path` (joined onto `base_url`) and
    /// optional pricing-field overrides. Applies to **both** static and
    /// auto-discovered models — resolution looks up `api_types[key]`, takes
    /// its `path` (defaulting to [`Api::default_path`] for the key's [`Api`]),
    /// and joins onto `base_url`. A minimal single-protocol provider can
    /// omit this and rely on the defaults.
    #[serde(default)]
    pub api_types: IndexMap<String, ApiTypeMapping>,
    /// Base URL (host root, e.g. `https://api.openai.com`) for the provider's
    /// API. When omitted, defaults to [`Api::default_base_url`] for the
    /// default api-type's [`Api`]. The full endpoint URL is built by joining
    /// the mapping's `path` onto this root.
    #[serde(default)]
    pub base_url: Option<String>,
    /// How the remote models endpoint reports pricing values. `per_token`
    /// (default) multiplies each value by 1M for a per-1M-token cost;
    /// `per_million` uses values as-is. Per-endpoint overrides live on each
    /// [`ApiTypeMapping`].
    #[serde(default)]
    pub pricing_convention: PricingConvention,
    /// Dot-notation paths to the per-model pricing fields in a discovered
    /// model entry, used when an [`ApiTypeMapping`] does not carry its own.
    /// Defaults to the OpenRouter-style `pricing.prompt` / `.completion` /
    /// `.input_cache_read` / `.input_cache_write` layout.
    #[serde(default)]
    pub pricing_field_mappings: PricingFieldMappings,
    /// Name of the environment variable holding the API key (e.g.
    /// `OPENAI_API_KEY`). Read lazily and *leniently*: if the variable is
    /// unset the provider is left keyless and simply not available, rather
    /// than aborting startup. This is the primary auth mechanism for the
    /// built-in defaults.
    #[serde(default)]
    pub env_name: Option<String>,
    /// Explicit API key or value-resolution expression (literal, `$ENV`,
    /// `!cmd`). When set it overrides `env_name` and is resolved *strictly*
    /// (a missing `$VAR` is an error, since the user authored it).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Extra HTTP headers to send (values subject to resolution).
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// Statically declared models, keyed by provider-local model id. The
    /// insertion order is preserved so "first available" selection is
    /// deterministic and follows the config file.
    #[serde(default)]
    pub models: IndexMap<String, ModelConfig>,
    /// Optional remote auto-models discovery (`/v1/models` style).
    #[serde(default)]
    pub auto_models: Option<AutoModelsConfig>,
    /// Explicitly mark this provider as unauthenticated (e.g. a local
    /// endpoint). When `true` the provider is selectable and requests omit
    /// auth headers regardless of `env_name`/`api_key`/`headers`.
    #[serde(default)]
    pub no_auth: bool,
    /// Optional per-provider default thinking level. Used when neither the
    /// CLI `:level` nor the model's own `thinking_level` selects one.
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    /// Per-provider thinking levels, in priority order. Models under this
    /// provider without their own `thinking_levels` inherit this list. Empty
    /// means the provider does not constrain levels (the agent default
    /// applies).
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
}

impl ProviderConfig {
    /// The default [`Api`] for this provider — the scalar `api_type` when
    /// set, else [`Api::OpenAiCompletions`].
    #[must_use]
    pub fn default_api(&self) -> Api {
        self.api_type.unwrap_or(Api::OpenAiCompletions)
    }

    /// The default api-type key (internal id string) for this provider — the
    /// scalar `api_type`'s id when set, else the constant default
    /// (`openai-completions`). Used to look up [`Self::api_types`] for models
    /// that do not name their own api-type.
    #[must_use]
    pub fn default_api_type_key(&self) -> String {
        self.default_api().id().to_string()
    }

    /// Resolve a model's [`Api`] from an optional api-type key (a per-model
    /// override or a discovered `preferred_api` already translated to an
    /// internal id). Falls back to the provider's default [`Api`] when the
    /// key is unset or unrecognized.
    #[must_use]
    pub fn resolve_api(&self, api_type: Option<&str>) -> Api {
        api_type
            .and_then(Api::parse)
            .unwrap_or_else(|| self.default_api())
    }

    /// Resolve a model's endpoint `path` for an api-type key (or the default),
    /// defaulting to [`Api::default_path`] for the resolved [`Api`] when the
    /// mapping does not set one.
    #[must_use]
    pub fn resolve_path(&self, api_type: Option<&str>) -> String {
        let default_key = self.default_api_type_key();
        let key = api_type.unwrap_or(&default_key);
        self.api_types
            .get(key)
            .and_then(|m| m.path.clone())
            .unwrap_or_else(|| self.resolve_api(api_type).default_path().to_string())
    }

    /// Resolve the pricing-field mappings for an api-type key, falling back to
    /// the provider-level default when the mapping does not carry its own.
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

/// Agent-level defaults.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Default thinking level applied when neither the CLI `:level`, the
    /// model, nor the provider selects one. Defaults to `medium` at
    /// resolution time when `None`.
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    /// Global thinking levels, in priority order. Providers and models
    /// without their own `thinking_levels` inherit this list. Empty means
    /// the agent does not constrain levels (the model/provider default
    /// applies).
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
}

/// Compaction settings. Currently governs the auto-compaction trigger
/// (the offline `/compact` is always available). Mirrors the
/// `autoCompact` block from pi's hm-smart-compact: the trigger fires only
/// on the upward crossing of the threshold, so a session hovering above the
/// threshold is not re-compacted every turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Auto-compaction trigger.
    #[serde(default)]
    pub auto: AutoCompactConfig,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self { auto: AutoCompactConfig::default() }
    }
}

/// Auto-compaction threshold configuration. The primary trigger is
/// `reserved_context_tokens`: compaction fires when the latest round's
/// input tokens exceed `context_window - reserved_context_tokens`, i.e.
/// when the remaining headroom for the model's response drops below the
/// reserve. The optional `max_context_tokens` and `context_ratio` caps
/// lower the threshold further when set (compacting earlier on large
/// windows); they are inert when unset. The trigger fires only on the
/// upward crossing of the threshold (hysteresis), so consecutive
/// above-threshold turns do not each trigger a compaction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoCompactConfig {
    /// Master switch. When `false` auto-compaction is disabled and only
    /// manual `/compact` runs.
    #[serde(default = "default_true")]
    pub enable: bool,
    /// Tokens to reserve for the model's response. Compaction triggers when
    /// `context_tokens > context_window - reserved_context_tokens`.
    /// Defaults to 20_000.
    #[serde(default = "default_reserved_context_tokens")]
    pub reserved_context_tokens: u64,
    /// Optional absolute cap. When set, the threshold is lowered to at most
    /// this many tokens, so compaction fires earlier on large context
    /// windows. Inert when unset.
    #[serde(default)]
    pub max_context_tokens: Option<u64>,
    /// Optional fraction of the context window in (0, 1]. When set, the
    /// threshold is lowered to at most `floor(context_window * ratio)`.
    /// Inert when unset.
    #[serde(default)]
    pub context_ratio: Option<f64>,
}

fn default_reserved_context_tokens() -> u64 {
    20_000
}

impl Default for AutoCompactConfig {
    fn default() -> Self {
        Self {
            enable: true,
            reserved_context_tokens: default_reserved_context_tokens(),
            max_context_tokens: None,
            context_ratio: None,
        }
    }
}

impl AutoCompactConfig {
    /// Resolve the effective trigger threshold for a context window: the
    /// reserve-based threshold (`window - reserved`), lowered by the
    /// optional absolute and ratio caps when they are set. Returns `None`
    /// when the window is zero or the resolved threshold is not positive.
    #[must_use]
    pub fn threshold(&self, context_window: u64) -> Option<u64> {
        if context_window == 0 {
            return None;
        }
        let mut threshold = context_window.saturating_sub(self.reserved_context_tokens);
        if let Some(cap) = self.max_context_tokens {
            threshold = threshold.min(cap);
        }
        if let Some(ratio) = self.context_ratio {
            if ratio > 0.0 && ratio <= 1.0 {
                threshold = threshold.min((ratio * context_window as f64) as u64);
            }
        }
        (threshold > 0).then_some(threshold)
    }
}

/// Top-level config tree parsed from `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Agent-level defaults.
    #[serde(default)]
    pub agent: AgentConfig,
    /// Compaction settings (auto-compaction trigger). `/compact` itself is
    /// always available regardless of this block.
    #[serde(default)]
    pub compaction: CompactionConfig,
    /// Default provider used when `--model` is omitted and no `default_model`
    /// resolves. Overrides the "first available provider" fallback.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default model as `provider/id` (or a bare id resolved against
    /// `default_provider`) used when `--model` is omitted. Takes precedence
    /// over `default_provider`'s first model and the first-available fallback.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Named providers, in config-file order. The first provider with a
    /// resolved key (or `no_auth`) supplies the default model when neither
    /// `default_model`, `default_provider`, nor `--model` selects one.
    pub providers: IndexMap<String, ProviderConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

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
        });
        round_trip(&ContentBlock::Thinking {
            text: "hmm".to_string(),
            signature: None,
        });
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
        });
    }

    #[test]
    fn streaming_event_round_trips() {
        round_trip(&StreamingEvent::TextDelta("x".to_string()));
        round_trip(&StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        });
        round_trip(&StreamingEvent::Done(Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        }));
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
                thinking_level: None,
                thinking_levels: Vec::new(),
            },
        );
        let cfg = Config {
            agent: AgentConfig::default(),
            default_provider: None,
            default_model: None,
            providers,
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
        let l: ThinkingLevel =
            serde_json::from_str(concat!('"', "medium", '"')).unwrap();
        assert_eq!(l, ThinkingLevel::Medium);
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
        assert_eq!(Api::parse("anthropic-messages"), Some(Api::AnthropicMessages));
        // snake_case still works.
        assert_eq!(Api::parse("openai_responses"), Some(Api::OpenAiResponses));
    }

    #[test]
    fn auto_models_defaults() {
        // `path` defaults to "data", `auth` to true, everything else optional.
        let json = "{}";
        let am: AutoModelsConfig = serde_json::from_str(json).unwrap();
        assert_eq!(am.path, "data");
        assert!(am.auth);
        assert!(!am.enabled);
        assert!(am.models_url.is_none());
    }
}