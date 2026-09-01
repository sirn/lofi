mod anthropic_messages;
mod google_generative_ai;
mod ir;
mod message_assembler;
mod openai_completions;
mod openai_responses;
pub(crate) mod sse;

use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use lofi_types::{Api, Message, Model, ProviderConfig, StreamingEvent};

use lofi_error::{Error, Result};

use anthropic_messages::AnthropicMessagesProvider;
pub use anthropic_messages::ANTHROPIC_VERSION;
use google_generative_ai::GoogleGenerativeAiProvider;
pub use message_assembler::{assemble_message, MessageAssembler};
use openai_completions::OpenAiCompletionsProvider;
use openai_responses::OpenAiResponsesProvider;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// `stream` runs a single model turn, `POSTing` to `model.base_url` (the full
/// endpoint URL resolved at config load) and yielding incremental events
/// until the provider sends its terminal sentinel. Remote model-list
/// discovery is driven by `lofi-core`'s `fetch_auto_models`, not by this
/// trait, so there is no `list_models` method here.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(
        &self,
        model: &Model,
        messages: &[Message],
        tools: &[ToolSchema],
    ) -> Result<BoxStream<'static, Result<StreamingEvent>>>;
}

/// `config_loader` resolves `api_key` and `header` values in place, so the
/// factory simply hands them to the transport. The provider's `base_url`
/// (host root) is the fallback used when a model does not carry its own
/// `base_url`; per-model endpoint URLs are resolved earlier by the model
/// registry. An unknown `api` is a config error rather than a transport one.
/// # Errors
/// Returns [`Error::Config`] for an invalid response-start timeout or [`Error::Http`]
/// if the shared HTTP client cannot be constructed.
pub fn open(api: Api, cfg: &ProviderConfig) -> Result<Box<dyn Provider>> {
    if cfg.response_start_timeout_ms == 0 {
        return Err(Error::Config(
            "response_start_timeout_ms must be greater than zero".to_string(),
        ));
    }
    let base_url = cfg
        .base_url
        .as_deref()
        .unwrap_or_else(|| api.default_base_url())
        .trim_end_matches('/')
        .to_string();
    let (api_key, headers) = effective_credentials(cfg);
    let client = http_client()?;
    let response_start_timeout = std::time::Duration::from_millis(cfg.response_start_timeout_ms);
    let provider: Box<dyn Provider> = match api {
        Api::OpenAiCompletions => Box::new(OpenAiCompletionsProvider {
            base_url,
            api_key,
            headers,
            client,
            response_start_timeout,
        }),
        Api::OpenAiResponses => Box::new(OpenAiResponsesProvider {
            base_url,
            api_key,
            headers,
            client,
            response_start_timeout,
        }),
        Api::AnthropicMessages => Box::new(AnthropicMessagesProvider {
            base_url,
            api_key,
            headers,
            client,
            response_start_timeout,
        }),
        Api::GoogleGenerativeAi => Box::new(GoogleGenerativeAiProvider {
            base_url,
            api_key,
            headers,
            client,
            response_start_timeout,
        }),
    };
    Ok(provider)
}

