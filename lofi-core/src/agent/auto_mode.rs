//! Auto-mode: builds an [`AutoModeFn`] that consults an LLM to pre-approve
//! shell-policy `ask` commands.
//!
//! When auto-mode is enabled in the shell policy config, a command that the
//! policy engine classifies as `ask` is first sent to a configured model with
//! a safety-evaluation prompt. If the model returns `{"decision":"allow"}` the
//! command runs without prompting the user. Any other response, a timeout, or
//! a failure falls back to the normal confirmation flow.
//!
//! The callback holds an [`Arc<dyn Provider>`] and a [`Model`] resolved from
//! the config at agent build time, so each `ask` command triggers a fresh
//! single-turn stream — no session, no tools, no thinking.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use lofi_code::policy::auto_mode;
use lofi_code::{AutoModeFn, AutoModeOutcome};
use lofi_providers::{open, Provider};
use lofi_types::{AutoModeConfig, Config, ContentBlock, Message, Model, Role};

use lofi_error::{Error, Result};

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

/// Build an [`AutoModeFn`] from the shell-policy auto-mode config, or return
/// `None` when auto-mode is disabled or the configured model is unavailable.
///
/// The provider is opened from `config.providers` using the auto-mode
/// config's `provider` key, and the model is resolved from the registry.
/// Both are captured in the returned closure so each invocation is a
/// self-contained LLM round-trip.
///
/// # Errors
/// Returns [`Error::Config`] when the auto-mode config names a provider or
/// model that does not exist or is not available (no credentials).
pub fn build_auto_mode(
    config: &Config,
    registry: &crate::models::ModelRegistry,
    auto_cfg: &AutoModeConfig,
    cwd: &std::path::Path,
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

    // Verify the model's provider has credentials.
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
    let timeout = Duration::from_millis(auto_cfg.timeout_ms);
    let max_tokens = auto_cfg.max_tokens.or(model.max_tokens);
    let cwd = cwd.to_path_buf();

    let auto_mode_fn: AutoModeFn = Arc::new(move |command: String| {
        let provider = provider.clone();
        let model = model.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            // The provider stream future is !Sync, so the entire evaluation
            // runs inside a spawned task whose JoinHandle is Send + Sync.
            let handle = tokio::task::spawn(async move {
                evaluate_command(&provider, &model, &command, &cwd, max_tokens).await
            });
            let mut handle = handle;
            let mut abort_on_drop = AbortOnDrop(Some(handle.abort_handle()));
            let outcome = match tokio::time::timeout(timeout, &mut handle).await {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(e)) => AutoModeOutcome::Failed {
                    reason: format!("evaluation task failed: {e}"),
                },
                Err(_) => {
                    handle.abort();
                    AutoModeOutcome::Failed {
                        reason: format!("evaluation timed out after {}s", timeout.as_secs()),
                    }
                }
            };
            abort_on_drop.disarm();
            outcome
        })
            as std::pin::Pin<Box<dyn std::future::Future<Output = AutoModeOutcome> + Send + Sync>>
    });

    Ok(Some(auto_mode_fn))
}

/// Run a single-turn LLM evaluation of a command.
///
/// Return the evaluator's decision with a user-facing reason. Provider,
/// stream, and parsing failures remain distinct so the confirmation dialog can
/// explain why automatic approval did not complete.
async fn evaluate_command(
    provider: &Arc<dyn Provider>,
    model: &Model,
    command: &str,
    cwd: &std::path::Path,
    max_tokens: Option<u64>,
) -> AutoModeOutcome {
    let prompt = auto_mode::build_prompt(command, &cwd.display().to_string());

    let mut model = model.clone();
    // Disable thinking for the evaluation: we want a fast, cheap response.
    model.thinking = lofi_types::ThinkingLevel::Off;
    if let Some(mt) = max_tokens {
        model.max_tokens = Some(mt);
    }

    let messages = vec![Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text { text: prompt }],
    }];

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
        Ok(text) => {
            let decision = auto_mode::parse_decision(&text);
            if let Some(d) = decision {
                if d.allow {
                    tracing::debug!(
                        command = command,
                        reason = d.reason.as_str(),
                        "auto-mode: approved"
                    );
                    AutoModeOutcome::Allow { reason: d.reason }
                } else {
                    tracing::debug!(
                        command = command,
                        reason = d.reason.as_str(),
                        "auto-mode: deferred to confirmation"
                    );
                    AutoModeOutcome::Ask { reason: d.reason }
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

/// Collect all text deltas from a provider stream into a single string.
async fn collect_text(
    mut stream: futures::stream::BoxStream<'static, Result<lofi_types::StreamingEvent>>,
) -> Result<String> {
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(lofi_types::StreamingEvent::TextDelta(d)) => text.push_str(&d),
            Ok(lofi_types::StreamingEvent::Done(_)) => break,
            Ok(lofi_types::StreamingEvent::Error(msg)) => {
                return Err(Error::Provider(msg));
            }
            Ok(_) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(text)
}
