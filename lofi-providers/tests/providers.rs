//! Integration tests for the provider HTTP transports.
//!
//! Each of the three APIs is exercised against a `mockito` SSE server: a
//! canned stream is mapped to the expected [`lofi_types::StreamingEvent`]
//! sequence, and [`lofi_providers::ir::assemble_message`] is checked against the
//! final assembled [`lofi_types::Message`]. The canned bodies match the wire
//! shapes the `ir` mappers expect (see `ir/openai_completions.rs`,
//! `ir/openai_responses.rs`, `ir/anthropic_messages.rs`).

#![allow(clippy::unwrap_used)]



use futures::StreamExt;
use lofi_providers::ir::assemble_message;
use lofi_providers::open;
use lofi_types::{
    Api, ContentBlock, Message, Model, PricingFieldMappings,
    PricingConvention, ProviderConfig, Role, StreamingEvent, Usage,
};

/// A minimal user message used to satisfy the provider's `messages` argument;
/// the canned responses ignore it.
fn user_msg() -> Message {
    Message {
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: "hi".to_string(),
        }],
    }
}

/// A minimal model entry tagged with `api` and a full endpoint `base_url`
/// matching the path the mock server registers. In production the registry
/// resolves this from the provider's `base_url` + api-type mapping `path`;
/// tests set it directly so the mock path is explicit.
fn model_for(api: Api, base_url: &str) -> Model {
    let path = match api {
        Api::OpenAiCompletions => "/v1/chat/completions",
        Api::OpenAiResponses => "/v1/responses",
        Api::AnthropicMessages => "/v1/messages",
    };
    Model {
        id: "test-model".to_string(),
        name: "test-model".to_string(),
        provider: "test".to_string(),
        api,
        reasoning: false,
        thinking: lofi_types::ThinkingLevel::Off,
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

/// Build a `ProviderConfig` pointed at `base_url` with a dummy key. The
/// provider's default `api` is set directly, and the provider's `base_url`
/// is the mock server root; per-model endpoint URLs are not set on the
/// model, so the provider POSTs to `base_url` verbatim and the mock
/// registers the full endpoint path.
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
        thinking_level: None,
        thinking_levels: Vec::new(),
    }
}

/// Collect every event from a provider stream into a `Vec`.
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
        .stream(&model_for(Api::OpenAiCompletions, &server.url()), &[user_msg()], &[])
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            StreamingEvent::TextDelta("Hello".to_string()),
            StreamingEvent::TextDelta(" world".to_string()),
            StreamingEvent::Done(Usage {
                input_tokens: 5,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
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
        .stream(&model_for(Api::OpenAiResponses, &server.url()), &[user_msg()], &[])
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(
        events,
        vec![
            StreamingEvent::TextDelta("Hi".to_string()),
            StreamingEvent::TextDelta("!".to_string()),
            StreamingEvent::Done(Usage {
                input_tokens: 3,
                output_tokens: 4,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
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
    // Anthropic SSE blocks carry an `event:` line. The sequence below mixes a
    // tool_use block with a text delta and an input-json delta, exercising
    // the start/delta/stop lifecycle plus the terminal `message_delta` usage.
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
        .stream(&model_for(Api::AnthropicMessages, &server.url()), &[user_msg()], &[])
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
            StreamingEvent::Done(Usage {
                input_tokens: 0,
                output_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
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


