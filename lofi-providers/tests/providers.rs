#![allow(clippy::unwrap_used)]

use futures::StreamExt;
use lofi_providers::assemble_message;
use lofi_providers::open;
use lofi_types::{
    Api, ContentBlock, Message, Model, PartSignatureFormat, PricingConvention,
    PricingFieldMappings, PromptKind, ProviderConfig, Role, StreamingEvent, Usage,
};

fn user_msg() -> Message {
    Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: "hi".to_string(),
        }],
        kind: PromptKind::default(),
    }
}

fn model_for(api: Api, base_url: &str) -> Model {
    let path = match api {
        Api::OpenAiCompletions => "/v1/chat/completions",
        Api::OpenAiResponses => "/v1/responses",
        Api::AnthropicMessages => "/v1/messages",
        Api::GoogleGenerativeAi => "/v1beta",
        Api::SystemOne => "/v1/systemone",
    };
    Model {
        id: "test-model".to_string(),
        name: "test-model".to_string(),
        provider: "test".to_string(),
        api,
        reasoning: false,
        thinking: lofi_types::ThinkingLevel::Off,
        service_tier: lofi_types::ServiceTier::Auto,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: Some(format!("{base_url}{path}")),
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    }
}

fn cfg(api: Api, base_url: String) -> ProviderConfig {
    ProviderConfig {
        api_type: Some(api),
        api_types: indexmap::IndexMap::new(),
        base_url: Some(base_url),
        pricing_convention: PricingConvention::PerToken,
        pricing_field_mappings: PricingFieldMappings::default(),
        env_name: None,
        api_key: Some("dummy-key".to_string()),
        headers: None,
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

async fn collect(
    stream: futures::stream::BoxStream<'static, lofi_error::Result<StreamingEvent>>,
) -> Vec<StreamingEvent> {
    let mut out = Vec::new();
    let mut s = stream;
    while let Some(ev) = s.next().await {
        out.push(ev.unwrap());
    }
    out
}

#[tokio::test]
async fn openai_chat_completions_maps_canned_stream() {
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );

    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = open(
        Api::OpenAiCompletions,
        &cfg(Api::OpenAiCompletions, server.url()),
    )
    .unwrap();
    let stream = provider
        .stream(
            &model_for(Api::OpenAiCompletions, &server.url()),
            &[user_msg()],
            &[],
        )
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            StreamingEvent::TextDelta("Hello".to_string()),
            StreamingEvent::TextDelta(" world".to_string()),
            StreamingEvent::Done {
                usage: Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                stop_reason: Some(lofi_types::StopReason::EndTurn),
            },
        ]
    );

    let assembled = assemble_message(&events);
    assert_eq!(assembled.role, Role::Assistant);
    assert_eq!(
        assembled.blocks,
        vec![ContentBlock::Text {
            text: "Hello world".to_string()
        }]
    );
}

#[tokio::test]
async fn openai_responses_maps_canned_stream() {
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hi\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"!\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":4}}}\n\n",
        "data: [DONE]\n\n",
    );

    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/v1/responses")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = open(
        Api::OpenAiResponses,
        &cfg(Api::OpenAiResponses, server.url()),
    )
    .unwrap();
    let stream = provider
        .stream(
            &model_for(Api::OpenAiResponses, &server.url()),
            &[user_msg()],
            &[],
        )
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            StreamingEvent::TextDelta("Hi".to_string()),
            StreamingEvent::TextDelta("!".to_string()),
            StreamingEvent::Done {
                usage: Usage {
                    input_tokens: 3,
                    output_tokens: 4,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                stop_reason: Some(lofi_types::StopReason::EndTurn),
            },
        ]
    );

    let assembled = assemble_message(&events);
    assert_eq!(
        assembled.blocks,
        vec![ContentBlock::Text {
            text: "Hi!".to_string()
        }]
    );
}

