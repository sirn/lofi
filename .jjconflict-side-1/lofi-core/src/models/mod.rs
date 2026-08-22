//! Builds a [`ModelRegistry`] from the parsed [`crate::Config`]: each
//! provider's static `models` list is mapped to [`lofi_types::Model`] entries,
//! and any provider with an enabled `auto_models` block fetches its model list
//! from an OpenAI-style models endpoint at startup, injecting the discovered
//! entries into the provider's `models` map (static wins on id collision) and
//! persisting them to the agent-owned state tree so they survive restarts and
//! cover offline launches.
//! [`lofi_types::Model`] carries a `provider` field, and the provider
//! transports put `model.id` verbatim into the wire `"model"` field. To keep
//! that wire contract intact, **`Model.id` stays the raw provider-local id**
//! (e.g. `gpt-4o`), and the qualified `provider/id` form used for unambiguous
//! resolution and `--list-models` is composed from `Model.provider` +
//! `Model.id` at lookup/display time.
//! A provider's `auto_models` block names a models endpoint plus optional
//! field mappings. Each entry is parsed into a `ModelConfig` whose `api` and
//! endpoint `base_url` are resolved from the provider's `api_type` routing
//! table (so one provider can span several upstream APIs, e.g. an
//! OpenAI-compatible proxy), and whose `thinking_levels` are inherited from
//! the block. Discovered entries are injected into the provider's `models`
//! map so the agent layer resolves them identically to static models. The
//! fetched list is cached under `<state>/discovery.json` with a per-block TTL;
//! a fetch failure falls back to the cached entries.

use std::collections::HashMap;
use std::time::Duration;

use indexmap::IndexMap;
use lofi_types::{Config, Model, ModelConfig, ProviderConfig, ServiceTier, ThinkingLevel};

use lofi_error::Result;

use crate::state;

mod auto;
use auto::{
    inject_discovered, read_auto_cache, write_auto_cache, CachedDiscovery, DEFAULT_AUTO_TTL_SECS,
};

#[derive(Debug, Default, Clone)]
pub struct ModelRegistry {
    providers: IndexMap<String, ProviderConfig>,
    models: Vec<Model>,
}

impl ModelRegistry {
    /// # Errors
    /// Returns [`Error::Config`] only if a static model references an unknown
    /// provider (impossible in practice since models are nested under their
    /// provider, but the mapping is fallible for symmetry).
    pub fn load(config: &Config) -> Result<Self> {
        let models = static_models(&config.providers);
        Ok(Self {
            providers: config.providers.clone(),
            models,
        })
    }

    /// Static models win on `provider/id` collision: a discovered entry that
    /// duplicates a static one only fills fields the static entry left unset.
    /// A successful refresh (even one yielding zero models) replaces that
    /// provider's cached entries, so a model the provider removed is not
    /// resurrected by a later failure. If a fetch fails, the previously cached
    /// discovery for that provider is reused; with no cache the provider
    /// contributes only its static models.
    /// # Errors
    /// Returns [`Error::Http`] on a transport failure that is not covered by
    /// the cache fallback, or [`Error::Io`] / [`Error::State`] on cache
    /// write failure.
    pub async fn load_async(config: &Config) -> Result<Self> {
        let mut augmented = config.clone();
        let cache_path = state::discovery_cache_path()?;

        let mut cached: HashMap<String, CachedDiscovery> =
            read_auto_cache(&cache_path).unwrap_or_default();
        let now = auto::now_ms();
        let mut any_refreshed = false;

        for (name, pcfg) in &mut augmented.providers {
            let Some(am) = pcfg.auto_models.as_mut() else {
                continue;
            };
            if !am.enabled {
                continue;
            }
            let am = am.clone();
            let ttl = Duration::from_secs(am.ttl_seconds.unwrap_or(DEFAULT_AUTO_TTL_SECS));

            if let Some(cd) = cached.get(name) {
                let age = Duration::from_millis(now.saturating_sub(cd.fetched_at));
                if age < ttl {
                    inject_discovered(&mut pcfg.models, &cd.entries);
                    continue;
                }
            }

            match auto::fetch_auto_models(pcfg, &am).await {
                Ok(entries) => {
                    any_refreshed = true;
                    cached.insert(
                        name.clone(),
                        CachedDiscovery {
                            fetched_at: now,
                            entries: entries.clone(),
                        },
                    );
                    inject_discovered(&mut pcfg.models, &entries);
                }
                Err(remote_err) => {
                    if let Some(cd) = cached.get(name) {
                        inject_discovered(&mut pcfg.models, &cd.entries);
                    } else {
                        tracing::debug!(
                            provider = %name,
                            error = %remote_err,
                            "auto-models fetch failed and cache is empty; using static models only"
                        );
                    }
                }
            }
        }

        if any_refreshed {
            write_auto_cache(&cache_path, &cached)?;
        }

        let models = static_models(&augmented.providers);
        Ok(Self {
            providers: augmented.providers,
            models,
        })
    }

