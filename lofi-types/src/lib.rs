//! Pure data shapes shared across `lofi`.
//!
//! This crate holds only serde-friendly data types plus small helper methods,
//! with no behavior and no I/O. All enums use
//! `#[serde(rename_all = "snake_case")]`; `ContentBlock` is internally tagged so
//! it round-trips cleanly through JSON.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;
use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

pub mod recall;

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

/// A compressed, normalized view of one conversation message, used as the
/// intermediate representation for the compaction section extractors and the
/// brief transcript builder.
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
    /// The result of an exec call. `text` is the surfaced value (or the
    /// error message); `is_error` marks failures.
    ToolResult {
        id: String,
        text: String,
        is_error: bool,
    },
}

/// A named section produced by a compaction hook.
///
/// Each hook returns zero or more sections; each section is a title and a
/// list of bullet items (`- item` lines). Sections are inserted into the
/// summary between the stable built-in sections (Goal, Preferences, Files,
/// Commits) and the volatile section (Outstanding Context).
#[derive(Debug, Clone)]
pub struct SummarySection {
    /// The section title, shown as `[Title]` in the summary.
    pub title: String,
    /// Bullet items, each shown as `- item`.
    pub items: Vec<String>,
}

/// Hook trait allowing external crates (lofi-code) to inject custom summary
/// sections into the compaction output. The hook receives the normalized
/// transcript blocks and returns additional sections.
///
/// This avoids hardcoding tool-specific knowledge (like API descriptions)
/// in lofi-core; instead lofi-code registers a hook that knows its own tools.
pub trait CompactionHook: Send + Sync {
    /// Return additional summary sections for this compaction.
    /// `blocks` is the normalized transcript of the summarized prefix.
    fn sections(&self, blocks: &[CompactBlock]) -> Vec<SummarySection>;

    /// Return bullet items for the [Files And Changes] section.
    ///
    /// Inspects native tool calls to determine which files were modified,
    /// created, or read. Default returns empty (no file tracking).
    fn file_changes(&self, _blocks: &[CompactBlock]) -> Vec<String> {
        Vec::new()
    }

    /// Return bullet items for the [Commits] section.
    ///
    /// Inspects native tool calls for git commit commands. Default returns
    /// empty (no commit tracking).
    fn commits(&self, _blocks: &[CompactBlock]) -> Vec<String> {
        Vec::new()
    }

    /// Compress a tool result string for the brief transcript.
    ///
    /// Hooks that know the structure of their tool results (e.g. JSON
    /// format) can extract meaningful fields instead of just taking the
    /// first line. Default returns `None` (caller falls back to its own
    /// generic compression).
    fn compress_tool_result(&self, _text: &str, _max: usize) -> Option<String> {
        None
    }

