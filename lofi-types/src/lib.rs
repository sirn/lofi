//! Pure data shapes shared across `lofi`.
//!
//! This crate holds only serde-friendly data types plus small helper methods,
//! mirroring the `fabric-types` convention: no behavior, no I/O. All enums use
//! `#[serde(rename_all = "snake_case")]`; `ContentBlock` is internally tagged so
//! it round-trips cleanly through JSON.

#![cfg_attr(test, allow(clippy::unwrap_used))]

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Provider wire protocol used to talk to a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Api {
    /// `OpenAI` Chat Completions (`/chat/completions`).
    #[serde(rename = "openai_completions")]
    OpenAiCompletions,
    /// `OpenAI` Responses API (`/responses`).
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Anthropic Messages API (`/v1/messages`).
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl Api {
    /// Return the `snake_case` identifier used in config files.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
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

    /// Parse a config identifier back into an [`Api`].
    ///
    /// Returns `None` for unrecognized strings rather than erroring, so callers
    /// can attach their own diagnostic context.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai_completions" => Some(Self::OpenAiCompletions),
            "openai_responses" => Some(Self::OpenAiResponses),
            "anthropic_messages" => Some(Self::AnthropicMessages),
            _ => None,
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
    Thinking { text: String },
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
    /// Whether the model accepts image inputs.
    #[serde(default)]
    pub supports_image: bool,
    /// Context window size in tokens, if known.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// Max output tokens, if known.
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

/// A model entry declared statically in TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelConfig {
    /// Provider-local model identifier.
    pub id: String,
    /// Optional display name; defaults to the id when absent.
    #[serde(default)]
    pub name: Option<String>,
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
}

fn default_discovery_path() -> String {
    "data".to_string()
}

/// Remote model-discovery settings for a provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscoveryConfig {
    /// Path appended to `base_url` to fetch the model list.
    pub url: String,
    /// JSON pointer-ish path (dot-separated) to the array of models in the
    /// response; defaults to `data`.
    #[serde(default = "default_discovery_path")]
    pub path: String,
    /// Optional field name overriding the per-provider `api` for discovered
    /// models.
    #[serde(default)]
    pub api_field: Option<String>,
}

/// A provider entry in the config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Base URL for the provider's API.
    pub base_url: String,
    /// Wire protocol used by this provider.
    pub api: Api,
    /// API key or value-resolution expression (literal, `$ENV`, `!cmd`).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Extra HTTP headers to send (values subject to resolution).
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// Statically declared models.
    #[serde(default)]
    pub models: Vec<ModelConfig>,
    /// Optional remote discovery endpoint.
    #[serde(default)]
    pub discover: Option<DiscoveryConfig>,
}

/// Top-level config tree parsed from `config.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Named providers.
    pub providers: HashMap<String, ProviderConfig>,
    /// Default provider key used when none is requested.
    #[serde(default)]
    pub default_provider: Option<String>,
    /// Default `provider/id` (or bare id) used when none is requested.
    #[serde(default)]
    pub default_model: Option<String>,
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
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                base_url: "https://api.openai.com/v1".to_string(),
                api: Api::OpenAiCompletions,
                api_key: Some("$OPENAI_API_KEY".to_string()),
                headers: None,
                models: vec![ModelConfig {
                    id: "gpt-4o".to_string(),
                    name: None,
                    reasoning: None,
                    supports_image: Some(true),
                    context_window: Some(128_000),
                    max_tokens: None,
                }],
                discover: None,
            },
        );
        let cfg = Config {
            providers,
            default_provider: Some("openai".to_string()),
            default_model: Some("openai/gpt-4o".to_string()),
        };
        round_trip(&cfg);
    }

    #[test]
    fn discovery_path_default() {
        let json = "{\"url\":\"/v1/models\"}";
        let dc: DiscoveryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(dc.path, "data");
        assert_eq!(dc.url, "/v1/models");
    }
}
