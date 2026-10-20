use std::collections::HashMap;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{Api, AutoContinueConfig, ServiceTier, ThinkingLevel};

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
    /// `label` it always emits the thinking level and the service tier, so a
    /// restored `off` level or `auto` tier is not silently replaced by a
    /// configured default.
    #[must_use]
    pub fn query(&self) -> String {
        format!(
            "{}/{}:{}@{}",
            self.provider,
            self.id,
            self.thinking.as_str(),
            self.service_tier.as_str(),
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

pub(crate) fn default_true() -> bool {
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
