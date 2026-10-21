use std::path::PathBuf;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::model::default_true;
use crate::{ProviderConfig, ServiceTier, ThinkingLevel};

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
    /// The model to run: an id, or a model query
    /// `provider-or-backend/model[:level][@tier]`.
    pub model: String,
    /// The provider or backend that runs `model`, in a config written before
    /// model queries. Read the pair through [`AutoModeConfig::query`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
}

impl AutoModeConfig {
    /// The model query to resolve.
    ///
    /// A config that names a provider and an id separately joins them into one
    /// query, which is what the two fields meant together. A model that is
    /// already qualified keeps the provider it names.
    #[must_use]
    pub fn query(&self) -> String {
        match &self.provider {
            Some(provider) if !self.model.contains('/') => format!("{provider}/{}", self.model),
            _ => self.model.clone(),
        }
    }
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

/// External agent invocation. Login and credentials remain owned by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BackendConfig {
    ClaudeCode {
        #[serde(default = "default_claude_command")]
        command: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        models: Vec<String>,
        /// The window Lofi budgets for this backend. The child reports its
        /// own window, but compaction needs a window before the first turn,
        /// exactly as a provider model does.
        #[serde(default = "default_claude_context_window")]
        context_window: Option<u64>,
    },
}

impl BackendConfig {
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        match self {
            Self::ClaudeCode { model, .. } => model.as_deref(),
        }
    }

    #[must_use]
    pub fn models(&self) -> &[String] {
        match self {
            Self::ClaudeCode { models, .. } => models,
        }
    }

    #[must_use]
    pub fn context_window(&self) -> Option<u64> {
        match self {
            Self::ClaudeCode { context_window, .. } => *context_window,
        }
    }
}

fn default_claude_command() -> String {
    "claude".into()
}

/// The standard Claude Code window. A turn that omits it can still run; the
/// hard cap then waits for the first reported fill.
#[allow(clippy::unnecessary_wraps)]
fn default_claude_context_window() -> Option<u64> {
    Some(200_000)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub backends: IndexMap<String, BackendConfig>,
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
    #[serde(default)]
    pub providers: IndexMap<String, ProviderConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backends: IndexMap::new(),
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
