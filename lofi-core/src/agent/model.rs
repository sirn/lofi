#![allow(clippy::wildcard_imports)]

use super::*;

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

/// Walking stops at the repository boundary so unrelated ancestor
/// instructions cannot affect the workspace.
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

    let skills_block = skills_section(config_dir, root);

    if sections.is_empty() && skills_block.is_none() {
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
    if let Some(block) = skills_block {
        out.push_str("\n\n");
        out.push_str(&block);
    }
    out
}

/// Build the `<skills>` index the model reads with `lofi.skill(name)`, listing
/// each available skill's name and one-line description. Returns `None` when
/// no skills are installed, so the prompt never carries an empty index.
fn skills_section(config_dir: Option<&std::path::Path>, root: &std::path::Path) -> Option<String> {
    let skills_dir = config_dir.map(|d| d.join("skills"));
    let summaries =
        lofi_code::tools::skills::scan_skill_summaries(root, skills_dir.as_deref()).ok()?;
    if summaries.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<skills>\n  <instruction>\n    Load a skill with `lofi.skill(name)` before following it.\n  </instruction>\n",
    );
    for s in summaries {
        out.push_str("  <skill>\n    <name>");
        out.push_str(&s.name);
        out.push_str("</name>\n    <description>");
        out.push_str(&s.description);
        out.push_str("</description>\n    <location>");
        out.push_str(&s.location);
        out.push_str("</location>\n  </skill>\n");
    }
    out.push_str("</skills>");
    Some(out)
}

fn read_agents_md(path: &std::path::Path) -> Option<String> {
    let body = std::fs::read_to_string(path).ok()?;
    (!body.trim().is_empty()).then_some(body)
}

/// Collect `(origin, body)` pairs for every `AGENTS.md` from `root` up to
/// the enclosing project root (inclusive), ordered outermost-first. When
/// `root` is not inside a recognized project only `root`'s own `AGENTS.md`
/// is considered, so the walk never escapes into unrelated ancestors.
fn dir_agents_md(root: &std::path::Path) -> Vec<(String, String)> {
    let boundary = project_boundary(root).unwrap_or_else(|| root.to_path_buf());
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

const PROJECT_ROOT_MARKERS: &[&str] = &[
    // Version-control metadata. `.git` may be either a directory or a file
    // in linked worktrees, so marker detection deliberately uses `exists`.
    ".git",
    ".jj",
    ".hg",
    ".svn",
    // High-signal language and build manifests.
    "Cargo.toml",
    "go.mod",
    "go.work",
    "package.json",
    "deno.json",
    "deno.jsonc",
    "pyproject.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "Gemfile",
    "composer.json",
    "mix.exs",
    "pubspec.yaml",
    "Package.swift",
    "CMakeLists.txt",
];

fn is_project_root(path: &std::path::Path) -> bool {
    PROJECT_ROOT_MARKERS
        .iter()
        .any(|marker| path.join(marker).exists())
}

fn project_boundary(start: &std::path::Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|path| is_project_root(path))
        .map(std::path::Path::to_path_buf)
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
    model_obj.thinking = level.clone();

    let provider_cfg = config
        .providers
        .get(&model_obj.provider)
        .ok_or_else(|| Error::Config(format!("provider not found: {}", model_obj.provider)))?;
    let provider = open(model_obj.api, provider_cfg)?;

    let agent = if let Some(a) = existing {
        a.with_model(provider, model_obj.clone())
    } else {
        let tmp_lease = state::create_session_tmp_dir(root)?;
        Agent::new(
            provider,
            model_obj.clone(),
            root.to_path_buf(),
            tmp_lease.path().to_path_buf(),
            SYSTEM_PROMPT.to_string(),
            None,
            config.compaction.reserved_context_tokens,
            &config.bash,
            config.truncate,
            config.image,
            &config.shell_policy,
        )
        .with_tmp_lease(tmp_lease)
        .with_retry(crate::retry::RetryPolicy::from(config.retry))
    };
    Ok((agent, model_obj, level))
}

pub(crate) struct ModelQuery {
    provider: String,
    model: String,
    level: Option<ThinkingLevel>,
    tier: Option<ServiceTier>,
}