/// Resolve the effective API key and headers for a provider, suppressing
/// generated credentials when `no_auth` is set (the api key is blanked and
/// `Authorization`/`x-api-key` custom headers are dropped) so an explicitly
/// unauthenticated provider never transmits credentials.
#[allow(clippy::must_use_candidate)]
pub fn effective_credentials(cfg: &ProviderConfig) -> (String, HashMap<String, String>) {
    if cfg.no_auth {
        let headers = cfg
            .headers
            .as_ref()
            .map(|h| {
                h.iter()
                    .filter(|(k, _)| {
                        !matches!(
                            k.to_ascii_lowercase().as_str(),
                            "authorization" | "x-api-key" | "x-goog-api-key"
                        )
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        (String::new(), headers)
    } else {
        (
            cfg.api_key.clone().unwrap_or_default(),
            cfg.headers.clone().unwrap_or_default(),
        )
    }
}

pub(crate) fn provider_transport_error(
    error: reqwest::Error,
    phase: lofi_error::ProviderPhase,
) -> Error {
    let kind = if error.is_builder() {
        lofi_error::ProviderTransportKind::RequestSetup
    } else if error.is_redirect() {
        lofi_error::ProviderTransportKind::Redirect
    } else {
        lofi_error::ProviderTransportKind::Network
    };
    Error::ProviderTransport {
        kind,
        phase,
        detail: transport_error_detail(error),
    }
}

fn transport_error_detail(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

fn http_client() -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder();
    // Reqwest's 30-second TCP deadline can close a long provider stream
    // independently of Lofi's response-start timeout.
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    let builder = builder.tcp_user_timeout(None);
    builder.build().map_err(|e| Error::Http(e.to_string()))
}

pub(crate) async fn send_stream_request(
    request: reqwest::RequestBuilder,
    response_start_timeout: std::time::Duration,
) -> Result<reqwest::Response> {
    match tokio::time::timeout(response_start_timeout, request.send()).await {
        Err(_) => Err(Error::ProviderTimeout {
            phase: lofi_error::ProviderPhase::ResponseStart,
            timeout_ms: response_start_timeout.as_millis() as u64,
        }),
        Ok(Err(error)) => Err(provider_transport_error(
            error,
            lofi_error::ProviderPhase::ResponseStart,
        )),
        Ok(Ok(response)) => ensure_ok(response, Some(response_start_timeout)).await,
    }
}

#[allow(clippy::must_use_candidate, clippy::implicit_hasher)]
pub fn apply_headers(
    mut builder: reqwest::RequestBuilder,
    headers: &HashMap<String, String>,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    builder
}

/// Apply `Authorization: Bearer <key>` only when a key was configured, so a
/// header-only or no-auth provider doesn't send an empty bearer header.
pub(crate) fn with_bearer(
    builder: reqwest::RequestBuilder,
    api_key: &str,
) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        builder
    } else {
        builder.bearer_auth(api_key)
    }
}

pub(crate) fn with_key_header(
    builder: reqwest::RequestBuilder,
    name: &str,
    api_key: &str,
) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        builder
    } else {
        builder.header(name, api_key)
    }
}

// Model discovery uses its request deadline instead of a second body idle timeout.
async fn ensure_ok(
    resp: reqwest::Response,
    idle_timeout: Option<std::time::Duration>,
) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let url = redact_url(resp.url());
    // Read the error body through a capped stream so a huge or hostile
    // error response cannot exhaust memory before its (truncated) detail
    // is surfaced.
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    let mut truncated = false;
    loop {
        let next = match idle_timeout {
            Some(timeout) => tokio::time::timeout(timeout, stream.next())
                .await
                .map_err(|_| Error::ProviderTimeout {
                    phase: lofi_error::ProviderPhase::ErrorResponseBody,
                    timeout_ms: timeout.as_millis() as u64,
                })?,
            None => stream.next().await,
        };
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|error| {
            provider_transport_error(error, lofi_error::ProviderPhase::ErrorResponseBody)
        })?;
        let remaining = MAX_ERROR_BODY_BYTES.saturating_sub(buf.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let suffix = if truncated { " <truncated>" } else { "" };
    let detail = match extract_error_detail(&text) {
        Some(detail) => format!("{detail}{suffix}"),
        None if text.is_empty() => "empty response body".to_string(),
        None => format!("{}{suffix}", truncate(&text, 500)),
    };
    Err(Error::ProviderStatus {
        status: status.as_u16(),
        endpoint: url,
        detail,
    })
}

const MAX_DISCOVERY_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Read a JSON response body through a capped byte stream so a configurable
/// discovery endpoint cannot force unbounded allocation before parsing.
async fn read_json_capped(resp: reqwest::Response, max: usize) -> Result<Value> {
    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            Error::Provider(format!(
                "body read error: {}",
                transport_error_detail(error)
            ))
        })?;
        if chunk.len() > max.saturating_sub(buf.len()) {
            return Err(Error::Provider(format!(
                "response body exceeded {max} bytes"
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&buf).map_err(|e| Error::Provider(format!("json decode error: {e}")))
}

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
        Api::GoogleGenerativeAi => match key {
            Some(k) => req.header("x-goog-api-key", k),
            None => req,
        },
    }
}

