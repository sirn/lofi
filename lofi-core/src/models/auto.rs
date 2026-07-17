//! Auto-model discovery: fetch, parse, inject, and cache remote model lists.
//!
//! See the parent module docs for the full design. Each provider with an
//! `auto_models` block fetches an OpenAI-style models endpoint at startup;
//! entries are parsed into `ModelConfig`s, injected into the provider's
//! `models` map (static wins on id collision), and cached under
//! `<state>/discovery.json` with a per-block TTL.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime};

use indexmap::IndexMap;
use lofi_types::{
    Api, AutoModelsConfig, ModelConfig, PricingConvention, ProviderConfig, ThinkingLevel,
};
use serde_json::Value;

use lofi_error::{Error, Result};
use lofi_providers::apply_headers;
use lofi_providers::ANTHROPIC_VERSION;

use super::resolve_model_base_url;
/// Default cache freshness for auto-discovered model lists (5 minutes).
pub(super) const DEFAULT_AUTO_TTL_SECS: u64 = 300;

/// Insert auto-discovered `(id, ModelConfig)` entries into a provider's
/// `models` map. A static entry wins on id collision, but any field it leaves
/// unset is filled from the discovered entry — so a static entry that only
/// pins `thinking_levels` still inherits `context_window`, pricing, and other
/// metadata the remote endpoint reports. New ids are appended in discovery
/// order.
pub(super) fn inject_discovered(
    models: &mut IndexMap<String, ModelConfig>,
    entries: &[(String, ModelConfig)],
) {
    for (id, mc) in entries {
        match models.entry(id.clone()) {
            indexmap::map::Entry::Occupied(mut e) => {
                fill_missing(e.get_mut(), mc);
            }
            indexmap::map::Entry::Vacant(e) => {
                e.insert(mc.clone());
            }
        }
    }
}

/// Copy each unset field of `dst` from `src`, so a static entry inherits
/// metadata the remote endpoint reports without the user re-declaring it.
/// Explicit static values are preserved.
fn fill_missing(dst: &mut ModelConfig, src: &ModelConfig) {
    if dst.name.is_none() {
        dst.name.clone_from(&src.name);
    }
    if dst.api_type.is_none() {
        dst.api_type.clone_from(&src.api_type);
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
        dst.thinking_levels.clone_from(&src.thinking_levels);
    }
    if dst.thinking_level.is_none() {
        dst.thinking_level = src.thinking_level;
    }
    if dst.base_url.is_none() {
        dst.base_url.clone_from(&src.base_url);
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
    if dst.per_request_price.is_none() {
        dst.per_request_price = src.per_request_price;
    }
}

/// Fetch a provider's auto-discovered model list and map it to
/// `(id, ModelConfig)` entries. When `auth` is false the request is sent
/// without credentials (public list endpoints that reject auth headers).
///
/// The models endpoint URL is `am.models_url` when set, otherwise it is
/// derived from the provider's default api-type mapping: the first path
/// segment of the endpoint `path` (the API version prefix, e.g. `/v1`)
/// joined onto the provider `base_url` with `/models` appended — so a
/// provider whose chat endpoint is `/v1/chat/completions` discovers at
/// `/v1/models` without an explicit `models_url`.
pub(super) async fn fetch_auto_models(
    pcfg: &ProviderConfig,
    am: &AutoModelsConfig,
) -> Result<Vec<(String, ModelConfig)>> {
    let url = if let Some(u) = &am.models_url {
        u.clone()
    } else {
        let base = pcfg
            .base_url
            .as_deref()
            .unwrap_or_else(|| pcfg.default_api().default_base_url());
        format!(
            "{}{}",
            base.trim_end_matches('/'),
            default_models_path(pcfg)
        )
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;
    let (api_key, headers) = lofi_providers::effective_credentials(pcfg);
    let key_arg = if api_key.is_empty() {
        None
    } else {
        Some(&api_key[..])
    };
    let req = if am.auth {
        authed_get(&client, pcfg.default_api(), key_arg, &url)
    } else {
        client.get(&url)
    };
    let req = apply_headers(req, &headers);

    let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
    let resp = lofi_providers::ensure_ok(resp).await?;
    let body: Value =
        lofi_providers::read_json_capped(resp, lofi_providers::MAX_DISCOVERY_BODY_BYTES).await?;

    Ok(parse_auto_models(pcfg, am, &body))
}

/// Default models-endpoint path derived from the provider's default api-type
/// mapping: the first segment of the endpoint `path` (the version prefix)
/// plus `/models`. For the built-in defaults this yields `/v1/models`.
fn default_models_path(pcfg: &ProviderConfig) -> String {
    let path = pcfg.resolve_path(None);
    let first = path.split('/').nth(1).unwrap_or("");
    if first.is_empty() {
        "/models".to_string()
    } else {
        format!("/{first}/models")
    }
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

/// Read a JSON value as `f64`, accepting either a number or a numeric
/// string (some providers return pricing as strings like `"5e-7"`).
fn json_num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<f64>().ok()))
}

