//! The model registry.
//!
//! Builds a [`ModelRegistry`] from the parsed [`crate::Config`]: each
//! provider's static `models` list is mapped to [`lofi_types::Model`] entries,
//! and any provider with an enabled `auto_models` block fetches its model list
//! from an OpenAI-style `/v1/models` endpoint at startup, injecting the
//! discovered entries into the provider's `models` map (static wins on id
//! collision) and persisting them to the agent-owned state tree so they survive
//! restarts and cover offline launches.
//!
//! ## Identifier representation
//!
//! [`lofi_types::Model`] already carries a `provider` field, and the existing
//! provider transports put `model.id` verbatim into the wire `"model"` field
//! (see [`lofi_providers::ir`]). To keep that wire contract intact, **`Model.id` stays
//! the raw provider-local id** (e.g. `gpt-4o`), and the qualified
//! `provider/id` form used for unambiguous resolution and `--list-models` is
//! *composed* from `Model.provider` + `Model.id` at lookup/display time. No
//! extra field is added; this is the least-surprising choice for callers that
//! already pass `Model` straight to the API.
//!
//! ## Auto-models discovery
//!
//! A provider's `auto_models` block names a models endpoint (default
//! `{base_url}/models`) plus optional field mappings. Each entry is parsed into
//! a `ModelConfig` whose `api` is resolved from `api_type_field` +
//! `api_type_mappings` (so one provider can span several upstream APIs, e.g. a
//! OpenAI-compatible proxy), and whose `thinking_levels` are inherited from the block.
//! Discovered entries are injected into the provider's `models` map so the
//! agent layer can resolve them identically to static models. The fetched list
//! is cached under `<state>/discovery.json` with a per-block TTL; a fetch
//! failure falls back to the cached entries.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use indexmap::IndexMap;
use lofi_types::{Api, AutoModelsConfig, Config, Model, ModelConfig, ProviderConfig, ThinkingLevel};
use serde_json::Value;

use crate::state;
use lofi_error::{Error, Result};
use lofi_providers::anthropic_messages::ANTHROPIC_VERSION;
use lofi_providers::apply_headers;

/// The registry of models available to the agent.
///
/// `providers` holds the resolved provider configs (so [`Self::available`] can
/// tell which providers have a key without re-reading the config); `models` is
/// the merged static + discovered list, keyed logically by `provider/id`.
#[derive(Debug, Default, Clone)]
pub struct ModelRegistry {
    providers: IndexMap<String, ProviderConfig>,
    models: Vec<Model>,
}

impl ModelRegistry {
    /// Build a registry from the **static** model lists only — no network.
    ///
    /// Use this for tests and offline launches; [`Self::load_async`] adds
    /// remote discovery on top.
    ///
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

