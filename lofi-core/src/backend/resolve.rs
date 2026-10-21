use std::path::{Path, PathBuf};
use std::sync::Arc;

use lofi_error::{Error, Result};
use lofi_types::{Config, ModelChoice, ServiceTier, ThinkingLevel};

use super::contract::{BackendRef, ModelSelection};
use super::session::ExternalBackend;
use crate::agent::ToolExecutor;
use crate::models::ModelRegistry;

/// A model query split into the backend that runs it and the selection inside
/// that backend.
pub(crate) struct ParsedQuery {
    pub provider: String,
    pub model: String,
    pub level: Option<ThinkingLevel>,
    pub tier: ServiceTier,
}

pub(crate) fn parse_query(query: &str) -> Result<ParsedQuery> {
    let parsed = crate::agent::parse_model_query(query)?;
    Ok(ParsedQuery {
        provider: parsed.provider,
        model: parsed.model,
        level: parsed.level,
        tier: parsed.tier.unwrap_or(ServiceTier::Auto),
    })
}

/// Opens a model query as a backend, whichever transport it names.
///
/// The provider table and the backend table share one namespace: a query names
/// a provider or a backend and the factory picks the transport. Nothing above
/// this point knows which one it got.
///
/// Credentials stay lazy. Building the catalog reads no credential command, and
/// opening a provider resolves only that provider.
#[derive(Clone)]
pub struct BackendFactory {
    config: Arc<Config>,
    registry: ModelRegistry,
    root: PathBuf,
    env: Arc<Vec<(String, String)>>,
    skills_dir: Option<PathBuf>,
    persist_sessions: bool,
}