/// Read a JSON value as `u64`, accepting either a number or a numeric
/// string. Floats are floored.
fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_f64().map(|f| f as u64))
        .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
}

/// Navigate `body` to the array at `am.path` and map each entry to an
/// `(id, ModelConfig)` pair. The per-model `api_type` is read from the
/// field named by `am.api_type_field` (e.g. `preferred_api`), translated
/// through `am.api_type_mappings` (remote vocabulary → internal [`Api`] id),
/// and stored as the model's `api_type` key so it resolves through the
/// provider's `api_types` table exactly like a static model's override.
/// When `api_type_field` is unset or the remote value has no mapping, the
/// model inherits the provider's default `api_type`.
///
/// Pricing is read via the resolved api-type's `pricing_field_mappings`
/// (falling back to the provider-level default) and scaled by the provider's
/// `pricing_convention`. Non-pricing fields are read via `am.field_mappings`.
/// `reasoning` and `supports_image` are inferred from the entry's
/// `supported_parameters` array; discovered models inherit
/// `thinking_levels`/`thinking_level` from the `auto_models` config (lofi
/// has no built-in model catalog), defaulting to the standard four-level
/// ladder when the endpoint reports `reasoning` support.
pub(super) fn parse_auto_models(
    pcfg: &ProviderConfig,
    am: &AutoModelsConfig,
    body: &Value,
) -> Vec<(String, ModelConfig)> {
    let Some(arr) = navigate(body, &am.path).and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            continue;
        };
        // `preferred_api` may be a string or an array (some providers return
        // `["responses"]`); take the first element either way so each model
        // routes to its real upstream API instead of the block default.
        let remote_api = am
            .api_type_field
            .as_deref()
            .and_then(|field| entry.get(field))
            .and_then(|v| {
                v.as_str().map(str::to_string).or_else(|| {
                    v.as_array()
                        .and_then(|a| a.first())
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
            });
        // Translate the remote vocabulary to an internal api id; an
        // unmapped value falls back to the provider's default.
        let api_type = remote_api
            .as_deref()
            .and_then(|r| am.api_type_mappings.get(r))
            .map(|api| api.id().to_string());
        let fields = pcfg.resolve_pricing_fields(api_type.as_deref());
        let base_url = resolve_model_base_url(pcfg, api_type.as_deref());
        let display_name = navigate(entry, &am.field_mappings.name)
            .and_then(Value::as_str)
            .map(str::to_string);
        let context_window = navigate(entry, &am.field_mappings.context_window).and_then(json_u64);
        let max_tokens = navigate(entry, &am.field_mappings.max_tokens).and_then(json_u64);
        let scale = match pcfg.pricing_convention {
            PricingConvention::PerToken => 1_000_000.0,
            PricingConvention::PerMillion => 1.0,
        };
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
        let per_request_price = price(&fields.per_request);
        let supported_params = entry.get("supported_parameters").and_then(Value::as_array);
        let supports_reasoning =
            supported_params.is_some_and(|a| a.iter().any(|p| p.as_str() == Some("reasoning")));
        let supports_image =
            supported_params.is_some_and(|a| a.iter().any(|p| p.as_str() == Some("image")));
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
                api_type,
                reasoning: Some(!thinking_levels.is_empty()),
                supports_image: Some(supports_image),
                context_window,
                max_tokens,
                thinking_levels,
                thinking_level: am.thinking_level,
                base_url: Some(base_url),
                input_price,
                output_price,
                cache_read_price,
                cache_write_price,
                per_request_price,
            },
        ));
    }
    out
}

/// Join a mapping `path` (e.g. `/v1/chat/completions`) onto a provider
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
pub(super) fn cache_age(path: &Path) -> Result<Option<Duration>> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Io(e)),
    };
    let mtime = meta.modified()?;
    let elapsed = SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::ZERO);
    Ok(Some(elapsed))
}

/// Write the per-provider auto-discovered model lists to `path` as pretty
/// JSON. Each entry is an `[id, ModelConfig]` tuple so the id (which is the
/// map key in the provider's `models` table) round-trips with its config.
pub(super) fn write_auto_cache(
    path: &Path,
    cache: &HashMap<String, Vec<(String, ModelConfig)>>,
) -> Result<()> {
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
pub(super) fn read_auto_cache(path: &Path) -> Result<HashMap<String, Vec<(String, ModelConfig)>>> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(HashMap::new()),
        Ok(s) => Ok(serde_json::from_str(&s)
            .map_err(|e| Error::State(format!("auto-models cache decode error: {e}")))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(Error::Io(e)),
    }
}