    #[must_use]
    pub(crate) fn providers(&self) -> &IndexMap<String, ProviderConfig> {
        &self.providers
    }

    #[must_use]
    pub fn available(&self) -> Vec<Model> {
        self.models
            .iter()
            .filter(|m| match self.providers.get(&m.provider) {
                Some(p) => {
                    p.no_auth
                        || p.api_key.as_deref().is_some_and(|k| !k.is_empty())
                        || p.headers
                            .as_ref()
                            .is_some_and(|h| h.values().any(|v| !v.is_empty()))
                }
                None => false,
            })
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn choices(&self) -> Vec<lofi_types::ModelChoice> {
        let mut out: Vec<lofi_types::ModelChoice> = self
            .available()
            .into_iter()
            .map(|m| {
                let mc = self
                    .providers
                    .get(&m.provider)
                    .and_then(|p| p.models.get(&m.id));
                lofi_types::ModelChoice {
                    thinking_levels: mc.map(|mc| mc.thinking_levels.clone()).unwrap_or_default(),
                    service_tiers: mc.map(|mc| mc.service_tiers.clone()).unwrap_or_default(),
                    supports_image: m.supports_image,
                    provider: m.provider,
                    id: m.id,
                    name: m.name,
                    context_window: m.context_window,
                }
            })
            .collect();
        out.sort_by(|a, b| a.provider.cmp(&b.provider).then(a.id.cmp(&b.id)));
        out
    }

    #[must_use]
    pub fn resolve(&self, qualified: &str) -> Option<&Model> {
        let (provider, id) = split_qualified(qualified)?;
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    #[must_use]
    pub fn resolve_by_pattern(&self, query: &str) -> Option<&Model> {
        self.resolve_by_pattern_in(query, None)
    }

    /// Like [`resolve_by_pattern`](Self::resolve_by_pattern) but restricted to
    /// models of `provider`. A qualified query that resolves to a different
    /// provider yields `None` so the caller can raise a clear cross-wire error
    /// instead of silently running a model against the wrong transport.
    #[must_use]
    pub fn resolve_for_provider(&self, query: &str, provider: &str) -> Option<&Model> {
        self.resolve_by_pattern_in(query, Some(provider))
    }

    fn resolve_by_pattern_in(&self, query: &str, provider: Option<&str>) -> Option<&Model> {
        let in_scope = |m: &Model| provider.is_none_or(|p| m.provider == p);
        if let Some(m) = self.resolve(query) {
            return if in_scope(m) { Some(m) } else { None };
        }
        let lower = query.to_ascii_lowercase();
        self.models
            .iter()
            .filter(|m| in_scope(m))
            .find(|m| m.id == query || m.name == query)
            .or_else(|| {
                self.models.iter().filter(|m| in_scope(m)).find(|m| {
                    qualified(m).to_ascii_lowercase().contains(&lower)
                        || m.id.to_ascii_lowercase().contains(&lower)
                        || m.name.to_ascii_lowercase().contains(&lower)
                })
            })
    }

    #[must_use]
    pub fn list_models_print(&self) -> String {
        let mut lines: Vec<String> = self
            .models
            .iter()
            .map(|m| {
                let mut s = format!("{} — {}", qualified(m), m.name);
                if m.supports_image {
                    s.push_str("  ·img");
                }
                s
            })
            .collect();
        lines.sort();
        lines.join("\n")
    }
}

fn qualified(m: &Model) -> String {
    format!("{}/{}", m.provider, m.id)
}

fn split_qualified(s: &str) -> Option<(&str, &str)> {
    let (provider, id) = s.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some((provider, id))
}

fn static_models(providers: &IndexMap<String, ProviderConfig>) -> Vec<Model> {
    let mut out = Vec::new();
    for (name, pcfg) in providers {
        for (id, mc) in &pcfg.models {
            out.push(model_from_config(name, id, pcfg, mc));
        }
    }
    out
}

fn model_from_config(name: &str, id: &str, pcfg: &ProviderConfig, mc: &ModelConfig) -> Model {
    let api = pcfg.resolve_api(mc.api_type.as_deref());
    let base_url = mc
        .base_url
        .clone()
        .unwrap_or_else(|| resolve_model_base_url(pcfg, mc.api_type.as_deref()));
    Model {
        id: id.to_string(),
        name: mc.name.clone().unwrap_or_else(|| id.to_string()),
        provider: name.to_string(),
        api,
        reasoning: mc.reasoning.unwrap_or(false),
        thinking: ThinkingLevel::default(),
        service_tier: ServiceTier::default(),
        supports_image: mc.supports_image.unwrap_or(false),
        context_window: mc.context_window,
        max_tokens: mc.max_tokens,
        base_url: Some(base_url),
        input_price: mc.input_price,
        output_price: mc.output_price,
        cache_read_price: mc.cache_read_price,
        cache_write_price: mc.cache_write_price,
        per_request_price: mc.per_request_price,
    }
}

fn resolve_model_base_url(pcfg: &ProviderConfig, api_type: Option<&str>) -> String {
    let base = pcfg
        .base_url
        .as_deref()
        .unwrap_or_else(|| pcfg.resolve_api(api_type).default_base_url());
    let path = pcfg.resolve_path(api_type);
    join_base_url(Some(base), &path)
}

fn join_base_url(base: Option<&str>, path: &str) -> String {
    let base = base.unwrap_or("").trim_end_matches('/');
    let path = path.trim_start_matches('/');
    format!("{base}/{path}")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::auto::{inject_discovered, parse_auto_models, read_auto_cache, write_auto_cache};

    use super::*;
    use lofi_types::{
        Api, ApiTypeMapping, AutoModelsConfig, FieldMappings, PricingConvention,
        PricingFieldMappings,
    };

    fn mapping() -> ApiTypeMapping {
        ApiTypeMapping {
            path: None,
            pricing_field_mappings: None,
        }
    }

    fn pcfg(api: Api, models: IndexMap<String, ModelConfig>) -> ProviderConfig {
        let mut mappings = IndexMap::new();
        mappings.insert(api.id().to_string(), mapping());
        ProviderConfig {
            api_type: Some(api),
            api_types: mappings,
            base_url: Some("https://api.example.com".to_string()),
            pricing_convention: PricingConvention::PerToken,
            pricing_field_mappings: PricingFieldMappings::default(),
            env_name: None,
            api_key: Some("sk-test".to_string()),
            headers: None,
            models,
            auto_models: None,
            no_auth: false,
            thinking_level: None,
            thinking_levels: Vec::new(),
            service_tier: None,
            service_tiers: Vec::new(),
        }
    }

    fn mc(id: &str) -> (String, ModelConfig) {
        (
            id.to_string(),
            ModelConfig {
                name: None,
                api_type: None,
                reasoning: None,
                supports_image: None,
                context_window: None,
                max_tokens: None,
                thinking_levels: Vec::new(),
                thinking_level: None,
                service_tiers: Vec::new(),
                service_tier: None,
                base_url: None,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
                per_request_price: None,
            },
        )
    }

    fn mc_named(id: &str, name: &str) -> (String, ModelConfig) {
        let (k, mut m) = mc(id);
        m.name = Some(name.to_string());
        (k, m)
    }

    fn models(ids: &[&str]) -> IndexMap<String, ModelConfig> {
        let mut map = IndexMap::new();
        for id in ids {
            let (k, v) = mc(id);
            map.insert(k, v);
        }
        map
    }

    fn config_with(providers: IndexMap<String, ProviderConfig>) -> Config {
        Config {
            providers,
            ..Config::default()
        }
    }

    #[test]
    fn static_models_built_from_each_provider() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            pcfg(Api::OpenAiCompletions, models(&["gpt-4o", "gpt-4o-mini"])),
        );
        providers.insert(
            "anthropic".to_string(),
            pcfg(Api::AnthropicMessages, models(&["claude-opus-4"])),
        );
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        assert_eq!(reg.models.len(), 3);
        let gpt = reg.resolve("openai/gpt-4o").unwrap();
        assert_eq!(gpt.api, Api::OpenAiCompletions);
        assert_eq!(gpt.name, "gpt-4o");
        reg.resolve("anthropic/claude-opus-4").unwrap();
    }