    /// Extract a "full output" path from a tool result that was truncated.
    ///
    /// Hooks that produce truncation notices (e.g. bash output redirected to
    /// a temp file) can return the path so the brief transcript can reference
    /// it. Default returns None.
    fn full_output_path(&self, _text: &str) -> Option<String> {
        None
    }
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
    ThinkingTiming { elapsed_ms: u64 },
    /// A completed turn: the raw model identity that ran it, wall-clock
    /// duration, accumulated USD cost, and the final round's token usage.
    /// Rendered as the `◇ Done in Ns with <model>` block and folded into the
    /// status bar totals.
    TurnEnd {
        #[serde(alias = "label", default)]
        model: RunModel,
        elapsed_ms: u64,
        cost: f64,
        usage: Usage,
    },
    /// A turn that ended in failure (a non-retryable provider error or a
    /// user cancel): the raw model identity that ran it, wall-clock duration,
    /// the error message, and the cost/usage accumulated by the rounds that
    /// did run. Rendered as a `◇ Failed in Ns with <model>` marker. Its
    /// `parent_id` points at
    /// the turn's checkpoint (the last event before the failed turn started),
    /// so the active-path walk excludes the failed turn's messages from the
    /// agent's history on resume while keeping them visible in the tree.
    TurnFailed {
        #[serde(alias = "label", default)]
        model: RunModel,
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
        /// Event ids `[first, last]` of the summarized range on the active
        /// path — every live message folded into this summary. `/recall`
        /// with `scope:compaction:N` resolves these to global message
        /// indices and searches within the range. Empty strings mean
        /// compact-all collapsed the whole live list.
        summarized_range: [String; 2],
        /// True when the kept tail was copied immediately before this marker
        /// as a durable, context-edited checkpoint. UI replay suppresses those
        /// copies (the original turns remain visible); model-history replay
        /// reads them as the authoritative post-compaction tail. Older marker
        /// layouts default to false.
        #[serde(default)]
        checkpointed_tail: bool,
        /// How many live messages were folded by this compaction (for the
        /// visible marker on resume).
        summarized: usize,
        /// Total original messages represented by the merged summary. This
        /// preserves compaction's minimum-history bookkeeping across resume
        /// and repeated compactions. Older markers fall back to `summarized`.
        #[serde(default)]
        represented: usize,
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

/// One selectable model in the `/model` picker. Built from a model
/// registry's available models.
#[derive(Debug, Clone)]
pub struct ModelChoice {
    /// Provider key in the config.
    pub provider: String,
    /// Provider-local model identifier (e.g. `gpt-4o`).
    pub id: String,
    /// Human-readable display name.
    pub name: String,
    /// Declared reasoning/thinking levels (empty if the model has none).
    pub thinking_levels: Vec<ThinkingLevel>,
    /// Whether the model accepts image inputs.
    pub supports_image: bool,
    /// Context window in tokens, if known.
    pub context_window: Option<u64>,
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

/// The model identity that ran a turn (or started a session): raw data, no
/// presentation. Persisted on turn-end markers and the session header so a
/// resumed session renders the turn's original model rather than the
/// (possibly switched) active one. Rendered to `provider/id:level` only at
/// display time — never stored as a formatted string, so a later UI change
/// can't strand stale text in old session files.
///
/// Deserializes from the new object form (`{provider, id, thinking}`) or a
/// legacy rendered string (`provider/id:level`, `provider/id · level`, or
/// bare `provider/id`), so pre-change session files still load. Serializes
/// only as the object form.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct RunModel {
    pub provider: String,
    pub id: String,
    #[serde(default)]
    pub thinking: ThinkingLevel,
}

impl RunModel {
    /// Render the display label `provider/id` plus `:level` when thinking is
    /// on (empty when off). The single place this presentation is produced.
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

    /// Best-effort parse of a legacy rendered model string
    /// (`provider/id[:level]` or `provider/id · level`) back into raw data.
    /// Used to load pre-change session files and as a test convenience.
    #[must_use]
    pub fn parse(s: &str) -> Self {
        let (provider, rest) = match s.split_once('/') {
            Some((p, r)) => (p.to_string(), r),
            None => (String::new(), s),
        };
        // ` · level` (agent) and `:level` (app) both appeared in the wild.
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
    /// Subagent concurrency settings (`[agent.subagents]`).
    #[serde(default)]
    pub subagents: SubagentConfig,
}

/// Subagent settings (`[agent.subagents]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentConfig {
    /// Maximum number of subagents (`lofi.agent()` calls) that may run
    /// concurrently. Defaults to `3`; set to `0` to disable throttling.
    /// Excess calls wait for a slot before starting.
    #[serde(default = "default_subagent_max_concurrent")]
    pub max_concurrent: usize,
}

const fn default_subagent_max_concurrent() -> usize {
    3
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_subagent_max_concurrent(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod subagent_config_tests {
    use super::SubagentConfig;

    #[test]
    fn default_concurrency_is_three() {
        assert_eq!(SubagentConfig::default().max_concurrent, 3);
        let parsed: SubagentConfig =
            serde_json::from_str("{}").expect("empty subagent config parses");
        assert_eq!(parsed.max_concurrent, 3);
    }
}

/// Compaction settings.
///
/// `reserved_context_tokens` is the **hard cap**: a run whose round input
/// tokens exceed `context_window - reserved_context_tokens` is force-stopped
/// mid-run, compacted, and silently continued. Consecutive force-compacts are
/// gated by `min_messages_between_hard_compacts` — if the run crosses the
/// hard cap again within that many agent messages of the last force-compact,
/// it errors out (the kept tail itself is too big to compact further).
///
/// The optional `[compaction.auto]` **soft caps** are speculative: a run may
/// cross them with no interruption, and when it reaches `agent_settled` with
/// context above the soft threshold, it compacts. Soft compaction only runs
/// when at least one soft cap is set; with defaults (reserved only) compaction
/// is hard-cap-only.
///
/// The offline `/compact` is always available regardless of these settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    /// Hard cap: tokens reserved for the model's response. A round whose
    /// input tokens exceed `context_window - reserved_context_tokens`
    /// triggers a force-compact. Defaults to `20_000`.
    #[serde(default = "default_reserved_context_tokens")]
    pub reserved_context_tokens: u64,
    /// Minimum agent messages that must elapse between two force-compacts.
    /// If a continued run crosses the hard cap again within this many
    /// messages of the last force-compact, it errors out instead of
    /// compacting again. Defaults to `6`.
    #[serde(default = "default_min_messages_between_hard_compacts")]
    pub min_messages_between_hard_compacts: usize,
    /// Speculative auto-compaction (soft caps + master switch).
    #[serde(default)]
    pub auto: AutoCompactConfig,
    /// Tiered-retention context editing applied to the kept tail at each
    /// compaction (elide old tool results / thinking / tool-call code,
    /// recoverable via `lofi.result`). Cache-safe: it rides the prefix
    /// rebuild that compaction already pays.
    #[serde(default)]
    pub edit: EditConfig,
}

fn default_reserved_context_tokens() -> u64 {
    20_000
}

fn default_min_messages_between_hard_compacts() -> usize {
    6
}

/// Settings for the `bash` native tool's child-process environment.
///
/// By default the child env is *stripped* to a minimal baseline (`PATH`,
/// `HOME`, locale, …) so inherited credentials never reach a model-run
/// shell. `pass_env` and `env_file` opt specific variables back in; their
/// values are redacted (`[redacted]`) from captured stdout/stderr before the
/// model sees them, so a command may *use* a secret without it leaking into
/// the transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BashConfig {
    /// Strip the inherited environment down to a minimal baseline before
    /// running a command. `false` inherits the full parent environment (an
    /// explicit trust opt-out; redaction of `pass_env`/`env_file` values still
    /// applies). Defaults to `true`.
    #[serde(default = "default_true")]
    pub strip_env: bool,
    /// Env var names to copy from the parent environment into the child on
    /// top of the baseline (or the inherited env when `strip_env` is false).
    /// Their values are redacted from output. Empty by default.
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

/// Shell policy enforcement mode.
///
/// - `ReadOnly` — only read-only commands (ls, cat, grep, git status, …).
/// - `WorkspaceWrite` — read-only plus workspace mutations (cargo, make,
///   mkdir, …); destructive ops denied or asked.
/// - `Unrestricted` — everything allowed unless explicitly denied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ShellPolicyMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    Unrestricted,
}

/// How a [`CommandEntry`] matches against a parsed command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchMode {
    /// Entire trimmed command string matches (case-insensitive).
    Exact,
    /// Command starts with `match` followed by a word boundary.
    Prefix,
    /// The exact contiguous token sequence exists anywhere in the command.
    Substring,
    /// `programPrefix:arg1 arg2…` — prefix matches the command start, and
    /// all listed args appear as tokens after the prefix.
    Args,
}

/// One rule in an allow/ask/deny list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEntry {
    /// The pattern to match. Meaning depends on [`MatchMode`].
    #[serde(rename = "match")]
    pub match_str: String,
    /// How to interpret `match`.
    pub mode: MatchMode,
}

