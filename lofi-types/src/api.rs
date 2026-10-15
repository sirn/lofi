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