/// The provider qualifier is mandatory: bare ids are rejected so a prompt
/// always names the endpoint it runs against. `level` is an optional
/// `:off`/`:low`/`:medium`/`:high`/`:xhigh` suffix; an unrecognized suffix is
/// an error rather than silently ignored.
pub(crate) fn parse_model_query(query: &str) -> Result<ModelQuery> {
    // Optional trailing `@tier` (e.g. `provider/id:high@flex`). Split it
    // before the thinking level so a level suffix is never confused with a
    // tier. An unrecognized suffix is an error rather than silently ignored,
    // so a typo never quietly sends the provider default.
    let (qual, tier) = match query.rsplit_once('@') {
        Some((head, tail)) if !tail.is_empty() => match ServiceTier::parse(tail) {
            Some(t) => (head, Some(t)),
            None => {
                return Err(Error::Config(format!(
                    "unknown service tier `{tail}` in `{query}` (expected auto|flex|priority)"
                )));
            }
        },
        _ => (query, None),
    };
    let (qual, level) = match qual.rsplit_once(':') {
        Some((head, tail)) if !tail.is_empty() => match ThinkingLevel::parse(tail) {
            Some(l) => (head, Some(l)),
            None => {
                return Err(Error::Config(format!(
                    "unknown thinking level `{tail}` in `{query}` (expected off|low|medium|high|xhigh)"
                )));
            }
        },
        _ => (qual, None),
    };
    let (provider, model) = qual.split_once('/').ok_or_else(|| {
        Error::Config(format!(
            "model `{query}` must be qualified as `provider/model[:level][@tier]`"
        ))
    })?;
    if provider.is_empty() || model.is_empty() {
        return Err(Error::Config(format!("malformed model query `{query}`")));
    }
    Ok(ModelQuery {
        provider: provider.to_string(),
        model: model.to_string(),
        level,
        tier,
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
    let (provider_name, model_id, explicit_level, explicit_tier) = if let Some(q) = model_query {
        let mq = parse_model_query(q)?;
        (mq.provider, mq.model, mq.level, mq.tier)
    } else if let Some(default) = config.default_model.as_deref() {
        let mq = parse_model_query(default)?;
        (mq.provider, mq.model, mq.level, mq.tier)
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
        (m.provider.clone(), m.id.clone(), None, None)
    } else {
        let m = available
            .first()
            .ok_or_else(|| Error::NoModels(NO_MODELS_HINT.to_string()))?;
        (m.provider.clone(), m.id.clone(), None, None)
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
    let level = resolve_thinking_level(
        explicit_level,
        mc,
        pcfg,
        config.agent.thinking_level.clone(),
    )?;
    let tier = resolve_service_tier(explicit_tier, mc, pcfg, config.agent.service_tier.clone())?;
    let mut model = model;
    model.service_tier = tier.clone();
    Ok((model, level))
}

pub(crate) fn resolve_thinking_level(
    explicit: Option<ThinkingLevel>,
    mc: &lofi_types::ModelConfig,
    pcfg: &lofi_types::ProviderConfig,
    agent: Option<ThinkingLevel>,
) -> Result<ThinkingLevel> {
    let was_explicit = explicit.is_some();
    let desired = explicit
        .or_else(|| mc.thinking_level.clone())
        .or_else(|| pcfg.thinking_level.clone())
        .or(agent)
        .unwrap_or(ThinkingLevel::Medium);
    if desired == ThinkingLevel::Off {
        return Ok(ThinkingLevel::Off);
    }
    if mc.thinking_levels.is_empty() {
        if was_explicit {
            return Err(Error::Config(format!(
                "model does not support thinking (no thinking_levels declared); cannot use `{}`",
                desired.as_str()
            )));
        }
        return Ok(ThinkingLevel::Off);
    }
    if !mc.thinking_levels.contains(&desired) {
        let allowed: Vec<&str> = mc
            .thinking_levels
            .iter()
            .map(ThinkingLevel::as_str)
            .collect();
        return Err(Error::Config(format!(
            "thinking level `{}` not supported by this model; allowed: {}",
            desired.as_str(),
            allowed.join(", ")
        )));
    }
    Ok(desired)
}

pub(crate) fn resolve_service_tier(
    explicit: Option<ServiceTier>,
    mc: &lofi_types::ModelConfig,
    pcfg: &lofi_types::ProviderConfig,
    agent: Option<ServiceTier>,
) -> Result<ServiceTier> {
    let was_explicit = explicit.is_some();
    let desired = explicit
        .or_else(|| mc.service_tier.clone())
        .or_else(|| pcfg.service_tier.clone())
        .or(agent)
        .unwrap_or(ServiceTier::Auto);
    if desired == ServiceTier::Auto {
        return Ok(ServiceTier::Auto);
    }
    if mc.service_tiers.is_empty() {
        // Like thinking levels, an undeclared list means the model declares
        // no service tiers, so an explicit non-auto tier is rejected rather
        // than silently forwarded to a provider that may not understand it.
        if was_explicit {
            return Err(Error::Config(format!(
                "model does not support service tiers (no service_tiers declared); cannot use `{}`",
                desired.as_str()
            )));
        }
        return Ok(ServiceTier::Auto);
    }
    if !mc.service_tiers.contains(&desired) {
        let allowed: Vec<&str> = mc.service_tiers.iter().map(ServiceTier::as_str).collect();
        return Err(Error::Config(format!(
            "service tier `{}` not supported by this model; allowed: {}",
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
    fn skills_index_appended_with_name_and_description() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "").unwrap();
        let cfg = tmp.path().join("config");
        let skill_dir = cfg.join("skills").join("git-workflow");
        fs::create_dir_all(&skill_dir).unwrap();
        write(
            &skill_dir.join("SKILL.md"),
            "# Git Workflow\n\nStandard branching workflow.\n",
        );
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        assert!(prompt.starts_with(SYSTEM_PROMPT));
        assert!(prompt.contains("<skills>"));
        assert!(prompt.contains("<instruction>"));
        assert!(prompt.contains("</instruction>"));
        assert!(prompt.contains("<name>git-workflow</name>"));
        assert!(prompt.contains("<description>Standard branching workflow.</description>"));
        assert!(prompt.contains("<location>"));
        assert!(prompt.contains("lofi.skill"));
    }

    #[test]
    fn no_skills_means_no_skills_block() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("proj");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".git"), "").unwrap();
        let cfg = tmp.path().join("config");
        fs::create_dir_all(&cfg).unwrap();
        let prompt = assemble_system_prompt(Some(&cfg), &root);
        // The base prompt references a `<skills>` index in prose; assert the
        // actual index block (which opens on its own line) is absent.
        assert!(!prompt.contains("\n<skills>"));
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
    fn recognizes_common_project_root_markers() {
        let tmp = TempDir::new().unwrap();
        let plain = tmp.path().join("plain");
        fs::create_dir_all(&plain).unwrap();
        assert!(!is_project_root(&plain));

        for marker in PROJECT_ROOT_MARKERS {
            let candidate = tmp.path().join(marker.replace('.', "_"));
            fs::create_dir_all(&candidate).unwrap();
            write(&candidate.join(marker), "marker");
            assert!(is_project_root(&candidate), "did not recognize {marker}");
        }
    }

    #[test]
    fn walk_stops_at_manifest_project_root() {
        let tmp = TempDir::new().unwrap();
        let project = tmp.path().join("project");
        let root = project.join("src");
        fs::create_dir_all(&root).unwrap();
        write(
            &project.join("pyproject.toml"),
            "[project]\nname = \"demo\"\n",
        );
        write(&tmp.path().join("AGENTS.md"), "OUTSIDE-LEAK\n");
        write(&project.join("AGENTS.md"), "project rules\n");
        write(&root.join("AGENTS.md"), "source rules\n");

        let prompt = assemble_system_prompt(Some(&tmp.path().join("config")), &root);

        assert!(prompt.contains("project rules"));
        assert!(prompt.contains("source rules"));
        assert!(!prompt.contains("OUTSIDE-LEAK"));
        assert!(prompt.find("project rules").unwrap() < prompt.find("source rules").unwrap());
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
    fn walk_stops_at_jj_boundary() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let root = repo.join("sub");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(repo.join(".jj")).unwrap();
        write(&tmp.path().join("AGENTS.md"), "OUTSIDE-LEAK\n");
        write(&repo.join("AGENTS.md"), "jj-repo rules\n");
        write(&root.join("AGENTS.md"), "sub rules\n");

        let prompt = assemble_system_prompt(Some(&tmp.path().join("config")), &root);

        assert!(prompt.contains("jj-repo rules"));
        assert!(prompt.contains("sub rules"));
        assert!(!prompt.contains("OUTSIDE-LEAK"));
        assert!(prompt.find("jj-repo rules").unwrap() < prompt.find("sub rules").unwrap());
    }

    #[test]
    fn unrecognized_project_uses_only_root() {
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