/// Wrapper kind for recursive command unwrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WrapperKind {
    /// `bash -c 'cmd'` — next operand after `-c` is the command string.
    ShellC,
    /// `sudo cmd`, `time cmd`, `nohup cmd` — first non-option operand.
    UtilityOperand,
    /// `env VAR=1 cmd` — skip env assignments, first remaining operand.
    Env,
    /// `xargs cmd` — same as utility-operand.
    Xargs,
    /// `docker run … image cmd` — skip flags + image name.
    DockerRun,
}

/// A wrapper rule mapping a command name to its unwrapping strategy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrapperRuleConfig {
    pub name: String,
    pub kind: WrapperKind,
}

/// Policy for output redirects (`>`, `>>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedirectPolicy {
    /// Action for non-safe redirect targets.
    #[serde(default = "default_redirect_action")]
    pub action: PolicyAction,
    /// Targets that are always allowed (e.g. `/dev/null`).
    #[serde(default)]
    pub safe_targets: Vec<String>,
    /// Allow `>&N` file-descriptor duplication.
    #[serde(default)]
    pub allow_fd_dup: bool,
}

fn default_redirect_action() -> PolicyAction {
    PolicyAction::Allow
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

/// Policy for heredocs (`<<`, `<<-`).
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

/// Action returned by the policy engine.
///
/// Serialized as a string for config files (`"allow"`, `"ask"`, `"deny"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyAction {
    Allow,
    Ask,
    Deny,
}

