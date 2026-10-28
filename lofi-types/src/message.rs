use serde::{Deserialize, Serialize};

use crate::{Api, Model};

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
    /// A subagent's task, written by the parent agent. It renders like a
    /// prompt but is never trusted as user authorization.
    Task,
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
    /// marks Anthropic redacted thinking: text is a placeholder, and signature
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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<ContentBlock>,
    /// Provider, API, and model that produced this assistant message.
    /// Provider-specific replay metadata in its blocks is valid only when
    /// this identity matches the target model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ModelOrigin>,
    /// Origin of a user-role prompt. Distinguishes typed input from
    /// app-injected notices so replay does not need a separate marker
    /// event. Defaults to `User` so older transcripts remain loadable.
    #[serde(default, skip_serializing_if = "is_default_prompt_kind")]
    pub kind: PromptKind,
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct ContentBlockWire {
            #[serde(flatten)]
            block: ContentBlock,
            #[serde(default)]
            origin: Option<serde_json::Value>,
        }

        #[derive(Deserialize)]
        struct MessageWire {
            role: Role,
            blocks: Vec<ContentBlockWire>,
            #[serde(default)]
            origin: Option<ModelOrigin>,
            #[serde(default)]
            kind: PromptKind,
        }

        let wire = MessageWire::deserialize(deserializer)?;
        // Legacy thinking blocks can carry per-block provenance. Lift it only
        // when every block agrees because message provenance covers all blocks.
        let origin = if let Some(origin) = wire.origin {
            Some(origin)
        } else {
            let origins = wire
                .blocks
                .iter()
                .filter(|block| matches!(block.block, ContentBlock::Thinking { .. }))
                .map(|block| {
                    block
                        .origin
                        .clone()
                        .map(serde_json::from_value)
                        .transpose()
                        .map_err(serde::de::Error::custom)
                })
                .collect::<Result<Vec<Option<ModelOrigin>>, D::Error>>()?;
            origins.first().and_then(|first| {
                first
                    .as_ref()
                    .filter(|_| origins.iter().all(|origin| origin == first))
                    .cloned()
            })
        };
        let blocks = wire.blocks.into_iter().map(|block| block.block).collect();
        Ok(Self {
            role: wire.role,
            blocks,
            origin,
            kind: wire.kind,
        })
    }
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

/// Identity of the model that produced an assistant message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOrigin {
    pub provider: String,
    pub model: String,
    pub api: Api,
}

impl ModelOrigin {
    #[must_use]
    pub fn matches(&self, model: &Model) -> bool {
        self.provider == model.provider && self.model == model.id && self.api == model.api
    }
}

impl From<&Model> for ModelOrigin {
    fn from(model: &Model) -> Self {
        Self {
            provider: model.provider.clone(),
            model: model.id.clone(),
            api: model.api,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PartSignatureFormat {
    Google,
    OpenAiExtraContent { namespace: String },
    OpenAiReasoningDetail,
}