#[tokio::test]
async fn anthropic_messages_maps_canned_stream() {
    let body = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\"}\n\n",
        "event: content_block_start\n",
        "data: {\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_1\",\"name\":\"exec\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Done\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"code\\\":\\\"1+1\\\"}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":10}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );

    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/v1/messages")
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = open(
        Api::AnthropicMessages,
        &cfg(Api::AnthropicMessages, server.url()),
    )
    .unwrap();
    let stream = provider
        .stream(
            &model_for(Api::AnthropicMessages, &server.url()),
            &[user_msg()],
            &[],
        )
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            StreamingEvent::ToolUseStart {
                id: "tool_1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::TextDelta("Done".to_string()),
            StreamingEvent::ToolUseInputDelta {
                id: "tool_1".to_string(),
                delta: "{\"code\":\"1+1\"}".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "tool_1".to_string(),
            },
            StreamingEvent::Done {
                usage: Usage {
                    input_tokens: 0,
                    output_tokens: 10,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                stop_reason: None,
            },
        ]
    );

    let assembled = assemble_message(&events);
    assert_eq!(assembled.blocks.len(), 2);
    assert_eq!(
        assembled.blocks[0],
        ContentBlock::ToolUse {
            id: "tool_1".to_string(),
            name: "exec".to_string(),
            input: serde_json::json!({"code": "1+1"}),
        }
    );
    assert_eq!(
        assembled.blocks[1],
        ContentBlock::Text {
            text: "Done".to_string()
        }
    );
}

#[tokio::test]
async fn google_generative_ai_maps_canned_stream() {
    let body = concat!(
        r#"data: {"candidates":[{"content":{"parts":[{"text":"Checking","thought":true}]}}]}

"#,
        r#"data: {"candidates":[{"content":{"parts":[{"functionCall":{"id":"call_1","name":"exec","args":{"code":"1+1"}},"thoughtSignature":"c2ln"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":8,"cachedContentTokenCount":2,"candidatesTokenCount":3,"thoughtsTokenCount":4}}

"#,
    );

    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock(
            "POST",
            "/v1beta/models/test-model:streamGenerateContent?alt=sse",
        )
        .match_header("x-goog-api-key", "dummy-key")
        .match_body(mockito::Matcher::PartialJson(serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
        })))
        .with_status(200)
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;

    let provider = open(
        Api::GoogleGenerativeAi,
        &cfg(Api::GoogleGenerativeAi, server.url()),
    )
    .unwrap();
    let stream = provider
        .stream(
            &model_for(Api::GoogleGenerativeAi, &server.url()),
            &[user_msg()],
            &[],
        )
        .await
        .unwrap();
    let events = collect(stream).await;
    mock.assert_async().await;

    assert_eq!(
        events,
        vec![
            StreamingEvent::ThinkingDelta("Checking".to_string()),
            StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputDelta {
                id: "call_1".to_string(),
                delta: "{\"code\":\"1+1\"}".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "call_1".to_string(),
            },
            StreamingEvent::PartSignature {
                provider: "test".to_string(),
                model: "test-model".to_string(),
                format: PartSignatureFormat::Google,
                target: Some("call_1".to_string()),
                signature: "c2ln".to_string(),
            },
            StreamingEvent::Done {
                usage: Usage {
                    input_tokens: 6,
                    output_tokens: 7,
                    cache_read_tokens: 2,
                    cache_write_tokens: 0,
                },
                stop_reason: Some(lofi_types::StopReason::EndTurn),
            },
        ]
    );
}

fn decision_request() -> lofi_providers::DecisionRequest {
    let mut questions = std::collections::BTreeMap::new();
    questions.insert(
        "destructive".to_string(),
        lofi_types::DecisionQuestion::Noul {
            instructions: "Is it destructive?".to_string(),
            criteria: None,
        },
    );
    lofi_providers::DecisionRequest {
        model: "jev-latest".to_string(),
        state: serde_json::json!({"command": "ls"}),
        questions,
    }
}

const DECISION_BODY: &str = concat!(
    r#"{"model":"jev-1.14.0","answers":"#,
    r#"{"destructive":{"type":"noul","noul":0.1}},"#,
    r#""usage":{"input_tokens":10,"output_tokens":1}}"#
);