    /// Build a registry, refreshing each provider's `discover` block from the
    /// network and persisting the result to the agent state cache.
    ///
    /// Static models win on `provider/id` collision: a discovered entry that
    /// duplicates a static one is dropped. A successful
    /// refresh (even one yielding zero models) replaces that provider's cached
    /// entries, so a model the provider removed is not resurrected by a later
    /// failure. If a fetch fails, the previously cached discovery for that
    /// provider is reused; with no cache the provider contributes only its
    /// static models.
    ///
    /// # Errors
    /// Returns [`Error::Http`] on a transport failure that is not covered by
    /// the cache fallback, or [`Error::Io`] / [`Error::State`] on cache
    /// write failure.
    pub async fn load_async(config: &Config) -> Result<Self> {
        // Augment a copy of the config with auto-discovered models so the
        // registry (and thinking-level resolution) treats them identically to
        // static entries. Static models always win on id collision.
        let mut augmented = config.clone();
        let cache_path = state::discovery_cache_path()?;
        let cache_fresh = cache_age(&cache_path).ok().flatten();

        let mut cached: HashMap<String, Vec<(String, ModelConfig)>> =
            read_auto_cache(&cache_path).unwrap_or_default();
        let mut any_refreshed = false;

        for (name, pcfg) in &mut augmented.providers {
            let Some(am) = pcfg.auto_models.as_mut() else { continue };
            if !am.enabled {
                continue;
            }
            // Clone so the mutable borrow of `pcfg.auto_models` ends before
            // `fetch_auto_models` takes an immutable borrow of `pcfg`.
            let am = am.clone();
            let ttl = Duration::from_secs(am.ttl_seconds.unwrap_or(DEFAULT_AUTO_TTL_SECS));

            // Serve from cache when fresh enough, avoiding a network round
            // trip on every startup within the TTL window.
            if cache_fresh.is_some_and(|age| age < ttl) {
                if let Some(entries) = cached.get(name) {
                    inject_discovered(&mut pcfg.models, entries);
                    continue;
                }
            }

            match fetch_auto_models(pcfg, &am).await {
                Ok(entries) => {
                    any_refreshed = true;
                    cached.insert(name.clone(), entries.clone());
                    inject_discovered(&mut pcfg.models, &entries);
                }
                Err(remote_err) => {
                    // Fall back to whatever the cache holds for this provider.
                    if let Some(entries) = cached.get(name) {
                        inject_discovered(&mut pcfg.models, entries);
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

    /// All models whose provider has a resolved (non-empty) `api_key`.
    ///
    /// The registry was built from an already-resolved [`Config`], so a
    /// The (possibly auto-discovery-augmented) provider configs the registry
    /// was built from. Exposed so the agent layer can resolve per-model
    /// settings (e.g. thinking levels) for discovered models that exist only
    /// in the registry's augmented copy.
    #[must_use]
    pub(crate) fn providers(&self) -> &IndexMap<String, ProviderConfig> {
        &self.providers
    }

    /// provider "has a key" when its `api_key` is `Some` and non-empty.
    #[must_use]
    pub fn available(&self) -> Vec<Model> {
        self.models
            .iter()
            .filter(|m| match self.providers.get(&m.provider) {
                // A provider is usable when it can authenticate (an API key,
                // or a custom header carrying credentials such as a proxy
                // token), or when it is explicitly marked `no_auth` (a local
                // endpoint). Without `no_auth`, a provider with neither key
                // nor credential header is rejected — so the zero-config
                // OpenAI default still requires `OPENAI_API_KEY`.
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

    /// Resolve an exact `provider/id` qualifier to a model.
    #[must_use]
    pub fn resolve(&self, qualified: &str) -> Option<&Model> {
        let (provider, id) = split_qualified(qualified)?;
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == id)
    }

    /// Resolve a free-form query to a model using a small fallback ladder:
    ///
    /// 1. exact `provider/id` qualifier;
    /// 2. exact raw `id`;
    /// 3. exact display `name`;
    /// 4. case-insensitive substring of the qualified form, raw id, or name
    ///    (first match in registry order).
    ///
    /// Step 4 is a convenience for `--model gpt-4o`-style abbreviations; it is
    /// intentionally prefix-free so a more specific qualifier always wins.
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

    /// Format the registry as `provider/id — name` lines for `--list-models`.
    ///
    /// Sorted by provider then id for stable output. Does **not** print — the
    /// caller writes the returned string to stdout.
    #[must_use]
    pub fn list_models_print(&self) -> String {
        let mut lines: Vec<String> = self
            .models
            .iter()
            .map(|m| format!("{} — {}", qualified(m), m.name))
            .collect();
        lines.sort();
        lines.join("\n")
    }
}

/// Compose the qualified `provider/id` form of a model.
fn qualified(m: &Model) -> String {
    format!("{}/{}", m.provider, m.id)
}

/// Split a `provider/id` qualifier; returns `None` if there is no `/`.
fn split_qualified(s: &str) -> Option<(&str, &str)> {
    let (provider, id) = s.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    Some((provider, id))
}

/// Build the static model list from every provider's `models` block.
///
/// Provider and model insertion order (from the [`IndexMap`]) is preserved so
/// "first available" selection is deterministic.
fn static_models(providers: &IndexMap<String, ProviderConfig>) -> Vec<Model> {
    let mut out = Vec::new();
    for (name, pcfg) in providers {
        for (id, mc) in &pcfg.models {
            out.push(model_from_config(name, id, pcfg, mc));
        }
    }
    out
}

/// Map a [`ModelConfig`] into a resolved [`Model`] under provider `name`.
///
/// `id` is the map key from the provider's `models` table. The effective
/// thinking level is left at the default ([`ThinkingLevel::Off`]); the agent
/// resolves and overrides it at selection time.
fn model_from_config(name: &str, id: &str, pcfg: &ProviderConfig, mc: &ModelConfig) -> Model {
    Model {
        id: id.to_string(),
        name: mc.name.clone().unwrap_or_else(|| id.to_string()),
        provider: name.to_string(),
        api: mc.api.unwrap_or(pcfg.api_type),
        reasoning: mc.reasoning.unwrap_or(false),
        thinking: ThinkingLevel::default(),
        supports_image: mc.supports_image.unwrap_or(false),
        context_window: mc.context_window,
        max_tokens: mc.max_tokens,
        base_url: mc.base_url.clone(),
        input_price: mc.input_price,
        output_price: mc.output_price,
        cache_read_price: mc.cache_read_price,
        cache_write_price: mc.cache_write_price,
    }
}

/// Default cache freshness for auto-discovered model lists (5 minutes).
const DEFAULT_AUTO_TTL_SECS: u64 = 300;

/// Insert auto-discovered `(id, ModelConfig)` entries into a provider's
/// `models` map. A static entry wins on id collision, but any field it leaves
/// unset is filled from the discovered entry — so a static entry that only
/// pins `thinking_levels` still inherits `context_window`, pricing, and other
/// metadata the remote endpoint reports. New ids are appended in discovery
/// order.
fn inject_discovered(
    models: &mut IndexMap<String, ModelConfig>,
    entries: &[(String, ModelConfig)],
) {
    for (id, mc) in entries {
        match models.entry(id.clone()) {
            indexmap::map::Entry::Occupied(mut e) => {
                let existing = e.get_mut();
                fill_missing(existing, mc);
            }
            indexmap::map::Entry::Vacant(e) => {
                e.insert(mc.clone());
            }
        }
    }
}

/// Copy each unset field of `dst` from `src`, so a static entry inherits
/// metadata the remote endpoint reports without the user re-declaring it.
/// Explicit static values are preserved (including `None` when the user set a
/// field to empty intentionally — but serde defaults unset fields to `None`,
/// so in practice this fills what the user omitted).
fn fill_missing(dst: &mut ModelConfig, src: &ModelConfig) {
    if dst.name.is_none() {
        dst.name = src.name.clone();
    }
    if dst.api.is_none() {
        dst.api = src.api;
    }
    if dst.reasoning.is_none() {
        dst.reasoning = src.reasoning;
    }
    if dst.supports_image.is_none() {
        dst.supports_image = src.supports_image;
    }
    if dst.context_window.is_none() {
        dst.context_window = src.context_window;
    }
    if dst.max_tokens.is_none() {
        dst.max_tokens = src.max_tokens;
    }
    if dst.thinking_levels.is_empty() {
        dst.thinking_levels = src.thinking_levels.clone();
    }
    if dst.thinking_level.is_none() {
        dst.thinking_level = src.thinking_level;
    }
    if dst.base_url.is_none() {
        dst.base_url = src.base_url.clone();
    }
    if dst.input_price.is_none() {
        dst.input_price = src.input_price;
    }
    if dst.output_price.is_none() {
        dst.output_price = src.output_price;
    }
    if dst.cache_read_price.is_none() {
        dst.cache_read_price = src.cache_read_price;
    }
    if dst.cache_write_price.is_none() {
        dst.cache_write_price = src.cache_write_price;
    }
}

/// Fetch a provider's auto-discovered model list from `models_url` (default
/// `{base_url}/models`) and map it to `(id, ModelConfig)` entries. When
/// `auth` is false the request is sent without credentials (public list
/// endpoints that reject auth headers).
async fn fetch_auto_models(
    pcfg: &ProviderConfig,
    am: &AutoModelsConfig,
) -> Result<Vec<(String, ModelConfig)>> {
    let url = if let Some(u) = &am.models_url {
        u.clone()
    } else {
        let base = pcfg
            .base_url
            .as_deref()
            .unwrap_or_else(|| pcfg.api_type.default_base_url())
            .trim_end_matches('/');
        format!("{base}/models")
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let (api_key, headers) = lofi_providers::effective_credentials(pcfg);
    let key_arg = if api_key.is_empty() { None } else { Some(&api_key[..]) };
    let req = if am.auth {
        authed_get(&client, pcfg.api_type, key_arg, &url)
    } else {
        client.get(&url)
    };
    let req = apply_headers(req, &headers);

    let resp = req.send().await?;
    let resp = lofi_providers::ensure_ok(resp).await?;
    let body: Value =
        lofi_providers::read_json_capped(resp, lofi_providers::MAX_DISCOVERY_BODY_BYTES).await?;

    Ok(parse_auto_models(pcfg, am, &body))
}

/// Build an authenticated GET request for `url` using the per-`Api` auth
/// scheme. Mirrors the concrete transports' header wiring.
fn authed_get(
    client: &reqwest::Client,
    api: Api,
    api_key: Option<&str>,
    url: &str,
) -> reqwest::RequestBuilder {
    let req = client.get(url);
    let key = api_key.filter(|k| !k.is_empty());
    match api {
        Api::OpenAiCompletions | Api::OpenAiResponses => match key {
            Some(k) => req.bearer_auth(k),
            None => req,
        },
        Api::AnthropicMessages => match key {
            Some(k) => req
                .header("x-api-key", k)
                .header("anthropic-version", ANTHROPIC_VERSION),
            None => req.header("anthropic-version", ANTHROPIC_VERSION),
        },
    }
}

/// Navigate `body` to the array at `am.path` and map each entry to an
/// `(id, ModelConfig)` pair. The per-model `api` is resolved from
/// `api_type_field` + `api_type_mappings` (falling back to the provider's
/// `api_type`), so one provider can span several upstream APIs. Discovered
/// models inherit `thinking_levels`/`thinking_level` from the `auto_models`
/// config (lofi has no built-in model catalog); `reasoning` is inferred as true when
/// any thinking levels are declared.
/// Read a JSON value as `f64`, accepting either a number or a numeric
/// string (some providers return pricing as strings like `"5e-7"`).
fn json_num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

fn parse_auto_models(
    pcfg: &ProviderConfig,
    am: &AutoModelsConfig,
    body: &Value,
) -> Vec<(String, ModelConfig)> {
    let Some(arr) = navigate(body, &am.path).and_then(Value::as_array) else {
        return Vec::new();
    };
    let default_mapping = am
        .default_api_type
        .as_deref()        .and_then(|k| am.api_type_mappings.get(k));
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        // `preferred_api` may be a string or an array (some providers return
        // `["responses"]`); take the first element either way so each model
        // routes to its real upstream API instead of the block default.
        let preferred = am
            .api_type_field
            .as_deref()
            .and_then(|field| entry.get(field))
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_array().and_then(|a| a.first()).and_then(Value::as_str).map(str::to_string))
            });
        let mapping = preferred
            .as_deref()
            .and_then(|v| am.api_type_mappings.get(v))
            .or(default_mapping);
        let api = mapping.map_or(pcfg.api_type, |m| m.api);
        // A mapping path (e.g. "/v1") is joined onto the provider base_url so
        // a proxy routing one base URL to several upstream APIs sends each
        // model to the right endpoint. With no path, base_url stays None and
        // the provider base_url is used.
        let base_url = mapping
            .and_then(|m| m.path.as_deref())
            .map(|p| join_base_url(pcfg.base_url.as_deref(), p));
        let display_name = entry
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string);
        // OpenRouter-style providers expose `context_length`, per-token
        // `pricing`, and `supported_parameters`. Wire them in so discovered
        // models carry a real context window, cost tracking, and thinking
        // support without a per-model static entry.
        let context_window = entry.get("context_length").and_then(Value::as_u64);
        let max_tokens = entry
            .get("top_provider")
            .and_then(|tp| tp.get("max_completion_tokens"))
            .and_then(Value::as_u64);
        // Pricing paths are configurable so a proxy whose pricing lives under
        // non-standard keys (or reports per-million instead of per-token) can
        // be mapped without code changes. Each dimension is read independently
        // so an endpoint that omits cache-write pricing still yields input/
        // output costs.
        let scale = match am.pricing_convention {
            lofi_types::PricingConvention::PerToken => 1_000_000.0,
            lofi_types::PricingConvention::PerMillion => 1.0,
        };
        let fields = &am.pricing_field_mappings;
        let price = |path: &Option<String>| {
            path.as_deref()
                .and_then(|p| navigate(entry, p))
                .and_then(json_num)
                .map(|x| x * scale)
        };
        let input_price = price(&fields.input);
        let output_price = price(&fields.output);
        let cache_read_price = price(&fields.cache_read);
        let cache_write_price = price(&fields.cache_write);
        let supports_reasoning = entry
            .get("supported_parameters")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|p| p.as_str() == Some("reasoning")));
        let thinking_levels = if !am.thinking_levels.is_empty() {
            am.thinking_levels.clone()
        } else if supports_reasoning {
            vec![
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
                ThinkingLevel::XHigh,
            ]
        } else {
            Vec::new()
        };
        out.push((
            id.to_string(),
            ModelConfig {
                name: display_name,
                api: Some(api),
                reasoning: Some(!thinking_levels.is_empty()),
                supports_image: None,
                context_window,
                max_tokens,
                thinking_levels,
                thinking_level: am.thinking_level,
                base_url,
                input_price,
                output_price,
                cache_read_price,
                cache_write_price,
            },
        ));
    }
    out
}

/// Join a mapping `path` (e.g. `/v1`) onto a provider `base_url`, trimming
/// the trailing/leading slash so the result is `base/path`.
fn join_base_url(base: Option<&str>, path: &str) -> String {
    let base = base.unwrap_or("").trim_end_matches('/');
    let path = path.trim_start_matches('/');
    format!("{base}/{path}")
}

/// Walk a dot-separated path through a JSON object (`data.models`, etc.).
fn navigate<'a>(mut value: &'a Value, path: &str) -> Option<&'a Value> {
    for segment in path.split('.') {
        if segment.is_empty() {
            continue;
        }
        value = value.get(segment)?;
    }
    Some(value)
}

