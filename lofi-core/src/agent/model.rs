#![allow(clippy::wildcard_imports)]

use super::*;

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

/// Shared by the `lofi-ui` presentation drivers (`run_print`, `run_interactive`)
/// so the resolution ladder (config load, registry + discovery, model +
/// thinking-level resolution, transport construction) stays in one place.
/// With no `--model`, the first available model (config order) is selected; if
/// no provider has credentials (or the selected provider is disabled),
/// [`Error::NoModels`] is returned so the interactive UI can launch and show
/// a friendly message. Remote discovery failures
/// fall back to static-only [`ModelRegistry::load`].
/// # Errors
/// Propagates [`Error`] from config load, model resolution, thinking-level
/// validation, or provider construction.
pub async fn build_agent(
    config_path: Option<&std::path::Path>,
    model: Option<&str>,
    root: &std::path::Path,
) -> Result<(
    Agent,
    Model,
    ThinkingLevel,
    lofi_types::Config,
    ModelRegistry,
)> {
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

    let (agent, model_obj, level) = rebuild_agent(None, &registry, &config, model, root)?;
    let agent = agent.with_system_prompt(assemble_system_prompt(config_path.parent(), root));
    let skills_dir = config_path.parent().map(|p| p.join("skills"));
    let agent = agent.with_skills_dir(skills_dir);
    let agent = if let Some(auto_cfg) = config.shell_policy.auto_mode.as_ref() {
        match super::auto_mode::build_auto_mode(&config, &registry, auto_cfg, root) {
            Ok(Some(fn_)) => agent.with_auto_mode(fn_),
            Ok(None) => agent,
            Err(e) => {
                tracing::warn!(error = %e, "auto-mode: disabled due to configuration error");
                agent
            }
        }
    } else {
        agent
    };
    Ok((agent, model_obj, level, config, registry))
}

/// Two sources, ordered least to most specific so the most specific file is
/// last and most prominent:
/// - **Global** — `<config_dir>/AGENTS.md`, user-wide instructions kept next
///   to the config file. Skipped when `config_dir` is `None`.
/// - **Per-directory** — every `AGENTS.md` found walking from the workspace
///   `root` up to the enclosing git repo root (inclusive). Walking stops at
///   the repo boundary so unrelated ancestor directories never contribute;
///   when `root` is not inside a git repository only `root`'s own
///   `AGENTS.md` is considered. Files are ordered outermost-first.
/// Missing or whitespace-only files are skipped. When none are found the base
/// [`SYSTEM_PROMPT`] is returned unchanged. Otherwise each found file is
/// appended as an `agents_md` XML block (with a `source` attribute naming
/// its origin) after the unwrapped base prompt.
fn assemble_system_prompt(config_dir: Option<&std::path::Path>, root: &std::path::Path) -> String {
    let mut sections: Vec<(String, String)> = Vec::new();

    if let Some(dir) = config_dir {
        if let Some(body) = read_agents_md(&dir.join("AGENTS.md")) {
            sections.push(("global".to_string(), body));
        }
    }

    for (dir, body) in dir_agents_md(root) {
        sections.push((dir, body));
    }

    if sections.is_empty() {
        return SYSTEM_PROMPT.to_string();
    }
    let mut out = String::from(SYSTEM_PROMPT);
    for (origin, body) in sections {
        out.push_str("\n\n<agents_md source=\"");
        out.push_str(&origin);
        out.push_str("\">\n");
        out.push_str(body.trim());
        out.push_str("\n</agents_md>");
    }
    out
}

fn read_agents_md(path: &std::path::Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    (!body.trim().is_empty()).then_some(body)
}

/// Collect `(origin, body)` pairs for every `AGENTS.md` from `root` up to
/// the enclosing git repo root (inclusive), ordered outermost-first. When
/// `root` is not inside a git repository only `root`'s own `AGENTS.md` is
/// considered, so the walk never escapes into unrelated ancestor directories.
fn dir_agents_md(root: &std::path::Path) -> Vec<(String, String)> {
    let boundary = git_boundary(root).unwrap_or_else(|| root.to_path_buf());
    let mut found: Vec<(String, String)> = Vec::new();
    let mut cur = Some(root);
    while let Some(d) = cur {
        if let Some(body) = read_agents_md(&d.join("AGENTS.md")) {
            found.push((d.display().to_string(), body));
        }
        if d == boundary.as_path() {
            break;
        }
        cur = d.parent();
    }
    found.reverse();
    found
}

fn git_boundary(start: &std::path::Path) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(d) = cur {
        if d.join(".git").exists() {
            return Some(d.to_path_buf());
        }
        cur = d.parent();
    }
    None
}