    #[test]
    fn static_model_resolves_default_base_url() {
        let providers = IndexMap::from([(
            "openai".to_string(),
            pcfg(Api::OpenAiCompletions, models(&["gpt-4o"])),
        )]);
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        let m = reg.resolve("openai/gpt-4o").unwrap();
        assert_eq!(
            m.base_url.as_deref(),
            Some("https://api.example.com/v1/chat/completions")
        );
    }

    #[test]
    fn inject_discovered_merges_unset_fields_from_discovered() {
        let mut models = models(&["gpt-4o"]);
        models.get_mut("gpt-4o").unwrap().thinking_levels = vec![ThinkingLevel::Medium];
        let mut discovered = mc_named("gpt-4o", "GPT-4o").1;
        discovered.context_window = Some(128_000);
        discovered.input_price = Some(0.005);
        let entries = vec![("gpt-4o".to_string(), discovered)];
        inject_discovered(&mut models, &entries);
        let m = models.get("gpt-4o").unwrap();
        assert_eq!(m.thinking_levels, vec![ThinkingLevel::Medium]);
        assert_eq!(m.name.as_deref(), Some("GPT-4o"));
        assert_eq!(m.context_window, Some(128_000));
        assert_eq!(m.input_price, Some(0.005));
    }

    #[test]
    fn inject_discovered_preserves_explicit_static_values() {
        let mut models = models(&["gpt-4o"]);
        models.get_mut("gpt-4o").unwrap().name = Some("Custom".to_string());
        models.get_mut("gpt-4o").unwrap().context_window = Some(64_000);
        let mut discovered = mc_named("gpt-4o", "GPT-4o").1;
        discovered.context_window = Some(128_000);
        let entries = vec![("gpt-4o".to_string(), discovered)];
        inject_discovered(&mut models, &entries);
        let m = models.get("gpt-4o").unwrap();
        assert_eq!(m.name.as_deref(), Some("Custom"));
        assert_eq!(m.context_window, Some(64_000));
    }

