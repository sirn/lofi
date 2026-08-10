use futures::StreamExt;
use lofi_code::policy::auto_mode;
use lofi_code::{AutoModeFn, AutoModeOutcome};
use lofi_providers::{open, Provider};
use lofi_types::{AutoModeConfig, Config, ContentBlock, Message, Model, Role};
use std::sync::Arc;

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

    let auto_mode_fn: AutoModeFn = Arc::new(move |command: String| {
        let provider = provider.clone();
        let model = model.clone();
        let cwd = cwd.clone();
        Box::pin(async move {
            let handle = tokio::task::spawn(async move {
                evaluate_command(&provider, &model, &command, &cwd, max_tokens).await
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

async fn evaluate_command(
    provider: &Arc<dyn Provider>,
    model: &Model,
    command: &str,
    cwd: &std::path::Path,
    max_tokens: Option<u64>,
) -> AutoModeOutcome {
    let prompt = auto_mode::build_prompt(command, &cwd.display().to_string());

    let mut model = model.clone();
    model.thinking = lofi_types::ThinkingLevel::Off;
    if let Some(mt) = max_tokens {
        model.max_tokens = Some(mt);
    }

    let messages = vec![Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text { text: prompt }],
        kind: lofi_types::PromptKind::User,
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

async fn collect_text(
    mut stream: futures::stream::BoxStream<'static, Result<lofi_types::StreamingEvent>>,
) -> Result<String> {
    let mut text = String::new();
    loop {
        let ev = match tokio::time::timeout(super::DEFAULT_STREAM_IDLE_TIMEOUT, stream.next()).await
        {
            Ok(Some(ev)) => ev,
            Ok(None) => break,
            Err(_) => return Err(Error::Provider("stream idle timeout".into())),
        };
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
