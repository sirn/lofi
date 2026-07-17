#![allow(clippy::wildcard_imports)]

use super::*;

/// Build the initial message history (system + user).
pub(crate) fn initial_history(system: &str, user_prompt: &str) -> Vec<Message> {
    let mut messages = Vec::with_capacity(2);
    if !system.is_empty() {
        messages.push(Message {
            role: Role::System,
            blocks: vec![ContentBlock::Text {
                text: system.to_string(),
            }],
        });
    }
    messages.push(Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: user_prompt.to_string(),
        }],
    });
    messages
}

/// Build an [`Agent`] and its selected [`Model`] from the user config and
/// an optional `--model provider/model[:level]` query.
///
/// Shared by the `lofi-ui` presentation drivers (`run_print`, `run_interactive`)
/// so the resolution ladder (config load, registry + discovery, model +
/// thinking-level resolution, transport construction) stays in one place.
/// With no `--model`, the first available model (config order) is selected; if
/// no provider has credentials (or the selected provider is disabled),
/// [`Error::NoModels`] is returned so the interactive UI can launch and show
/// a friendly message. Remote discovery failures
/// fall back to static-only [`ModelRegistry::load`].
///
/// # Errors
/// Propagates [`Error`] from config load, model resolution, thinking-level
/// validation, or provider construction.
pub async fn build_agent(
    config_path: Option<&std::path::Path>,
    model: Option<&str>,
    root: &std::path::Path,
) -> Result<(Agent, Model, ThinkingLevel, lofi_types::Config)> {
    let config_path = match config_path {
        Some(p) => p.to_path_buf(),
        None => crate::config_loader::user_config_path()?,
    };
    let config = load_config_or_default(&config_path).await?;

    let registry = match ModelRegistry::load_async(&config).await {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(error = %e, "async model load failed; falling back to static");
            ModelRegistry::load(&config)?
        }
    };

    let (mut model_obj, level) = select_model(&registry, &config, model)?;
    // The effective thinking level is resolved here, not in the registry, so
    // the same cached registry serves runs at different levels.
    model_obj.thinking = level;

    let provider_cfg = config
        .providers
        .get(&model_obj.provider)
        .ok_or_else(|| Error::Config(format!("provider not found: {}", model_obj.provider)))?;
    let provider = open(model_obj.api, provider_cfg)?;

    let agent = Agent::new(
        provider,
        model_obj.clone(),
        root.to_path_buf(),
        state::create_session_tmp_dir()?,
        SYSTEM_PROMPT.to_string(),
        None,
        config.compaction.reserved_context_tokens,
        &config.bash,
    )
    .with_retry(crate::retry::RetryPolicy::from(config.retry));
    Ok((agent, model_obj, level, config))
}

/// A parsed --model query: provider/model[:level].
pub(crate) struct ModelQuery {
    provider: String,
    model: String,
    level: Option<ThinkingLevel>,
}

/// Parse a `--model` argument of the form `provider/model[:level]`.
///
/// The provider qualifier is mandatory: bare ids are rejected so a prompt
/// always names the endpoint it runs against. `level` is an optional
/// `:off`/`:low`/`:medium`/`:high`/`:xhigh` suffix; an unrecognized suffix is
/// an error rather than silently ignored.
pub(crate) fn parse_model_query(query: &str) -> Result<ModelQuery> {
    // Split off a trailing :level only when it parses as a level, so a model
    // id that happens to contain : is not misread.
    let (qual, level) = match query.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() => match ThinkingLevel::parse(tail) {
            Some(l) => (head, Some(l)),
            None => {
                return Err(Error::Config(format!(
                    "unknown thinking level `{tail}` in `{query}` (expected off|low|medium|high|xhigh)"
                )));
            }
        },
        _ => (query, None),
    };
    let (provider, model) = qual.split_once('/').ok_or_else(|| {
        Error::Config(format!(
            "model `{query}` must be qualified as `provider/model[:level]`"
        ))
    })?;
    if provider.is_empty() || model.is_empty() {
        return Err(Error::Config(format!("malformed model query `{query}`")));
    }
    Ok(ModelQuery {
        provider: provider.to_string(),
        model: model.to_string(),
        level,
    })
}

pub(crate) const NO_MODELS_HINT: &str = "No models configured.";