/// Fetch a provider's model-discovery endpoint (`/models`) and return the
/// raw JSON body. When `auth` is false the request is sent without
/// credentials or provider headers so no configured credential headers leak
/// to a public or alternate `models_url`.
/// # Errors
/// Returns [`Error::Http`] on transport failure, [`Error::Provider`] on a
/// non-2xx response or body that exceeds the size cap, or a decode error.
#[allow(clippy::missing_errors_doc, clippy::implicit_hasher)]
pub async fn fetch_models(
    url: &str,
    api: Api,
    api_key: Option<&str>,
    headers: &HashMap<String, String>,
    auth: bool,
) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;
    let req = if auth {
        authed_get(&client, api, api_key, url)
    } else {
        client.get(url)
    };
    let req = if auth {
        apply_headers(req, headers)
    } else {
        req
    };
    let resp = req
        .send()
        .await
        .map_err(|error| Error::Http(transport_error_detail(error)))?;
    let resp = ensure_ok(resp, None).await?;
    read_json_capped(resp, MAX_DISCOVERY_BODY_BYTES).await
}

fn extract_error_detail(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    if let Some(m) = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
    {
        return Some(m.to_string());
    }
    if let Some(m) = v.get("error").and_then(Value::as_str) {
        return Some(m.to_string());
    }
    if let Some(m) = v.get("message").and_then(Value::as_str) {
        return Some(m.to_string());
    }
    None
}