/// The startup path passes `None` and gets a fresh agent (a new tmp dir).
/// The `/model` selector passes the live agent so the switch doesn't orphan
/// `lofi.bash` full-output logs or `lofi.bash_read` state. This is sync and
/// side-effect-free beyond provider construction, so a switch never blocks
/// the UI on remote discovery — the registry is retained from startup.
/// # Errors
/// Propagates [`Error`] from model resolution, thinking-level validation,
/// or provider construction.
pub fn rebuild_agent(
    existing: Option<&Agent>,
    registry: &ModelRegistry,
    config: &lofi_types::Config,
    model: Option<&str>,
    root: &std::path::Path,
) -> Result<(Agent, Model, ThinkingLevel)> {
    let (mut model_obj, level) = select_model(registry, config, model)?;
    model_obj.thinking = level;

    let provider_cfg = config
        .providers
        .get(&model_obj.provider)
        .ok_or_else(|| Error::Config(format!("provider not found: {}", model_obj.provider)))?;
    let provider = open(model_obj.api, provider_cfg)?;

    let agent = if let Some(a) = existing {
        a.with_model(provider, model_obj.clone())
    } else {
        Agent::new(
            provider,
            model_obj.clone(),
            root.to_path_buf(),
            state::create_session_tmp_dir()?,
            SYSTEM_PROMPT.to_string(),
            None,
            config.compaction.reserved_context_tokens,
            &config.bash,
            &config.shell_policy,
        )
        .with_retry(crate::retry::RetryPolicy::from(config.retry))
    };
    Ok((agent, model_obj, level))
}

pub(crate) struct ModelQuery {
    provider: String,
    model: String,
    level: Option<ThinkingLevel>,
}

/// The provider qualifier is mandatory: bare ids are rejected so a prompt
/// always names the endpoint it runs against. `level` is an optional
/// `:off`/`:low`/`:medium`/`:high`/`:xhigh` suffix; an unrecognized suffix is
/// an error rather than silently ignored.
pub(crate) fn parse_model_query(query: &str) -> Result<ModelQuery> {
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
        let mq = parse_model_query(default)?;
        (mq.provider, mq.model, mq.level)
    } else if let Some(provider) = config.default_provider.as_deref() {
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

    let pcfg = registry
        .providers()
        .get(&provider_name)
        .ok_or_else(|| Error::Config(format!("unknown provider: {provider_name}")))?;
    let mc = pcfg.models.get(&model_id).ok_or_else(|| {
        Error::Config(format!(
            "no model `{model_id}` for provider `{provider_name}`"
        ))
    })?;
    let level = resolve_thinking_level(explicit_level, mc, pcfg, config.agent.thinking_level)?;
    Ok((model, level))
}

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, body: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    fn no_agents_md_returns_base_prompt() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "").unwrap();
        let cfg = tmp.path().join("config");
        fs::create_dir_all(&cfg).unwrap();
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        assert_eq!(prompt, SYSTEM_PROMPT);
    }

    #[test]
    fn global_agents_md_appended() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "").unwrap();
        let cfg = tmp.path().join("config");
        write(&cfg.join("AGENTS.md"), "Be terse.\n");
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        assert!(prompt.starts_with(SYSTEM_PROMPT));
        assert_eq!(prompt.matches(r#"<agents_md source="global">"#).count(), 1);
        assert_eq!(prompt.matches("</agents_md>").count(), 1);
        assert!(prompt.contains("Be terse."));
    }

    #[test]
    fn per_dir_agents_md_ordered_outermost_first() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let root = repo.join("sub");
        fs::create_dir_all(&root).unwrap();
        fs::write(repo.join(".git"), "").unwrap();
        write(&repo.join("AGENTS.md"), "repo-level rules\n");
        write(&root.join("AGENTS.md"), "sub-level rules\n");
        let cfg = tmp.path().join("config");
        fs::create_dir_all(&cfg).unwrap();
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        let repo_pos = prompt.find("repo-level").unwrap();
        let sub_pos = prompt.find("sub-level").unwrap();
        assert!(
            repo_pos < sub_pos,
            "outermost (repo) must precede innermost (sub)"
        );
    }

    #[test]
    fn walk_stops_at_git_boundary() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let root = repo.join("sub");
        fs::create_dir_all(&root).unwrap();
        fs::write(repo.join(".git"), "").unwrap();
        // Outside the repo boundary — must not be picked up.
        write(&tmp.path().join("AGENTS.md"), "OUTSIDE-LEAK\n");
        write(&root.join("AGENTS.md"), "inside\n");
        let prompt = assemble_system_prompt(Some(&tmp.path().join("config")), &root);
        assert!(prompt.contains("inside"));
        assert!(!prompt.contains("OUTSIDE-LEAK"));
    }

    #[test]
    fn non_git_project_uses_only_root() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        write(&root.join("AGENTS.md"), "root-only\n");
        let cfg = tmp.path().join("config");
        fs::create_dir_all(&cfg).unwrap();
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        assert!(prompt.contains("root-only"));
        assert_eq!(prompt.matches("<agents_md").count(), 1);
        assert_eq!(prompt.matches("</agents_md>").count(), 1);
    }

    #[test]
    fn empty_agents_md_skipped() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "").unwrap();
        write(&root.join("AGENTS.md"), "   \n\n  \n");
        let cfg = tmp.path().join("config");
        fs::create_dir_all(&cfg).unwrap();
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        assert_eq!(prompt, SYSTEM_PROMPT);
    }
}