/// Auto-mode configuration for shell policy.
///
/// When enabled, commands that would normally require user confirmation
/// (policy `ask` or unmatched `default`) are first evaluated by a small
/// LLM. If the model returns "allow", the command runs without prompting
/// the user. Any other outcome (or a timeout/failure) falls back to the
/// normal confirmation flow.
///
/// This is distinct from YOLO mode: YOLO blindly allows anything not
/// denied, while auto-mode makes a per-command safety judgment via an
/// LLM call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoModeConfig {
    /// Master switch.
    #[serde(default)]
    pub enable: bool,
    /// Provider key in the config (e.g. "openai").
    pub provider: String,
    /// Model id within the provider (e.g. "gpt-4o-mini").
    pub model: String,
    /// Timeout in milliseconds for the LLM evaluation. Defaults to 30s.
    #[serde(default = "default_auto_mode_timeout_ms")]
    pub timeout_ms: u64,
    /// Max output tokens for the evaluation response.
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

fn default_auto_mode_timeout_ms() -> u64 {
    30_000
}

/// Shell policy configuration, parsed from the `[shell_policy]` table.
///
/// The `mode` selects a built-in default policy; custom `allow`/`ask`/`deny`
/// rules are merged on top. When `yolo` is true, `ask` and unmatched
/// (`default`) commands are treated as `allow` — only explicit `deny`
/// blocks execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ShellPolicyConfig {
    /// Built-in policy preset. Defaults to `workspace_write`.
    #[serde(default)]
    pub mode: ShellPolicyMode,
    /// Allow-unless-deny: skip confirmation for `ask`/`default` commands.
    #[serde(default)]
    pub yolo: bool,
    /// Custom allow rules merged on top of the mode's defaults.
    #[serde(default)]
    pub allow: Vec<CommandEntry>,
    /// Custom ask rules merged on top of the mode's defaults.
    #[serde(default)]
    pub ask: Vec<CommandEntry>,
    /// Custom deny rules merged on top of the mode's defaults.
    #[serde(default)]
    pub deny: Vec<CommandEntry>,
    /// Custom wrapper rules merged on top of the mode's defaults.
    #[serde(default)]
    pub wrappers: Vec<WrapperRuleConfig>,
    /// Redirect policy. Defaults to allow.
    #[serde(default)]
    pub redirects: RedirectPolicy,
    /// Heredoc policy. Defaults to ask.
    #[serde(default)]
    pub heredocs: HeredocPolicy,
    /// Auto-mode: LLM-based pre-approval of `ask` commands. When `None`
    /// or `enable: false`, the normal confirmation flow is used.
    #[serde(default)]
    pub auto_mode: Option<AutoModeConfig>,
}

/// Transient-error retry settings for provider/transport failures.
///
/// A failed round whose error matches a transient pattern (overloaded, rate
/// limit, 429/5xx, network drops, stream truncation) is retried after an
/// exponential backoff: attempt N waits `base_delay_ms * 2^(N-1)`, clamped to
/// `max_delay_ms`. Non-transient errors (auth, quota/billing exhaustion,
/// context overflow, bad requests) are never retried — the round surfaces them
/// immediately. `max_retries: 0` disables retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryConfig {
    /// Maximum retry attempts after the initial try. Defaults to `10`.
    #[serde(default = "default_retry_max_retries")]
    pub max_retries: u32,
    /// Base delay (ms) for the first retry; later retries double it, clamped
    /// by `max_delay_ms`. Defaults to `2000` (2s).
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
    /// Hard-cap threshold: `context_window - reserved_context_tokens`. A run
    /// crossing this mid-run is force-stopped and compacted. Returns `None`
    /// when the window is zero or the reserve leaves no positive headroom.
    #[must_use]
    pub fn hard_threshold(&self, context_window: u64) -> Option<u64> {
        if context_window == 0 {
            return None;
        }
        let threshold = context_window.saturating_sub(self.reserved_context_tokens);
        (threshold > 0).then_some(threshold)
    }

    /// Soft-cap threshold: the lesser of the set optional caps
    /// (`max_context_tokens`, `floor(window * context_ratio)`). Returns
    /// `None` when neither cap is set (or they are out of range) — in which
    /// case there is no speculative compaction, only the hard cap.
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