/// Age of the cache file, or `None` if it does not exist. Used to decide
/// whether the cached auto-models list is fresh enough to skip a network fetch.
fn cache_age(path: &Path) -> Result<Option<Duration>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let mtime = meta.modified()?;
    let elapsed = SystemTime::now().duration_since(mtime).unwrap_or(Duration::ZERO);
    Ok(Some(elapsed))
}

/// Write the per-provider auto-discovered model lists to `path` as pretty
/// JSON. Each entry is an `[id, ModelConfig]` tuple so the id (which is the
/// map key in the provider's `models` table) round-trips with its config.
fn write_auto_cache(path: &Path, cache: &HashMap<String, Vec<(String, ModelConfig)>>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache)
        .map_err(|e| Error::State(format!("auto-models cache encode error: {e}")))?;
    std::fs::write(path, json)?;
    Ok(())
}

/// Read the cached auto-models map from `path`. A missing file yields an
/// empty map (cache miss, not an error); parse failures propagate.
fn read_auto_cache(path: &Path) -> Result<HashMap<String, Vec<(String, ModelConfig)>>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(HashMap::new()),
        Ok(s) => Ok(serde_json::from_str(&s)
            .map_err(|e| Error::State(format!("auto-models cache decode error: {e}")))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{AutoModelsConfig, ApiTypeMapping};

    fn pcfg(api: Api, models: IndexMap<String, ModelConfig>) -> ProviderConfig {
        ProviderConfig {
            api_type: api,
            base_url: Some("https://api.example.com/v1".to_string()),
            env_name: None,
            api_key: Some("sk-test".to_string()),
            headers: None,
            models,
            auto_models: None,
            no_auth: false,
            thinking_level: None,
        }
    }

    fn mc(id: &str) -> (String, ModelConfig) {
        (
            id.to_string(),
            ModelConfig {
                name: None,
                api: None,
                reasoning: None,
                supports_image: None,
                context_window: None,
                max_tokens: None,
                thinking_levels: Vec::new(),
                thinking_level: None,
                base_url: None,
                input_price: None,
                output_price: None,
                cache_read_price: None,
                cache_write_price: None,
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
            agent: lofi_types::AgentConfig::default(),
            default_provider: None,
            default_model: None,
            providers,
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
    fn inject_discovered_merges_unset_fields_from_discovered() {
        // A static entry that only sets thinking_levels should inherit
        // context_window, pricing, and name from the discovered entry.
        let mut models = models(&["gpt-4o"]);
        models.get_mut("gpt-4o").unwrap().thinking_levels =
            vec![ThinkingLevel::Medium];
        let mut discovered = mc_named("gpt-4o", "GPT-4o").1;
        discovered.context_window = Some(128_000);
        discovered.input_price = Some(0.005);
        let entries = vec![("gpt-4o".to_string(), discovered)];
        inject_discovered(&mut models, &entries);
        let m = models.get("gpt-4o").unwrap();
        // Static thinking_levels preserved.
        assert_eq!(m.thinking_levels, vec![ThinkingLevel::Medium]);
        // Discovered fields filled in where static left them unset.
        assert_eq!(m.name.as_deref(), Some("GPT-4o"));
        assert_eq!(m.context_window, Some(128_000));
        assert_eq!(m.input_price, Some(0.005));
    }

    #[test]
    fn inject_discovered_preserves_explicit_static_values() {
        // A static entry that explicitly sets a field keeps it; the
        // discovered value does not override.
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
        assert_eq!(models.get("claude").unwrap().name.as_deref(), Some("Claude"));
    }

    #[test]
    fn resolve_and_split_qualified() {
        let mut providers = IndexMap::new();
        providers.insert("openai".to_string(), pcfg(Api::OpenAiCompletions, models(&["gpt-4o"])));
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
        assert_eq!(reg.resolve_by_pattern("openai/gpt-4o").map(|m| m.id.clone()), Some("gpt-4o".to_string()));
        assert_eq!(reg.resolve_by_pattern("gpt-4o-mini").map(|m| m.id.clone()), Some("gpt-4o-mini".to_string()));
        assert_eq!(reg.resolve_by_pattern("GPT 4o").map(|m| m.id.clone()), Some("gpt-4o".to_string()));
        assert_eq!(reg.resolve_by_pattern("mini").map(|m| m.id.clone()), Some("gpt-4o-mini".to_string()));
        assert!(reg.resolve_by_pattern("nope").is_none());
    }

    #[test]
    fn available_filters_by_resolved_key() {
        let mut providers = IndexMap::new();
        providers.insert("openai".to_string(), pcfg(Api::OpenAiCompletions, models(&["gpt-4o"])));
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
    fn auto_cache_round_trip() {
        let mut cache: HashMap<String, Vec<(String, ModelConfig)>> = HashMap::new();
        cache.insert(
            "anthropic".to_string(),
            vec![("claude-opus-4".to_string(), mc_named("claude-opus-4", "Claude Opus 4").1)],
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auto.json");
        write_auto_cache(&path, &cache).unwrap();
        let loaded = read_auto_cache(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        let entries = loaded.get("anthropic").unwrap();
        assert_eq!(entries[0].0, "claude-opus-4");
        assert_eq!(entries[0].1.name.as_deref(), Some("Claude Opus 4"));
    }

    #[test]
    fn parse_auto_models_maps_id_name_and_api_override() {
        let payload = serde_json::json!({
            "data": [
                {"id": "remote-1", "name": "Remote One", "preferred_api": "messages"},
            ]
        });
        let p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        let mut mappings = IndexMap::new();
        mappings.insert(
            "messages".to_string(),
            ApiTypeMapping { api: Api::AnthropicMessages, path: None },
        );
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: Some("preferred_api".to_string()),
            api_type_mappings: mappings,
            default_api_type: None,
            thinking_levels: vec![ThinkingLevel::Medium],
            thinking_level: None,
            ttl_seconds: None,
            pricing_convention: lofi_types::PricingConvention::PerToken,
            pricing_field_mappings: lofi_types::PricingFieldMappings::default(),
        };
        let models = parse_auto_models(&p, &am, &payload);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "remote-1");
        assert_eq!(models[0].1.name.as_deref(), Some("Remote One"));
        // api_type_field + mappings override the provider's default API.
        assert_eq!(models[0].1.api, Some(Api::AnthropicMessages));
        assert_eq!(models[0].1.thinking_levels, vec![ThinkingLevel::Medium]);
        assert!(models[0].1.reasoning.unwrap_or(false));
    }

    #[test]
    fn parse_auto_models_reads_pricing_from_configured_paths() {
        // Plexus-style pricing: per-token values under `pricing.prompt`,
        // `pricing.completion`, `pricing.input_cache_read`,
        // `pricing.input_cache_write`. The default mapping reads them; a
        // custom mapping reads from non-standard keys.
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
            api_type_mappings: IndexMap::new(),
            default_api_type: None,
            thinking_levels: vec![],
            thinking_level: None,
            ttl_seconds: None,
            pricing_convention: lofi_types::PricingConvention::PerToken,
            pricing_field_mappings: lofi_types::PricingFieldMappings::default(),
        };
        let models = parse_auto_models(&p, &am, &payload);
        let mc = &models[0].1;
        // Per-token values multiplied by 1M.
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
        let p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: None,
            api_type_mappings: IndexMap::new(),
            default_api_type: None,
            thinking_levels: vec![],
            thinking_level: None,
            ttl_seconds: None,
            pricing_convention: lofi_types::PricingConvention::PerMillion,
            pricing_field_mappings: lofi_types::PricingFieldMappings::default(),
        };
        let models = parse_auto_models(&p, &am, &payload);
        let mc = &models[0].1;
        // Values already per-1M — used as-is, no scaling.
        assert_eq!(mc.input_price, Some(1.5));
        assert_eq!(mc.output_price, Some(3.0));
    }

    #[test]
    fn parse_auto_models_custom_pricing_paths() {
        // A provider whose pricing lives under non-standard keys (e.g.
        // `cost.in`, `cost.out`) is mapped via `pricing_field_mappings`.
        let payload = serde_json::json!({
            "data": [
                {"id": "m", "cost": {"in": "0.000002", "out": "0.000006"}}
            ]
        });
        let p = pcfg(Api::OpenAiCompletions, IndexMap::new());
        let am = AutoModelsConfig {
            enabled: true,
            auth: true,
            models_url: None,
            path: "data".to_string(),
            api_type_field: None,
            api_type_mappings: IndexMap::new(),
            default_api_type: None,
            thinking_levels: vec![],
            thinking_level: None,
            ttl_seconds: None,
            pricing_convention: lofi_types::PricingConvention::PerToken,
            pricing_field_mappings: lofi_types::PricingFieldMappings {
                input: Some("cost.in".to_string()),
                output: Some("cost.out".to_string()),
                cache_read: None,
                cache_write: None,
            },
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
        // discovery_cache_path() resolves under XDG_STATE_HOME on Linux; point
        // it at a temp dir so the test is hermetic and pre-seeds the cache the
        // loader falls back to when the live fetch fails.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_STATE_HOME", dir.path());
        let cache_path = crate::state::discovery_cache_path().unwrap();
        let mut cache: HashMap<String, Vec<(String, ModelConfig)>> = HashMap::new();
        cache.insert(
            "anthropic".to_string(),
            vec![("claude-opus-4".to_string(), mc_named("claude-opus-4", "Claude Opus 4").1)],
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
            api_type_mappings: IndexMap::new(),
            default_api_type: None,
            thinking_levels: vec![],
            thinking_level: None,
            // ttl 0 -> cache always stale -> live fetch attempted -> fails
            // (unroutable port) -> fallback to the pre-seeded cache.
            ttl_seconds: Some(0),
            pricing_convention: lofi_types::PricingConvention::PerToken,
            pricing_field_mappings: lofi_types::PricingFieldMappings::default(),
        });
        providers.insert("anthropic".to_string(), p);
        let cfg = config_with(providers);

        let reg = ModelRegistry::load_async(&cfg).await.unwrap();
        let m = reg.resolve("anthropic/claude-opus-4").unwrap();
        assert_eq!(m.api, Api::AnthropicMessages);
    }
}