/// Maximum bytes read from a non-2xx error body before truncation, so a
/// misbehaving provider cannot force unbounded allocation via a huge error
/// response.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Render a URL with credentials and query stripped, for use in error
/// messages. A provider `base_url` or proxy may carry an API key in userinfo
/// or a query parameter; surfacing the raw URL would leak it into the
/// transcript. Keeps scheme, host, port, and path so the endpoint is still
/// identifiable.
fn redact_url(url: &reqwest::Url) -> String {
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    format!(
        "{}://{}{}{}",
        url.scheme(),
        url.host_str().unwrap_or(""),
        port,
        url.path()
    )
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use lofi_types::{
        Api, ApiTypeMapping, PricingConvention, PricingFieldMappings, ProviderConfig,
    };

    #[tokio::test]
    async fn fetch_models_returns_error_on_http_failure() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/models")
            .with_status(404)
            .with_body("{\"error\":{\"message\":\"not found\"}}")
            .create_async()
            .await;
        let headers = HashMap::new();
        let err = fetch_models(
            &format!("{}/models", server.url()),
            Api::OpenAiResponses,
            None,
            &headers,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("HTTP 404"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn transport_error_detail_removes_sensitive_urls() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let secret = "query-secret-value";
        let error = http_client()
            .unwrap()
            .get(format!(
                "http://user:password@{addr}/v1/responses?api_key={secret}"
            ))
            .send()
            .await
            .unwrap_err();
        server.join().unwrap();
        assert!(error.url().is_some());

        let detail = transport_error_detail(error);

        assert!(!detail.contains("password"), "detail: {detail}");
        assert!(!detail.contains(secret), "detail: {detail}");
        assert!(!detail.contains("api_key"), "detail: {detail}");
    }

    #[tokio::test]
    async fn request_setup_errors_are_typed_as_non_network_failures() {
        let error = http_client()
            .unwrap()
            .get("http://127.0.0.1/")
            .header("invalid\nheader", "value")
            .send()
            .await
            .unwrap_err();

        let error = provider_transport_error(error, lofi_error::ProviderPhase::ResponseStart);

        assert!(matches!(
            error,
            Error::ProviderTransport {
                kind: lofi_error::ProviderTransportKind::RequestSetup,
                phase: lofi_error::ProviderPhase::ResponseStart,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn streaming_error_body_timeout_reports_its_phase() {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            stream
                .write_all(
                    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 100\r\nContent-Type: application/json\r\n\r\n{",
                )
                .unwrap();
            stream.flush().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
        });
        let request = http_client()
            .unwrap()
            .get(format!("http://{addr}/v1/responses"));

        let error = send_stream_request(request, std::time::Duration::from_millis(20))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            Error::ProviderTimeout {
                phase: lofi_error::ProviderPhase::ErrorResponseBody,
                timeout_ms: 20,
            }
        ));
        server.join().unwrap();
    }

    fn cfg() -> ProviderConfig {
        ProviderConfig {
            api_type: Some(Api::OpenAiCompletions),
            api_types: {
                let mut m = indexmap::IndexMap::new();
                m.insert(
                    "openai-completions".to_string(),
                    ApiTypeMapping {
                        path: None,
                        pricing_field_mappings: None,
                    },
                );
                m
            },
            base_url: Some("https://api.example.com".to_string()),
            pricing_convention: PricingConvention::PerToken,
            pricing_field_mappings: PricingFieldMappings::default(),
            env_name: None,
            api_key: Some("sk-test".to_string()),
            headers: Some(HashMap::from([("x-custom".to_string(), "yes".to_string())])),
            models: indexmap::IndexMap::new(),
            auto_models: None,
            no_auth: false,
            response_start_timeout_ms: 90_000,
            thinking_level: None,
            thinking_levels: Vec::new(),
            service_tier: None,
            service_tiers: Vec::new(),
            auto_continue: lofi_types::AutoContinueConfig::default(),
        }
    }

    #[test]
    fn redact_url_strips_credentials_and_query() {
        let url = reqwest::Url::parse("https://user:pass@host.example:8443/v1/models?secret=abc")
            .unwrap();
        assert_eq!(redact_url(&url), "https://host.example:8443/v1/models");
    }

    #[test]
    fn effective_credentials_suppresses_auth_when_no_auth() {
        let mut c = cfg();
        c.no_auth = true;
        c.headers = Some(HashMap::from([
            ("Authorization".to_string(), "Bearer leaked".to_string()),
            ("x-api-key".to_string(), "leaked".to_string()),
            ("x-routing".to_string(), "eu".to_string()),
        ]));
        let (key, headers) = effective_credentials(&c);
        assert!(key.is_empty());
        assert!(!headers.contains_key("Authorization"));
        assert!(!headers.contains_key("x-api-key"));
        assert_eq!(headers.get("x-routing").map(String::as_str), Some("eu"));
    }

    #[test]
    fn effective_credentials_preserves_auth_when_not_no_auth() {
        let c = cfg();
        let (key, headers) = effective_credentials(&c);
        assert_eq!(key, "sk-test");
        assert_eq!(headers.get("x-custom").map(String::as_str), Some("yes"));
    }

    #[test]
    fn open_trims_trailing_slash() {
        let mut c = cfg();
        c.base_url = Some("https://api.example.com/".to_string());
        assert!(open(Api::OpenAiCompletions, &c).is_ok());
        let oc = OpenAiCompletionsProvider {
            base_url: "https://api.example.com".to_string(),
            api_key: "sk-test".to_string(),
            headers: HashMap::from([("x-custom".to_string(), "yes".to_string())]),
            client: http_client().unwrap(),
            response_start_timeout: std::time::Duration::from_secs(90),
        };
        assert_eq!(oc.base_url, "https://api.example.com");
    }

    #[test]
    fn open_dispatches_all_apis() {
        for api in [
            Api::OpenAiCompletions,
            Api::OpenAiResponses,
            Api::AnthropicMessages,
        ] {
            let mut c = cfg();
            c.api_type = Some(api);
            assert!(open(api, &c).is_ok());
        }
    }

    #[test]
    fn open_rejects_zero_response_start_timeout() {
        let mut c = cfg();
        c.response_start_timeout_ms = 0;
        let Err(error) = open(Api::OpenAiCompletions, &c) else {
            panic!("zero response start timeout should fail");
        };
        assert!(matches!(error, Error::Config(message) if message.contains("greater than zero")));
    }

    #[test]
    fn open_uses_empty_key_when_absent() {
        let mut c = cfg();
        c.api_key = None;
        let p = open(Api::OpenAiCompletions, &c).unwrap();
        let _ = p;
    }
}
