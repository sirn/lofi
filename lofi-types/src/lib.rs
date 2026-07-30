#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub mod recall;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Api {
    #[serde(rename = "openai_completions", alias = "openai-completions")]
    OpenAiCompletions,
    #[serde(rename = "openai_responses", alias = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "anthropic_messages", alias = "anthropic-messages")]
    AnthropicMessages,
}

impl Api {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
        }
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "OpenAI Chat Completions",
            Self::OpenAiResponses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
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

    #[must_use]
    pub fn default_path(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
            Self::AnthropicMessages => "/v1/messages",
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
    },
    /// Chain-of-thought / reasoning trace (where the API exposes it).
    Thinking {
        text: String,
        signature: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingEvent {
    TextDelta(String),
    ToolUseStart { id: String, name: String },
    ToolUseInputDelta { id: String, delta: String },
    ToolUseEnd { id: String },
    ThinkingDelta(String),
    ThinkingSignature(String),
    Done(Usage),
    Error(String),
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
    UserBash {
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
    /// A turn explicitly interrupted by the user. Like Pi's aborted assistant
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
        /// copies only for legacy attached checkpoints; detached checkpoints
        /// use the copies as their visible retained tail.
        #[serde(default)]
        checkpointed_tail: bool,
        /// New checkpoints start a fresh event lineage, allowing active resume
        /// indexing to stay bounded after repeated compactions.
        #[serde(default)]
        detached: bool,
        /// The selected leaf immediately before a detached checkpoint. The old
        /// tree remains physically available for recall and rollback without
        /// remaining ancestral to the active model context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_leaf_id: Option<String>,
        summarized: usize,
        #[serde(default)]
        represented: usize,
        kept: usize,
    },
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
}

impl RunModel {
    #[must_use]
    pub fn label(&self) -> String {
        format!(
            "{}/{}{}",
            self.provider,
            self.id,
            if self.thinking == ThinkingLevel::Off {
                String::new()
            } else {
                format!(":{}", self.thinking.as_str())
            }
        )
    }

    #[must_use]
    pub fn parse(s: &str) -> Self {
        let (provider, rest) = match s.split_once('/') {
            Some((p, r)) => (p.to_string(), r),
            None => (String::new(), s),
        };
        let (id, thinking) = if let Some((i, lvl)) = rest.split_once(" · ") {
            (
                i.to_string(),
                ThinkingLevel::parse(lvl.trim()).unwrap_or_default(),
            )
        } else if let Some((i, lvl)) = rest.rsplit_once(':') {
            // Only treat the suffix as a thinking level when it parses;
            // otherwise the colon is part of the model id (core's model
            // query allows colons in ids) and must be preserved.
            match ThinkingLevel::parse(lvl.trim()) {
                Some(t) => (i.to_string(), t),
                None => (rest.to_string(), ThinkingLevel::Off),
            }
        } else {
            (rest.to_string(), ThinkingLevel::Off)
        };
        Self {
            provider,
            id,
            thinking,
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
            },
            Str(String),
        }
        Ok(match Repr::deserialize(deserializer)? {
            Repr::Obj {
                provider,
                id,
                thinking,
            } => Self {
                provider,
                id,
                thinking,
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
    #[serde(default)]
    pub thinking_level: Option<ThinkingLevel>,
    #[serde(default)]
    pub thinking_levels: Vec<ThinkingLevel>,
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
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub bash: BashConfig,
    #[serde(skip)]
    pub shell_policy: ShellPolicyConfig,
    #[serde(default)]
    pub retry: RetryConfig,
    #[serde(default)]
    pub default_provider: Option<String>,
    #[serde(default)]
    pub default_model: Option<String>,
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
            compaction: CompactionConfig::default(),
            bash: BashConfig::default(),
            shell_policy: ShellPolicyConfig::default(),
            retry: RetryConfig::default(),
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
        assert_eq!(both.soft_threshold(400_000), Some(150_000)); // min(150k, 200k)
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