    #[test]
    fn inject_discovered_appends_new_ids() {
        let mut models = models(&["gpt-4o"]);
        let entries = vec![("claude".to_string(), mc_named("claude", "Claude").1)];
        inject_discovered(&mut models, &entries);
        assert!(models.contains_key("claude"));
        assert_eq!(
            models.get("claude").unwrap().name.as_deref(),
            Some("Claude")
        );
    }

    #[test]
    fn resolve_and_split_qualified() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            pcfg(Api::OpenAiCompletions, models(&["gpt-4o"])),
        );
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        let m = reg.resolve("openai/gpt-4o").unwrap();
        assert_eq!(m.id, "gpt-4o");
        assert_eq!(m.provider, "openai");
        assert!(reg.resolve("openai/nope").is_none());
    }

    #[test]
    fn resolve_by_pattern_ladder() {
        let mut providers = IndexMap::new();
        let mut m = IndexMap::new();
        m.insert("gpt-4o".to_string(), mc_named("gpt-4o", "GPT 4o").1);
        m.insert("gpt-4o-mini".to_string(), mc("gpt-4o-mini").1);
        providers.insert("openai".to_string(), pcfg(Api::OpenAiCompletions, m));
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        assert_eq!(
            reg.resolve_by_pattern("openai/gpt-4o")
                .map(|m| m.id.clone()),
            Some("gpt-4o".to_string())
        );
        assert_eq!(
            reg.resolve_by_pattern("gpt-4o-mini").map(|m| m.id.clone()),
            Some("gpt-4o-mini".to_string())
        );
        assert_eq!(
            reg.resolve_by_pattern("GPT 4o").map(|m| m.id.clone()),
            Some("gpt-4o".to_string())
        );
        assert_eq!(
            reg.resolve_by_pattern("mini").map(|m| m.id.clone()),
            Some("gpt-4o-mini".to_string())
        );
        assert!(reg.resolve_by_pattern("nope").is_none());
    }

    #[test]
    fn available_filters_by_resolved_key() {
        let mut providers = IndexMap::new();
        providers.insert(
            "openai".to_string(),
            pcfg(Api::OpenAiCompletions, models(&["gpt-4o"])),
        );
        let keyed = pcfg(Api::OpenAiCompletions, models(&["m2"]));
        let mut empty = pcfg(Api::OpenAiCompletions, models(&["m3"]));
        empty.api_key = None;
        let mut no_key = pcfg(Api::OpenAiCompletions, models(&["m4"]));
        no_key.api_key = Some(String::new());
        providers.insert("keyed".to_string(), keyed);
        providers.insert("empty".to_string(), empty);
        providers.insert("none".to_string(), no_key);
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        let available = reg.available();
        let ids: Vec<&str> = available.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"gpt-4o"));
        assert!(ids.contains(&"m2"));
        assert!(!ids.contains(&"m3"));
        assert!(!ids.contains(&"m4"));
    }

    #[test]
    fn choices_carry_thinking_levels_and_sort() {
        let mut m = models(&["gpt-4o-mini", "gpt-4o"]);
        let gpt4o = m.get_mut("gpt-4o").unwrap();
        gpt4o.thinking_levels = vec![ThinkingLevel::Medium, ThinkingLevel::High];
        gpt4o.supports_image = Some(true);
        let mut providers = IndexMap::new();
        providers.insert(
            "anthropic".to_string(),
            pcfg(Api::AnthropicMessages, models(&["claude"])),
        );
        providers.insert("openai".to_string(), pcfg(Api::OpenAiCompletions, m));
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        let choices = reg.choices();
        let qualified: Vec<String> = choices
            .iter()
            .map(|c| format!("{}/{}", c.provider, c.id))
            .collect();
        assert_eq!(
            qualified,
            vec!["anthropic/claude", "openai/gpt-4o", "openai/gpt-4o-mini"]
        );
        let by_id: std::collections::HashMap<&str, &lofi_types::ModelChoice> =
            choices.iter().map(|c| (c.id.as_str(), c)).collect();
        assert_eq!(
            by_id["gpt-4o"].thinking_levels,
            vec![ThinkingLevel::Medium, ThinkingLevel::High]
        );
        assert!(by_id["gpt-4o-mini"].thinking_levels.is_empty());
        assert!(by_id["gpt-4o"].supports_image);
        assert!(!by_id["gpt-4o-mini"].supports_image);
        assert_eq!(by_id["gpt-4o"].provider, "openai");
    }

    #[test]
    fn list_models_print_formats_qualified_lines() {
        let mut providers = IndexMap::new();
        let mut m = IndexMap::new();
        m.insert("gpt-4o".to_string(), mc_named("gpt-4o", "GPT 4o").1);
        providers.insert("openai".to_string(), pcfg(Api::OpenAiCompletions, m));
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        let out = reg.list_models_print();
        assert_eq!(out, "openai/gpt-4o — GPT 4o");
    }

    #[test]
    fn list_models_print_marks_image_support() {
        let mut providers = IndexMap::new();
        let mut m = IndexMap::new();
        let mut gpt4o = mc_named("gpt-4o", "GPT 4o").1;
        gpt4o.supports_image = Some(true);
        m.insert("gpt-4o".to_string(), gpt4o);
        m.insert("gpt-4o-mini".to_string(), mc("gpt-4o-mini").1);
        providers.insert("openai".to_string(), pcfg(Api::OpenAiCompletions, m));
        let reg = ModelRegistry::load(&config_with(providers)).unwrap();
        let out = reg.list_models_print();
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[0], "openai/gpt-4o — GPT 4o  ·img");
        assert_eq!(lines[1], "openai/gpt-4o-mini — gpt-4o-mini");
    }

    #[test]
    fn auto_cache_round_trip() {
        let mut cache: HashMap<String, CachedDiscovery> = HashMap::new();
        cache.insert(
            "anthropic".to_string(),
            CachedDiscovery {
                fetched_at: auto::now_ms(),
                entries: vec![(
                    "claude-opus-4".to_string(),
                    mc_named("claude-opus-4", "Claude Opus 4").1,
                )],
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto.json");
        write_auto_cache(&path, &cache).unwrap();
        let loaded = read_auto_cache(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        let cd = loaded.get("anthropic").unwrap();
        assert_eq!(cd.entries[0].0, "claude-opus-4");
        assert_eq!(cd.entries[0].1.name.as_deref(), Some("Claude Opus 4"));
    }

    #[test]
    fn parse_auto_models_maps_id_name_and_api_override() {
        let payload = serde_json::json!({
            "data": [
                {"id": "remote-1", "name": "Remote One", "preferred_api": "messages"},
            ]
        });
        let mut p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        p.api_types.insert(
            "anthropic-messages".to_string(),
            ApiTypeMapping {
                path: None,
                pricing_field_mappings: None,
            },
        );
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: Some("preferred_api".to_string()),
            api_type_mappings: HashMap::from([("messages".to_string(), Api::AnthropicMessages)]),
            field_mappings: FieldMappings::default(),
            thinking_levels: vec![ThinkingLevel::Medium],
            thinking_level: None,
            service_tiers: Vec::new(),
            service_tier: None,
            ttl_seconds: None,
        };
        let models = parse_auto_models(&p, &am, &payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "remote-1");
        assert_eq!(models[0].1.name.as_deref(), Some("Remote One"));
        assert_eq!(models[0].1.api_type.as_deref(), Some("anthropic-messages"));
        assert_eq!(models[0].1.thinking_levels, vec![ThinkingLevel::Medium]);
        assert!(models[0].1.reasoning.unwrap_or(false));
        assert_eq!(
            models[0].1.base_url.as_deref(),
            Some("https://api.example.com/v1/messages")
        );
    }

    #[test]
    fn parse_auto_models_reads_pricing_from_default_paths() {
        let payload = serde_json::json!({
            "data": [
                {
                    "id": "m",
                    "name": "M",
                    "pricing": {
                        "prompt": "1.4e-7",
                        "completion": "2.8e-7",
                        "input_cache_read": "2.8e-9",
                        "input_cache_write": "0"
                    }
                }
            ]
        });
        let p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: None,
            api_type_mappings: HashMap::new(),
            field_mappings: FieldMappings::default(),
            thinking_levels: vec![],
            thinking_level: None,
            service_tiers: Vec::new(),
            service_tier: None,
            ttl_seconds: None,
        };
        let models = parse_auto_models(&p, &am, &payload);
        let mc = &models[0].1;
        assert_eq!(mc.input_price, Some(0.14));
        assert_eq!(mc.output_price, Some(0.28));
        assert_eq!(mc.cache_read_price, Some(0.0028));
        assert_eq!(mc.cache_write_price, Some(0.0));
    }

    #[test]
    fn parse_auto_models_per_million_convention_uses_values_as_is() {
        let payload = serde_json::json!({
            "data": [
                {"id": "m", "pricing": {"prompt": 1.5, "completion": 3.0}}
            ]
        });
        let mut p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        p.pricing_convention = PricingConvention::PerMillion;
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: None,
            api_type_mappings: HashMap::new(),
            field_mappings: FieldMappings::default(),
            thinking_levels: vec![],
            thinking_level: None,
            service_tiers: Vec::new(),
            service_tier: None,
            ttl_seconds: None,
        };
        let models = parse_auto_models(&p, &am, &payload);
        let mc = &models[0].1;
        assert_eq!(mc.input_price, Some(1.5));
        assert_eq!(mc.output_price, Some(3.0));
    }

    #[test]
    fn parse_auto_models_custom_pricing_paths_on_mapping() {
        let payload = serde_json::json!({
            "data": [
                {"id": "m", "cost": {"in": "0.000002", "out": "0.000006"}}
            ]
        });
        let mut p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        p.api_types
            .get_mut("openai-completions")
            .unwrap()
            .pricing_field_mappings = Some(PricingFieldMappings {
            input: Some("cost.in".to_string()),
            output: Some("cost.out".to_string()),
            cache_read: None,
            cache_write: None,
            per_request: None,
        });
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: None,
            api_type_mappings: HashMap::new(),
            field_mappings: FieldMappings::default(),
            thinking_levels: vec![],
            thinking_level: None,
            service_tiers: Vec::new(),
            service_tier: None,
            ttl_seconds: None,
        };
        let models = parse_auto_models(&p, &am, &payload);
        let mc = &models[0].1;
        assert_eq!(mc.input_price, Some(2.0));
        assert_eq!(mc.output_price, Some(6.0));
        assert_eq!(mc.cache_read_price, None);
        assert_eq!(mc.cache_write_price, None);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn load_async_uses_cache_when_remote_unreachable() {
        let _env = crate::state::STATE_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("XDG_STATE_HOME");
        std::env::set_var("XDG_STATE_HOME", dir.path());
        let cache_path = crate::state::discovery_cache_path().unwrap();
        let mut cache: HashMap<String, CachedDiscovery> = HashMap::new();
        cache.insert(
            "anthropic".to_string(),
            CachedDiscovery {
                fetched_at: auto::now_ms(),
                entries: vec![(
                    "claude-opus-4".to_string(),
                    mc_named("claude-opus-4", "Claude Opus 4").1,
                )],
            },
        );
        write_auto_cache(&cache_path, &cache).unwrap();

        let mut providers = IndexMap::new();
        let mut p = pcfg(Api::AnthropicMessages, IndexMap::new());
        p.auto_models = Some(AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: Some("http://127.0.0.1:1/v1/models".to_string()),
            path: "data".to_string(),
            api_type_field: None,
            api_type_mappings: HashMap::new(),
            field_mappings: FieldMappings::default(),
            thinking_levels: vec![],
            thinking_level: None,
            service_tiers: Vec::new(),
            service_tier: None,
            ttl_seconds: Some(0),
        });
        providers.insert("anthropic".to_string(), p);
        let cfg = config_with(providers);

        let reg = ModelRegistry::load_async(&cfg).await.unwrap();
        let m = reg.resolve("anthropic/claude-opus-4").unwrap();
        assert_eq!(m.api, Api::AnthropicMessages);

        match prev {
            Some(v) => std::env::set_var("XDG_STATE_HOME", v),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
    }
}