impl BackendFactory {
    /// Build the catalog from an unresolved config: a provider's own
    /// environment variable fills in its key for listing only, so the picker
    /// shows the models it would be able to open.
    ///
    /// # Errors
    /// Propagates static model registry errors.
    pub fn new(config: Config, root: &Path, env: Vec<(String, String)>) -> Result<Self> {
        let mut choices_config = config.clone();
        for provider in choices_config.providers.values_mut() {
            if provider.api_key.is_none() {
                provider.api_key = provider
                    .env_name
                    .as_ref()
                    .and_then(|name| std::env::var(name).ok());
            }
        }
        let registry = ModelRegistry::load_with_cache(&choices_config)?;
        Ok(Self {
            config: Arc::new(config),
            registry,
            root: root.to_path_buf(),
            env: Arc::new(env),
            skills_dir: None,
            persist_sessions: true,
        })
    }

    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    #[must_use]
    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// The skill directory the session reads, next to its config file.
    #[must_use]
    pub fn with_skills_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.skills_dir = dir;
        self
    }

    /// Whether subagent turns write transcripts. A session without a
    /// transcript turns them off for every backend it opens.
    #[must_use]
    pub fn with_subagent_persistence(mut self, persist: bool) -> Self {
        self.persist_sessions = persist;
        self
    }

    /// Every model of every transport, as the picker lists them.
    #[must_use]
    pub fn choices(&self) -> Vec<ModelChoice> {
        self.registry.choices()
    }

    /// The session-scoped state a backend opened by this factory starts with.
    ///
    /// # Errors
    /// Propagates session temporary directory creation.
    pub fn executor(&self) -> Result<ToolExecutor> {
        Ok(ToolExecutor::new(&self.root, &self.config, &self.env)?
            .with_subagent_support(self.support()))
    }

    /// What a subagent hook resolves model overrides with.
    #[must_use]
    pub fn support(&self) -> crate::SubagentSupport {
        crate::SubagentSupport::new(
            self.registry.clone(),
            (*self.config).clone(),
            self.root.clone(),
        )
        .with_persistence(self.persist_sessions)
        .with_env(self.env.to_vec())
        .with_skills_dir(self.skills_dir.clone())
    }

    /// Open the backend that a query names, or the configured default.
    ///
    /// # Errors
    /// Returns a configuration error for an unknown name, a model the backend
    /// does not declare, or a missing credential.
    pub async fn open(&self, query: Option<&str>, executor: ToolExecutor) -> Result<BackendRef> {
        let query = self.resolve_query(query)?;
        let parsed = parse_query(&query)?;
        if self.config.backends.contains_key(&parsed.provider) {
            return self.open_resolved(Some(&query), executor);
        }
        if !self.config.providers.contains_key(&parsed.provider) {
            let mut known: Vec<&str> = self.config.providers.keys().map(String::as_str).collect();
            known.extend(self.config.backends.keys().map(String::as_str));
            return Err(Error::Config(format!(
                "unknown backend \"{}\"; configured: {}",
                parsed.provider,
                known.join(", ")
            )));
        }
        // Resolve one provider only: opening a model must not run another
        // provider's credential command.
        let _env_guard = crate::config_loader::apply_env_specs(&self.env);
        let mut config = (*self.config).clone();
        config.providers.retain(|name, _| name == &parsed.provider);
        crate::config_loader::resolve_config(&mut config).await?;
        let registry = match ModelRegistry::load_async(&config).await {
            Ok(registry) => registry,
            Err(_) => ModelRegistry::load(&config)?,
        };
        BackendFactory::new(config, &self.root, self.env.to_vec())?
            .with_resolved_registry(registry)
            .with_skills_dir(self.skills_dir.clone())
            .with_subagent_persistence(self.persist_sessions)
            .open_resolved(Some(&query), executor)
    }

    /// Open a query against the config and registry this factory already
    /// holds. Credentials must be resolved.
    ///
    /// # Errors
    /// Returns a configuration error for an unknown name or a model the
    /// backend does not declare.
    pub fn open_resolved(&self, query: Option<&str>, executor: ToolExecutor) -> Result<BackendRef> {
        let query = self.resolve_query(query)?;
        let parsed = parse_query(&query)?;
        if self.config.backends.contains_key(&parsed.provider) {
            return self.open_external(&parsed, executor);
        }
        if !self.config.providers.contains_key(&parsed.provider) {
            let mut known: Vec<&str> = self.config.providers.keys().map(String::as_str).collect();
            known.extend(self.config.backends.keys().map(String::as_str));
            return Err(Error::Config(format!(
                "unknown backend \"{}\"; configured: {}",
                parsed.provider,
                known.join(", ")
            )));
        }
        let (agent, _, _) = crate::agent::rebuild_agent(
            None,
            &self.registry,
            &self.config,
            Some(&query),
            &self.root,
            &self.env,
        )?;
        let support = self.support();
        let system_prompt = crate::agent::assemble_system_prompt(
            self.skills_dir.as_deref().and_then(std::path::Path::parent),
            &self.root,
        );
        let executor = executor.with_skills_dir(self.skills_dir.clone());
        Ok(Arc::new(
            agent
                .with_system_prompt(system_prompt)
                .with_source(crate::backend::NativeSource::new(
                    self.registry.clone(),
                    (*self.config).clone(),
                ))
                .with_subagent_support(support)
                .with_executor(&executor),
        ))
    }

    /// Replace the static catalog with a discovered one. A caller that
    /// already resolved remote models passes them through instead of
    /// rebuilding a catalog that cannot name them.
    #[must_use]
    pub fn with_resolved_registry(mut self, registry: ModelRegistry) -> Self {
        self.registry = registry;
        self
    }

    fn open_external(&self, parsed: &ParsedQuery, executor: ToolExecutor) -> Result<BackendRef> {
        let config = self
            .config
            .backends
            .get(&parsed.provider)
            .ok_or_else(|| Error::Config(format!("unknown backend \"{}\"", parsed.provider)))?;
        let model = (parsed.model != "default").then_some(parsed.model.clone());
        if model.is_some()
            && !config.models().iter().any(|id| id == &parsed.model)
            && config.model() != Some(parsed.model.as_str())
        {
            return Err(Error::Config(format!(
                "no model \"{}\" for backend \"{}\"; configured: {}",
                parsed.model,
                parsed.provider,
                config.models().join(", ")
            )));
        }
        if parsed.tier != ServiceTier::Auto {
            return Err(Error::Config(format!(
                "service tier \"{}\" is not supported by \"{}\"",
                parsed.tier.as_str(),
                parsed.provider
            )));
        }
        let thinking = parsed.level.clone().unwrap_or(ThinkingLevel::Off);
        let system_prompt = executor.system_prompt();
        Ok(Arc::new(
            ExternalBackend::configure(
                parsed.provider.clone(),
                config.clone(),
                self.root.clone(),
                executor,
                model,
                thinking,
                system_prompt,
                self.config.compaction.clone(),
            )
            .with_retry(self.config.retry.into()),
        ))
    }

    /// The same conversation on another backend, or the same selection with
    /// another model inside the same backend.
    ///
    /// # Errors
    /// Propagates resolution and provider construction errors.
    pub async fn switch(&self, current: &BackendRef, query: &str) -> Result<BackendRef> {
        let parsed = parse_query(query)?;
        if parsed.provider == current.name() {
            let next = current.with_selection(query)?;
            return Ok(next.with_tool_executor(self.prepare(next.tool_executor())));
        }
        let executor = self.prepare(current.tool_executor());
        self.open(Some(query), executor).await
    }

    /// Session state for a backend opened by this factory.
    fn prepare(&self, executor: ToolExecutor) -> ToolExecutor {
        executor.with_skills_dir(self.skills_dir.clone())
    }

    /// Resolve the configured default when the caller names no model.
    fn resolve_query(&self, query: Option<&str>) -> Result<String> {
        if let Some(query) = query {
            return Ok(query.to_owned());
        }
        if let Some(default) = self.config.default_model.clone() {
            return Ok(default);
        }
        if let Some(name) = self.config.default_provider.clone() {
            if let Some(config) = self.config.backends.get(&name) {
                let model = config
                    .model()
                    .or_else(|| config.models().first().map(String::as_str))
                    .unwrap_or("default");
                return Ok(format!("{name}/{model}"));
            }
            if let Some(choice) = self
                .registry
                .choices()
                .into_iter()
                .find(|c| c.provider == name)
            {
                return Ok(format!("{name}/{}", choice.id));
            }
        }
        // Config order, not picker order: the first model the user wrote is
        // the default, not the alphabetically first one.
        if let Some(model) = self.registry.available().first() {
            return Ok(format!("{}/{}", model.provider, model.id));
        }
        if let Some((name, config)) = self.config.backends.first() {
            let model = config
                .model()
                .or_else(|| config.models().first().map(String::as_str))
                .unwrap_or("default");
            return Ok(format!("{name}/{model}"));
        }
        Err(Error::NoModels(crate::agent::NO_MODELS_HINT.to_string()))
    }

    /// The selection a restored session names.
    #[must_use]
    pub fn selection_of(&self, model: &str, thinking: ThinkingLevel) -> ModelSelection {
        ModelSelection {
            model: model.to_owned(),
            thinking,
            service_tier: ServiceTier::Auto,
        }
    }
}
