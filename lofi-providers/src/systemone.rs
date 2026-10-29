//! The systemone decision endpoint: POST /v1/systemone.
//!
//! A decision model answers typed questions about a state; it never
//! generates text, so it cannot serve a chat turn. `open` rejects it as a
//! chat provider and only `open_decision` builds a client for it.

use std::collections::BTreeMap;

use async_trait::async_trait;
use lofi_error::{Error, Result};
use lofi_types::{Api, DecisionQuestion, ProviderConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{apply_headers, effective_credentials, http_client, with_bearer};

/// One decision request: a state plus named questions. Both are
/// provider-neutral: the policy layer writes the state and the questions,
/// and this transport sends them as-is.
#[derive(Debug, Serialize)]
pub struct DecisionRequest {
    /// The decision model to run, for example `jev-latest`.
    pub model: String,
    /// The content every question refers to.
    pub state: Value,
    /// The named questions, keyed by the name the answers come back under.
    pub questions: BTreeMap<String, DecisionQuestion>,
}

/// The tokens one decision call billed. Decision output is free today, so
/// the output count is informational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
pub struct DecisionUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DecisionResponse {
    /// The versioned model that answered; may differ from the alias asked
    /// for, so log it rather than the request's name.
    pub model: String,
    pub answers: BTreeMap<String, lofi_types::DecisionAnswer>,
    #[serde(default)]
    pub usage: DecisionUsage,
}

/// A non-chat endpoint that answers typed questions about a state. Sits
/// beside `Provider` because a decision model cannot stream a turn, and a
/// `Provider` cannot carry a question set. Also the seam the auto-mode
/// tests use to answer decisions without a server.
#[async_trait]
pub trait Decision: Send + Sync {
    /// Evaluate the request's questions against its state in one call.
    /// # Errors
    /// Returns a provider error when the endpoint is unreachable, answers
    /// with a non-success status, or returns a malformed body.
    async fn decide(&self, request: &DecisionRequest) -> Result<DecisionResponse>;
}

pub(crate) struct SystemOneClient {
    url: String,
    api_key: String,
    headers: std::collections::HashMap<String, String>,
    client: reqwest::Client,
}

/// The largest response body read into memory. A decision answer is a few
/// small JSON objects; anything larger is a malfunctioning endpoint, and
/// an unbounded read would let one exhaust memory inside the gate's
/// deadline.
const MAX_BODY_BYTES: usize = 64 * 1024;

#[async_trait]
impl Decision for SystemOneClient {
    async fn decide(&self, request: &DecisionRequest) -> Result<DecisionResponse> {
        let mut response = self.post(&self.url, request).await?;

        // One retry on rate limiting, waiting what the endpoint asks for.
        // The caller's deadline bounds the whole call, so a Retry-After
        // longer than the budget surfaces as a timeout, not a hang.
        if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            if let Some(wait) = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
                response = self.post(&self.url, request).await?;
            }
        }

        let status = response.status();
        let body = read_body_capped(&mut response).await?;
        if !status.is_success() {
            return Err(Error::Provider(format!(
                "systemone: endpoint returned {status}: {}",
                clip(&body)
            )));
        }
        serde_json::from_str(&body)
            .map_err(|e| Error::Provider(format!("systemone: malformed response: {e}")))
    }
}

impl SystemOneClient {
    async fn post(&self, url: &str, request: &DecisionRequest) -> Result<reqwest::Response> {
        let builder = apply_headers(
            with_bearer(self.client.post(url).json(request), &self.api_key),
            &self.headers,
        );
        builder.send().await.map_err(|e| {
            super::provider_transport_error(e, lofi_error::ProviderPhase::ResponseStart)
        })
    }
}

fn clip(text: &str) -> String {
    text.chars().take(400).collect()
}