/// The registry hands the model's endpoint as a complete URL; the client
/// must not append the default path onto it again.
#[tokio::test]
async fn systemone_posts_to_the_model_endpoint_without_doubling_the_path() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/systemone")
        .match_header("authorization", "Bearer dummy-key")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(DECISION_BODY)
        .create_async()
        .await;

    let endpoint = model_for(Api::SystemOne, &server.url()).base_url.unwrap();
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    let response = client.decide(&decision_request()).await.unwrap();
    assert_eq!(response.model, "jev-1.14.0");
    mock.assert_async().await;
}

#[tokio::test]
async fn systemone_joins_the_default_path_when_no_model_endpoint_is_given() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/v1/systemone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(DECISION_BODY)
        .create_async()
        .await;

    let client =
        lofi_providers::open_decision(Api::SystemOne, &cfg(Api::SystemOne, server.url()), None)
            .unwrap();
    client.decide(&decision_request()).await.unwrap();
    mock.assert_async().await;
}

#[tokio::test]
async fn systemone_retries_one_rate_limit() {
    let mut server = mockito::Server::new_async().await;
    let limited = server
        .mock("POST", "/v1/systemone")
        .with_status(429)
        .with_header("retry-after", "0")
        .create_async()
        .await;
    let ok = server
        .mock("POST", "/v1/systemone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(DECISION_BODY)
        .create_async()
        .await;

    let endpoint = model_for(Api::SystemOne, &server.url()).base_url.unwrap();
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    client.decide(&decision_request()).await.unwrap();
    limited.assert_async().await;
    ok.assert_async().await;
}

#[tokio::test]
async fn systemone_errors_on_a_non_success_status() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/v1/systemone")
        .with_status(503)
        .with_body("unavailable")
        .create_async()
        .await;
    let endpoint = format!("{}/v1/systemone", server.url());
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    let error = client.decide(&decision_request()).await.unwrap_err();
    assert!(error.to_string().contains("503"), "{error}");
}

#[tokio::test]
async fn systemone_errors_on_a_malformed_body() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/v1/systemone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("not json")
        .create_async()
        .await;
    let endpoint = format!("{}/v1/systemone", server.url());
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    let error = client.decide(&decision_request()).await.unwrap_err();
    assert!(error.to_string().contains("malformed"), "{error}");
}

#[tokio::test]
async fn systemone_refuses_a_provider_without_credentials() {
    let mut config = cfg(Api::SystemOne, "https://api.example.com".to_string());
    config.api_key = None;
    let Err(error) = lofi_providers::open_decision(Api::SystemOne, &config, None) else {
        panic!("a provider without credentials must not open")
    };
    assert!(error.to_string().contains("no api key"), "{error}");
}

#[tokio::test]
async fn systemone_does_not_retry_a_rate_limit_without_retry_after() {
    let mut server = mockito::Server::new_async().await;
    let limited = server
        .mock("POST", "/v1/systemone")
        .with_status(429)
        .create_async()
        .await;
    let endpoint = format!("{}/v1/systemone", server.url());
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    let error = client.decide(&decision_request()).await.unwrap_err();
    assert!(error.to_string().contains("429"), "{error}");
    limited.assert_async().await;
}

#[tokio::test]
async fn systemone_does_not_retry_a_second_rate_limit() {
    let mut server = mockito::Server::new_async().await;
    let limited = server
        .mock("POST", "/v1/systemone")
        .with_status(429)
        .with_header("retry-after", "0")
        .expect(2)
        .create_async()
        .await;
    let endpoint = format!("{}/v1/systemone", server.url());
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    let error = client.decide(&decision_request()).await.unwrap_err();
    assert!(error.to_string().contains("429"), "{error}");
    limited.assert_async().await;
}

#[tokio::test]
async fn systemone_refuses_an_oversized_body() {
    let mut server = mockito::Server::new_async().await;
    let _m = server
        .mock("POST", "/v1/systemone")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("x".repeat(64 * 1024 + 1))
        .create_async()
        .await;
    let endpoint = format!("{}/v1/systemone", server.url());
    let client = lofi_providers::open_decision(
        Api::SystemOne,
        &cfg(Api::SystemOne, server.url()),
        Some(&endpoint),
    )
    .unwrap();
    let error = client.decide(&decision_request()).await.unwrap_err();
    assert!(error.to_string().contains("64 KiB"), "{error}");
}
