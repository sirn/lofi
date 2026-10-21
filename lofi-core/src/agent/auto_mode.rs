use futures::StreamExt;
use lofi_code::policy::auto_mode::{self, AutoModeRisk};
use lofi_code::{AutoModeFn, AutoModeHintHandle, AutoModeHints, AutoModeOutcome};
use lofi_providers::{open, Provider};
use lofi_types::{AutoModeConfig, Config, ContentBlock, Message, Model, Role};
use std::sync::Arc;

use lofi_error::{Error, Result};

/// A fresh, empty context handle. Each agent gets its own so a subagent
/// cannot read the hints of the session that spawned it.
#[must_use]
pub fn hint_handle() -> AutoModeHintHandle {
    Arc::new(std::sync::Mutex::new(AutoModeHints::default()))
}

/// Abort a spawned provider evaluation if the surrounding auto-mode future is
/// dropped because the user overrides it. Dropping a bare Tokio `JoinHandle`
/// would detach the request and let it keep consuming provider resources.
struct AbortOnDrop(Option<tokio::task::AbortHandle>);

impl AbortOnDrop {
    fn disarm(&mut self) {
        self.0 = None;
    }
}

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = &self.0 {
            handle.abort();
        }
    }
}

/// # Errors
/// Returns [`Error::Config`] when the auto-mode config names a provider or
/// model that does not exist or is not available (no credentials).
pub fn build_auto_mode(
    config: &Config,
    registry: &crate::models::ModelRegistry,
    auto_cfg: &AutoModeConfig,
    cwd: &std::path::Path,
    skills_dir: Option<&std::path::Path>,
    hints: &AutoModeHintHandle,
) -> Result<Option<AutoModeFn>> {
    if !auto_cfg.enable {
        return Ok(None);
    }

    let qualified = format!("{}/{}", auto_cfg.provider, auto_cfg.model);
    let model = registry.resolve(&qualified).cloned().ok_or_else(|| {
        Error::Config(format!(
            "auto-mode model \"{qualified}\" not found in registry"
        ))
    })?;

    let provider_cfg = config.providers.get(&auto_cfg.provider).ok_or_else(|| {
        Error::Config(format!(
            "auto-mode provider \"{}\" not found in config",
            auto_cfg.provider
        ))
    })?;

    let is_available = provider_cfg.no_auth
        || provider_cfg
            .api_key
            .as_deref()
            .is_some_and(|k| !k.is_empty())
        || provider_cfg
            .headers
            .as_ref()
            .is_some_and(|h| h.values().any(|v| !v.is_empty()));
    if !is_available {
        return Err(Error::Config(format!(
            "auto-mode provider \"{}\" has no credentials",
            auto_cfg.provider
        )));
    }

    let provider: Arc<dyn Provider> = Arc::from(open(model.api, provider_cfg)?);
    let max_tokens = auto_cfg.max_tokens.or(model.max_tokens);
    let cwd = cwd.to_path_buf();

    // Skill roots that actually exist are marked pre-vetted in the evaluator
    // prompt so installed skill scripts stop triggering confirmation.
    let mut skill_dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(dir) = skills_dir.filter(|d| d.is_dir()) {
        skill_dirs.push(dir.to_path_buf());
    }
    let ws_skills = cwd.join(".lofi").join("skills");
    if ws_skills.is_dir() {
        skill_dirs.push(ws_skills);
    }

    let hints = hints.clone();
    let auto_mode_fn: AutoModeFn = Arc::new(move |command: String| {
        let provider = provider.clone();
        let model = model.clone();
        let cwd = cwd.clone();
        let skill_dirs = skill_dirs.clone();
        let turn = hints
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Box::pin(async move {
            let handle = tokio::task::spawn(async move {
                evaluate_command(
                    &provider,
                    &model,
                    &command,
                    &cwd,
                    max_tokens,
                    &skill_dirs,
                    &turn,
                )
                .await
            });
            let mut abort_on_drop = AbortOnDrop(Some(handle.abort_handle()));
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(e) => AutoModeOutcome::Failed {
                    reason: format!("evaluation task failed: {e}"),
                },
            };
            abort_on_drop.disarm();
            outcome
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
    });

    Ok(Some(auto_mode_fn))
}

pub(super) async fn evaluate_command(
    provider: &Arc<dyn Provider>,
    model: &Model,
    command: &str,
    cwd: &std::path::Path,
    max_tokens: Option<u64>,
    skill_dirs: &[std::path::PathBuf],
    turn: &AutoModeHints,
) -> AutoModeOutcome {
    let skill_dirs: Vec<&std::path::Path> =
        skill_dirs.iter().map(std::path::PathBuf::as_path).collect();
    // The system message is the same on every call so the provider's prompt
    // cache covers it; only the user message changes per command.
    let user =
        auto_mode::build_user_message(command, &cwd.display().to_string(), &skill_dirs, turn);

    let mut model = model.clone();
    model.thinking = lofi_types::ThinkingLevel::Off;
    if let Some(mt) = max_tokens {
        model.max_tokens = Some(mt);
    }

    let messages = vec![
        Message {
            origin: None,
            role: Role::System,
            blocks: vec![ContentBlock::Text {
                text: auto_mode::system_prompt().to_string(),
            }],
            kind: lofi_types::PromptKind::User,
        },
        Message {
            origin: None,
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: user }],
            kind: lofi_types::PromptKind::User,
        },
    ];

    let started = std::time::Instant::now();
    let stream = match provider.stream(&model, &messages, &[]).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "auto-mode: provider stream failed");
            return AutoModeOutcome::Failed {
                reason: format!("provider request failed: {e}"),
            };
        }
    };

    match collect_text(stream).await {
        Ok((text, usage)) => {
            let decision = auto_mode::parse_decision(&text);
            if let Some(d) = decision {
                tracing::debug!(
                    command = command,
                    risk = ?d.risk,
                    reason = d.reason.as_str(),
                    elapsed_ms = started.elapsed().as_millis(),
                    input_tokens = usage.input_tokens,
                    output_tokens = usage.output_tokens,
                    cache_read_tokens = usage.cache_read_tokens,
                    "auto-mode: evaluated"
                );
                match d.risk {
                    AutoModeRisk::Low => AutoModeOutcome::Allow { reason: d.reason },
                    AutoModeRisk::Confirm => AutoModeOutcome::Ask { reason: d.reason },
                    AutoModeRisk::Block => AutoModeOutcome::HighRisk { reason: d.reason },
                }
            } else {
                tracing::warn!(
                    response = text.as_str(),
                    "auto-mode: could not parse LLM response"
                );
                AutoModeOutcome::Failed {
                    reason: "evaluator returned an invalid response".to_string(),
                }
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "auto-mode: stream error");
            AutoModeOutcome::Failed {
                reason: format!("evaluation stream failed: {e}"),
            }
        }
    }
}