/// Pick the model to run against and resolve its thinking level.
///
/// With a `model_query` of `provider/model[:level]`, the named model is resolved
/// within the named provider and must be available (its provider
/// authenticated or `no_auth`). Without a query, the first available model in
/// registry (config) order is chosen. The thinking level resolves from the
/// CLI `:level`, then the model/provider/agent defaults, then `medium`; it must
/// be `off` or one of the model's declared `thinking_levels`.
///
/// # Errors
/// Returns [`Error::NoModels`] when no model is available, or when the named
/// model's provider is disabled (no credentials); [`Error::Config`] for an
/// unresolvable query, an unknown model/provider, or an unsupported thinking
/// level.
pub fn select_model(
    registry: &ModelRegistry,
    config: &lofi_types::Config,
    model_query: Option<&str>,
) -> Result<(Model, ThinkingLevel)> {
    let available = registry.available();
    let (provider_name, model_id, explicit_level) = if let Some(q) = model_query {
        let mq = parse_model_query(q)?;
        (mq.provider, mq.model, mq.level)
    } else if let Some(default) = config.default_model.as_deref() {
        // `default_model` wins: parse it as a `provider/model[:level]` query
        // so a bare id is rejected the same way an explicit `--model` is.
        let mq = parse_model_query(default)?;
        (mq.provider, mq.model, mq.level)
    } else if let Some(provider) = config.default_provider.as_deref() {
        // `default_provider` selects that provider's first available model.
        let m = available
            .iter()
            .find(|a| a.provider == provider)
            .ok_or_else(|| {
                Error::Config(format!(
                    "default_provider `{provider}` has no available model; available:\n{}",
                    registry.list_models_print()
                ))
            })?;
        (m.provider.clone(), m.id.clone(), None)
    } else {
        let m = available
            .first()
            .ok_or_else(|| Error::NoModels(NO_MODELS_HINT.to_string()))?;
        (m.provider.clone(), m.id.clone(), None)
    };

    let model = registry
        .resolve(&format!("{provider_name}/{model_id}"))
        .ok_or_else(|| {
            Error::Config(format!(
                "no model `{model_id}` for provider `{provider_name}`; available:\n{}",
                registry.list_models_print()
            ))
        })?
        .clone();
    // Require the resolved model's provider to be available so a keyless
    // provider's model is never silently selected and then fails at request
    // time.
    if !available
        .iter()
        .any(|a| a.id == model.id && a.provider == model.provider)
    {
        return Err(Error::NoModels(format!(
            "model `{provider_name}/{model_id}` is not available: its provider has no API key set. \
             Set its env var (see the provider's `env_name`) or configure credentials in the config file."
        )));
    }

    // Use the registry's (possibly augmented) providers so auto-discovered
    // models — which load_async injects only into the registry's copy — are
    // resolvable here for thinking-level lookup. `config.agent` is unaffected
    // by discovery and supplies the agent-level default.
    let pcfg = registry
        .providers()
        .get(&provider_name)
        .ok_or_else(|| Error::Config(format!("unknown provider: {provider_name}")))?;
    let mc = pcfg
        .models
        .get(&model_id)
        .ok_or_else(|| Error::Config(format!("no model `{model_id}` for provider `{provider_name}`")))?;
    let level = resolve_thinking_level(explicit_level, mc, pcfg, config.agent.thinking_level)?;
    Ok((model, level))
}

/// Resolve the effective thinking level for a run.
///
/// Precedence: explicit CLI `:level`, then model default, then provider
/// default, then agent default, then `medium`. `off` is always allowed. A
/// non-`off` level must appear in the model's declared `thinking_levels`; if
/// the model declares none it does not support thinking and a non-`off`
/// explicit request is an error (an implicit default is silently clamped to
/// `off`).
pub(crate) fn resolve_thinking_level(
    explicit: Option<ThinkingLevel>,
    mc: &lofi_types::ModelConfig,
    pcfg: &lofi_types::ProviderConfig,
    agent: Option<ThinkingLevel>,
) -> Result<ThinkingLevel> {
    let desired = explicit
        .or(mc.thinking_level)
        .or(pcfg.thinking_level)
        .or(agent)
        .unwrap_or(ThinkingLevel::Medium);
    if desired == ThinkingLevel::Off {
        return Ok(ThinkingLevel::Off);
    }
    if mc.thinking_levels.is_empty() {
        if explicit.is_some() {
            return Err(Error::Config(format!(
                "model does not support thinking (no thinking_levels declared); cannot use `{}`",
                desired.as_str()
            )));
        }
        return Ok(ThinkingLevel::Off);
    }
    if !mc.thinking_levels.contains(&desired) {
        let allowed: Vec<&str> = mc.thinking_levels.iter().map(|l| l.as_str()).collect();
        return Err(Error::Config(format!(
            "thinking level `{}` not supported by this model; allowed: {}",
            desired.as_str(),
            allowed.join(", ")
        )));
    }
    Ok(desired)
}