/// Speculative auto-compaction soft caps. These lower the compaction
/// threshold below the reserve-based hard cap so large context windows
/// compact earlier. They are "soft" — the trigger fires only at
/// `agent_settled` (between turns), not mid-run. Both caps are optional
/// and inert when unset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoCompactConfig {
    /// Master switch. When `false` auto-compaction is disabled and only
    /// manual `/compact` runs.
    #[serde(default = "default_true")]
    pub enable: bool,
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
///
/// Elided tool results and tool-call code are replaced with recall-
/// recoverable stubs naming an event id the model can re-expand with
/// `lofi.result`. Thinking blocks are dropped outright (scratchpad; the
/// conclusion lives in the assistant text). Assistant prose is always kept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditConfig {
    /// Master switch. When `false` the kept tail is carried verbatim (the
    /// pre-edit behavior). Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Number of most-recent tool-result blocks kept verbatim in the tail.
    /// Older results are replaced with a `lofi.result`-recoverable stub.
    /// Defaults to `6`.
    #[serde(default = "default_edit_keep_results")]
    pub keep_results: usize,
    /// Number of most-recent thinking blocks kept verbatim. Older thinking
    /// blocks are dropped (the assistant text conclusion is kept). Defaults
    /// to `2`.
    #[serde(default = "default_edit_keep_thinking")]
    pub keep_thinking: usize,
    /// Number of most-recent tool-call (exec) blocks whose full `code` is
    /// kept. Older calls keep their `display` label (intent) but the
    /// verbatim code is replaced with a `lofi.result`-recoverable stub.
    /// Defaults to `6`.
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
    /// `bash` native-tool environment settings.
    #[serde(default)]
    pub bash: BashConfig,
    /// Shell policy enforcement for `lofi.bash`.
    ///
    /// Loaded from `policy.toml`, not from `config.toml`.
    #[serde(skip)]
    pub shell_policy: ShellPolicyConfig,
    /// Transient-error retry budget and backoff schedule.
    #[serde(default)]
    pub retry: RetryConfig,
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
    fn retry_config_defaults() {
        let cfg = RetryConfig::default();
        assert_eq!(cfg.max_retries, 10);
        assert_eq!(cfg.base_delay_ms, 2000);
        assert_eq!(cfg.max_delay_ms, 60_000);
    }

    #[test]
    fn retry_config_serde_omitted_uses_defaults() {
        // An empty `[retry]` block (or none) yields the default policy.
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
        // Unset fields keep their defaults.
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
        // snake_case still works.
        assert_eq!(Api::parse("openai_responses"), Some(Api::OpenAiResponses));
    }

    #[test]
    fn compaction_hard_threshold_reserved() {
        let cfg = CompactionConfig::default();
        // 200k window, 20k reserved -> 180k hard cap.
        assert_eq!(cfg.hard_threshold(200_000), Some(180_000));
        // Caps do not affect the hard threshold.
        let with_caps = CompactionConfig {
            auto: AutoCompactConfig {
                max_context_tokens: Some(100_000),
                context_ratio: Some(0.5),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(with_caps.hard_threshold(200_000), Some(180_000));
        // saturating sub when reserved >= window -> no positive threshold.
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
        // No caps set -> no soft threshold (hard-cap-only).
        assert_eq!(CompactionConfig::default().soft_threshold(200_000), None);
        // Absolute cap.
        let with_cap = CompactionConfig {
            auto: AutoCompactConfig {
                max_context_tokens: Some(150_000),
                ..AutoCompactConfig::default()
            },
            ..CompactionConfig::default()
        };
        assert_eq!(with_cap.soft_threshold(1_000_000), Some(150_000));
        assert_eq!(with_cap.soft_threshold(200_000), Some(150_000));
        // Ratio cap; out-of-range ratio is ignored.
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
        // Both caps -> the lesser.
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
        // [compaction] omitted -> reserved 20k, auto defaults.
        let cfg: CompactionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.reserved_context_tokens, 20_000);
        assert_eq!(cfg.min_messages_between_hard_compacts, 6);
        assert!(cfg.auto.enable);
        assert!(cfg.auto.max_context_tokens.is_none());
        assert!(cfg.auto.context_ratio.is_none());
    }

    #[test]
    fn bash_config_serde_defaults() {
        // [bash] omitted -> strip on, nothing passed, no env file.
        let cfg: BashConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.strip_env);
        assert!(cfg.pass_env.is_empty());
        assert!(cfg.env_file.is_none());
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
        // [compaction.auto] omitted -> enable=true, caps None (no reserved
        // field here; it lives on the parent [compaction]).
        let cfg: AutoCompactConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.enable);
        assert!(cfg.max_context_tokens.is_none());
        assert!(cfg.context_ratio.is_none());
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
