use serde::{Deserialize, Serialize};

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
    #[serde(rename = "systemone", alias = "system-one")]
    SystemOne,
}

impl Api {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai_completions",
            Self::OpenAiResponses => "openai_responses",
            Self::AnthropicMessages => "anthropic_messages",
            Self::GoogleGenerativeAi => "google_generative_ai",
            Self::SystemOne => "systemone",
        }
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerativeAi => "google-generative-ai",
            Self::SystemOne => "systemone",
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "OpenAI Chat Completions",
            Self::OpenAiResponses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
            Self::GoogleGenerativeAi => "Google Generative AI",
            Self::SystemOne => "TypeSafe System One",
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
            "systemone" | "system-one" => Some(Self::SystemOne),
            _ => None,
        }
    }

    /// Whether this API answers typed decision questions instead of chat
    /// completions. A decision model cannot run a chat turn, so every chat
    /// path rejects it; only a decision call can open it.
    #[must_use]
    pub fn is_decision(self) -> bool {
        matches!(self, Self::SystemOne)
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
            Self::SystemOne => "https://api.typesafe.ai",
        }
    }

    #[must_use]
    pub fn default_path(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "/v1/chat/completions",
            Self::OpenAiResponses => "/v1/responses",
            Self::AnthropicMessages => "/v1/messages",
            Self::GoogleGenerativeAi => "/v1beta",
            Self::SystemOne => "/v1/systemone",
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