/// Read the body up to 64 KiB, so neither a success nor an error body
/// can grow without bound.
async fn read_body_capped(response: &mut reqwest::Response) -> Result<String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| Error::Provider(format!("systemone: reading body failed: {e}")))?
    {
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(Error::Provider(
                "systemone: the response body exceeds 64 KiB".to_string(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Open a decision client for a provider whose `api` is a decision API.
///
/// `model_base_url` is the complete endpoint a registry entry carries:
/// the registry joins the api's default path onto the provider root, the
/// same way the chat providers' model base URL works. Only when no model
/// URL is given is the path joined onto the provider root here. The client
/// is built once and reused, so the first call does not pay a TLS
/// handshake out of the gate's deadline.
/// # Errors
/// Returns a config error when `api` cannot answer decisions or when
/// the provider has neither an api key nor an auth header, and propagates
/// client construction failures.
pub fn open_decision(
    api: Api,
    cfg: &ProviderConfig,
    model_base_url: Option<&str>,
) -> Result<Box<dyn Decision>> {
    if !api.is_decision() {
        return Err(Error::Config(format!(
            "{} is not a decision API",
            api.as_str()
        )));
    }
    let url = if let Some(url) = model_base_url {
        url.trim_end_matches('/').to_string()
    } else {
        let root = cfg
            .base_url
            .clone()
            .unwrap_or_else(|| api.default_base_url().to_string())
            .trim_end_matches('/')
            .to_string();
        format!("{root}{}", api.default_path())
    };
    let (api_key, headers) = effective_credentials(cfg);
    // A gate without credentials answers 401 on every call and turns every
    // gated command into a prompt, so refuse at startup like the chat path.
    let auth_header = headers.keys().any(|name| {
        matches!(
            name.to_ascii_lowercase().as_str(),
            "authorization" | "x-api-key" | "x-goog-api-key"
        )
    });
    if !cfg.no_auth && api_key.is_empty() && !auth_header {
        return Err(Error::Config(
            "systemone: the provider has no api key and no auth header".to_string(),
        ));
    }
    Ok(Box::new(SystemOneClient {
        url,
        api_key,
        headers,
        client: http_client()?,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use lofi_types::DecisionAnswer;

    fn systemone_cfg(api: lofi_types::Api) -> ProviderConfig {
        ProviderConfig {
            api_type: Some(api),
            base_url: Some("https://api.example.com".to_string()),
            api_key: Some("sk-test".to_string()),
            response_start_timeout_ms: 1000,
            ..serde_json::from_str::<ProviderConfig>("{}").unwrap()
        }
    }

    fn response(json: &str) -> DecisionResponse {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn parses_all_three_primitives() {
        let r = response(
            r#"{"model":"jev-1.14.0","answers":{
                "destructive":{"type":"noul","noul":0.98},
                "scope":{"type":"choice","choice":"project","probabilities":{"project":1},"confidence":0.9},
                "undo_cost":{"type":"score","score":1.4,"probabilities":{"0":0.1,"1":0.6,"2":0.3},"confidence":0.7}
            },"usage":{"input_tokens":120,"output_tokens":12}}"#,
        );
        assert_eq!(r.answers["destructive"].noul(), Some(0.98));
        let lofi_types::DecisionAnswer::Choice { probabilities, .. } = &r.answers["scope"] else {
            panic!("scope is a choice answer");
        };
        assert!((probabilities["project"] - 1.0).abs() < f64::EPSILON);
        assert_eq!(r.answers["scope"].confidence(), Some(0.9));
        assert!(matches!(
            r.answers["undo_cost"],
            DecisionAnswer::Score { .. }
        ));
        assert_eq!(r.usage.input_tokens, 120);
        assert_eq!(r.usage.output_tokens, 12);
    }

    #[test]
    fn integers_in_probabilities_still_parse() {
        let r = response(
            r#"{"model":"m","answers":{
                "scope":{"type":"choice","choice":"mixed","probabilities":{"mixed":1,"negative":0},"confidence":1}
            }}"#,
        );
        let lofi_types::DecisionAnswer::Choice { probabilities, .. } = &r.answers["scope"] else {
            panic!("scope is a choice answer");
        };
        assert!((probabilities["mixed"] - 1.0).abs() < f64::EPSILON);
        assert_eq!(r.answers["scope"].confidence(), Some(1.0));
    }

    #[test]
    fn unknown_answer_type_does_not_fail_the_response() {
        let r = response(
            r#"{"model":"m","answers":{
                "destructive":{"type":"noul","noul":0.5},
                "future":{"type":"box","value":1}
            }}"#,
        );
        assert_eq!(r.answers["future"], DecisionAnswer::Unknown);
        assert!(!r.answers["future"].is_valid());
        assert_eq!(r.answers["destructive"].noul(), Some(0.5));
    }

    #[test]
    fn missing_answer_and_wrong_type_read_as_invalid() {
        let r = response(
            r#"{"model":"m","answers":{
                "destructive":{"type":"choice","choice":"x","confidence":0.5}
            }}"#,
        );
        assert_eq!(r.answers["destructive"].noul(), None);
        assert_eq!(r.answers.get("missing"), None);
        assert_eq!(r.answers["destructive"].confidence(), Some(0.5));
        assert_eq!(r.usage, DecisionUsage::default());
    }

    #[test]
    fn out_of_range_answers_are_invalid() {
        let r = response(
            r#"{"model":"m","answers":{
                "destructive":{"type":"noul","noul":1.5},
                "scope":{"type":"choice","choice":"remote","probabilities":{"remote":2},"confidence":0.9}
            }}"#,
        );
        assert!(!r.answers["destructive"].is_valid());
        assert!(!r.answers["scope"].is_valid());
    }

    #[test]
    fn serializes_the_documented_request_shape() {
        let mut questions = BTreeMap::new();
        questions.insert(
            "billing".to_string(),
            DecisionQuestion::Noul {
                instructions: "Is this about billing?".to_string(),
                criteria: None,
            },
        );
        let request = DecisionRequest {
            model: "jev-latest".to_string(),
            state: serde_json::json!({"command": "ls"}),
            questions,
        };
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["model"], "jev-latest");
        assert_eq!(body["state"]["command"], "ls");
        assert_eq!(body["questions"]["billing"]["type"], "noul");
        assert_eq!(
            body["questions"]["billing"]["instructions"],
            "Is this about billing?"
        );
        assert!(body["questions"]["billing"].get("criteria").is_none());
    }

    #[test]
    fn noul_criteria_serialize_as_true_and_false() {
        let q = DecisionQuestion::Noul {
            instructions: "Is it harmful?".to_string(),
            criteria: Some(lofi_types::NoulCriteria {
                yes: "Data is lost".to_string(),
                no: "Nothing is lost".to_string(),
            }),
        };
        let body = serde_json::to_value(&q).unwrap();
        assert_eq!(body["criteria"]["true"], "Data is lost");
        assert_eq!(body["criteria"]["false"], "Nothing is lost");
    }

    #[test]
    fn open_decision_refuses_a_chat_api() {
        let cfg = systemone_cfg(lofi_types::Api::OpenAiCompletions);
        assert!(open_decision(lofi_types::Api::OpenAiCompletions, &cfg, None).is_err());
    }

    #[test]
    fn open_decision_accepts_the_decision_api() {
        let cfg = systemone_cfg(lofi_types::Api::SystemOne);
        assert!(open_decision(lofi_types::Api::SystemOne, &cfg, None).is_ok());
        assert!(open_decision(
            lofi_types::Api::SystemOne,
            &cfg,
            Some("https://proxy.example.com/v1/systemone")
        )
        .is_ok());
    }

    #[test]
    fn open_decision_accepts_header_only_credentials_and_no_auth() {
        let header_only = ProviderConfig {
            api_key: None,
            headers: Some(std::collections::HashMap::from([(
                "x-api-key".to_string(),
                "k".to_string(),
            )])),
            ..serde_json::from_str::<ProviderConfig>("{}").unwrap()
        };
        assert!(open_decision(lofi_types::Api::SystemOne, &header_only, None).is_ok());

        let no_auth = ProviderConfig {
            no_auth: true,
            ..serde_json::from_str::<ProviderConfig>("{}").unwrap()
        };
        assert!(open_decision(lofi_types::Api::SystemOne, &no_auth, None).is_ok());
    }

    #[test]
    fn chat_open_refuses_the_decision_api() {
        let cfg = systemone_cfg(lofi_types::Api::SystemOne);
        let err = match crate::open(lofi_types::Api::SystemOne, &cfg) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a decision API must not open as a chat provider"),
        };
        assert!(err.contains("decision API"), "{err}");
    }
}