/// Derive the evaluator context for the turn that is starting. Derived from
/// the live history instead of accumulated, so compaction, rewind, and a new
/// session drop the context without extra bookkeeping.
///
/// The current request is always passed. The earlier request and the agent's
/// last message are passed only for a terse request, because "yes" answers
/// the agent's question and cannot be judged on its own.
#[must_use]
pub fn derive_hints(messages: &[Message], current: Option<&str>) -> AutoModeHints {
    let Some(current) = current else {
        return AutoModeHints::default();
    };
    let mut hints = AutoModeHints {
        prompt: Some(auto_mode::bound(current)),
        ..AutoModeHints::default()
    };
    if !auto_mode::is_terse(current) {
        return hints;
    }
    hints.prior_prompt = messages
        .iter()
        .rev()
        .find_map(typed_prompt)
        .map(auto_mode::bound);
    hints.prior_reply = messages
        .iter()
        .rev()
        .find_map(assistant_text)
        .map(|text| auto_mode::bound(&text));
    hints
}

/// The text of a request that a human typed. Compaction summaries and
/// app-injected notices are model- or app-written, so they are not requests.
pub(super) fn typed_prompt(message: &Message) -> Option<&str> {
    if message.role != Role::User || message.kind != lofi_types::PromptKind::User {
        return None;
    }
    let text = message.blocks.iter().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.as_str()),
        _ => None,
    })?;
    (!text.starts_with(crate::compact::HANDOFF_PREAMBLE)).then_some(text)
}

/// The last text the agent produced, which carries the question a terse
/// request answers.
fn assistant_text(message: &Message) -> Option<String> {
    if message.role != Role::Assistant {
        return None;
    }
    message.blocks.iter().rev().find_map(|block| match block {
        ContentBlock::Text { text } if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

/// Collect the answer and the usage the evaluation spent, so the cost and
/// the prompt-cache hit rate stay observable.
async fn collect_text(
    mut stream: futures::stream::BoxStream<'static, Result<lofi_types::StreamingEvent>>,
) -> Result<(String, lofi_types::Usage)> {
    let mut text = String::new();
    let mut usage = lofi_types::Usage::default();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(lofi_types::StreamingEvent::TextDelta(d)) => text.push_str(&d),
            Ok(lofi_types::StreamingEvent::Done { usage: spent, .. }) => {
                usage = spent;
                break;
            }
            Ok(lofi_types::StreamingEvent::Error(msg)) => {
                return Err(Error::Provider(msg));
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok((text, usage))
}
