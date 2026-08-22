use std::fmt::Write as _;
use std::time::Duration;

use serde_json::json;

use crate::support::{
    anthropic_redacted_thinking_tool_response, anthropic_text_response, anthropic_tool_response,
    google_text_response, google_tool_response, responses_response, responses_tool_response,
    text_response, tool_response, Fixture, MockResponse, MockServer,
};

fn chat_body(text: &str) -> String {
    let event = json!({ "choices": [{ "delta": { "content": text } }] });
    format!("data: {event}\n\ndata: [DONE]\n\n")
}

fn responses_body(text: &str) -> String {
    let text = json!({ "type": "response.output_text.delta", "delta": text });
    let completed = json!({
        "type": "response.completed",
        "response": { "usage": { "input_tokens": 2, "output_tokens": 1 } },
    });
    format!("data: {text}\n\ndata: {completed}\n\ndata: [DONE]\n\n")
}

fn anthropic_body(text: &str) -> String {
    let delta = json!({
        "index": 0,
        "delta": { "type": "text_delta", "text": text },
    });
    format!("event: content_block_delta\ndata: {delta}\n\nevent: message_stop\ndata: {{}}\n\n")
}

fn google_body(text: &str) -> String {
    let event = json!({
        "candidates": [{
            "content": { "role": "model", "parts": [{ "text": text }] },
            "finishReason": "STOP",
        }]
    });
    format!("data: {event}\n\n")
}

#[test]
fn fragmented_sse_custom_headers_and_no_auth_work_for_every_provider() {
    let bodies = [
        chat_body("fragmented chat answer"),
        responses_body("fragmented responses answer"),
        anthropic_body("fragmented anthropic answer"),
        google_body("fragmented google answer"),
    ];
    let server = MockServer::start(
        bodies
            .into_iter()
            .map(|body| {
                let split_at: Vec<usize> = (1..body.len()).step_by(7).collect();
                MockResponse::fragmented_sse(body, &split_at, Duration::from_millis(1))
                    .with_header("x-mock-response", "fragmented")
            })
            .collect(),
    );
    let fixture = Fixture::new(&server);
    let mut config = std::fs::read_to_string(&fixture.config).unwrap();
    for provider in ["mock", "responses", "anthropic", "google"] {
        writeln!(
            config,
            "\n[providers.{provider}.headers]\nx-e2e-provider = \"{provider}\""
        )
        .unwrap();
    }
    std::fs::write(&fixture.config, config).unwrap();

    for (model, prompt, answer) in [
        ("mock/chat", "fragment chat", "fragmented chat answer"),
        (
            "responses/reasoning",
            "fragment responses",
            "fragmented responses answer",
        ),
        (
            "anthropic/tools",
            "fragment anthropic",
            "fragmented anthropic answer",
        ),
        (
            "google/tools",
            "fragment google",
            "fragmented google answer",
        ),
    ] {
        let output = fixture.output(&["--model", model, "--print", prompt]);
        assert!(
            output.status.success(),
            "{model}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(answer));
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    for (request, provider) in requests
        .iter()
        .zip(["mock", "responses", "anthropic", "google"])
    {
        let headers = request.headers.to_ascii_lowercase();
        assert!(
            headers.contains(&format!("x-e2e-provider: {provider}")),
            "{}",
            request.headers
        );
        assert!(!headers.contains("authorization:"));
        assert!(!headers.contains("x-api-key:"));
        assert!(!headers.contains("x-goog-api-key:"));
    }
}

#[test]
fn premature_stream_end_retries_for_every_provider_protocol() {
    let partial_chat = {
        let event = json!({ "choices": [{ "delta": { "content": "discard chat" } }] });
        format!("data: {event}\n\n")
    };
    let partial_responses = {
        let event = json!({
            "type": "response.output_text.delta",
            "delta": "discard responses",
        });
        format!("data: {event}\n\n")
    };
    let partial_anthropic = {
        let event = json!({
            "index": 0,
            "delta": { "type": "text_delta", "text": "discard anthropic" },
        });
        format!("event: content_block_delta\ndata: {event}\n\n")
    };
    let partial_google = {
        let event = json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{ "text": "discard google" }] }
            }]
        });
        format!("data: {event}\n\n")
    };
    let server = MockServer::start(vec![
        MockResponse::truncated_sse(partial_chat, 13),
        MockResponse::sse(chat_body("complete chat retry")),
        MockResponse::sse(partial_responses),
        MockResponse::sse(responses_body("complete responses retry")),
        MockResponse::sse(partial_anthropic),
        MockResponse::sse(anthropic_body("complete anthropic retry")),
        MockResponse::sse(partial_google),
        MockResponse::sse(google_body("complete google retry")),
    ]);
    let fixture = Fixture::new(&server);

    for (model, answer) in [
        ("mock/chat", "complete chat retry"),
        ("responses/reasoning", "complete responses retry"),
        ("anthropic/tools", "complete anthropic retry"),
        ("google/tools", "complete google retry"),
    ] {
        let output = fixture.output(&["--model", model, "--print", "retry premature stream"]);
        assert!(
            output.status.success(),
            "{model}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(answer));
    }
    assert_eq!(server.request_count(), 8);
}

#[test]
fn service_tier_is_forwarded_for_openai_protocols() {
    // mock (openai-completions) defaults to flex per config; responses
    // (openai-responses) defaults to priority. Both must land on the wire and
    // @tier must override the configured default for one run.
    let server = MockServer::start(vec![
        MockResponse::sse(chat_body("chat flex")),
        MockResponse::sse(responses_body("responses override")),
    ]);
    let fixture = Fixture::new(&server);

    let out = fixture.output(&["--model", "mock/chat", "--print", "p"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("chat flex"));

    // The @flex suffix overrides the configured priority default.
    let out = fixture.output(&["--model", "responses/reasoning@flex", "--print", "p"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("responses override"));

    let requests = server.requests();
    let chat_body = &requests[0].body;
    let responses_body = &requests[1].body;
    assert!(
        chat_body.contains("\"service_tier\":\"flex\""),
        "mock/chat body: {chat_body}"
    );
    assert!(
        responses_body.contains("\"service_tier\":\"flex\""),
        "responses body: {responses_body}"
    );
}

#[test]
fn provider_reported_stream_errors_fail_without_silent_partial_answers() {
    let chat = json!({ "error": { "message": "chat stream rejected" } });
    let responses = json!({
        "type": "response.failed",
        "error": { "message": "responses stream rejected" },
    });
    let anthropic = json!({
        "type": "error",
        "error": { "message": "anthropic stream rejected" },
    });
    let google = json!({ "error": { "message": "google stream rejected" } });
    let server = MockServer::start(vec![
        MockResponse::sse(format!("data: {chat}\n\n")),
        MockResponse::sse(format!("data: {responses}\n\n")),
        MockResponse::sse(format!("event: error\ndata: {anthropic}\n\n")),
        MockResponse::sse(format!("data: {google}\n\n")),
    ]);
    let fixture = Fixture::new(&server);

    for (model, marker) in [
        ("mock/chat", "chat stream rejected"),
        ("responses/reasoning", "responses stream rejected"),
        ("anthropic/tools", "anthropic stream rejected"),
        ("google/tools", "google stream rejected"),
    ] {
        let output = fixture.output(&["--model", model, "--print", "provider stream error"]);
        assert!(!output.status.success(), "{model} unexpectedly succeeded");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(marker),
            "{model}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());
    }
    assert_eq!(server.request_count(), 4);
}

#[test]
fn image_tool_results_use_each_provider_native_wire_format() {
    let read_image = r#"return await lofi.read("pixel.png");"#;
    let server = MockServer::start(vec![
        tool_response("chat-image", read_image),
        text_response("chat image final"),
        responses_tool_response("responses-image", read_image),
        responses_response("responses image thought", "responses image final"),
        anthropic_tool_response("anthropic-image", read_image),
        anthropic_text_response("anthropic image final"),
        google_tool_response("google-image", read_image),
        google_text_response("google image final"),
    ]);
    let fixture = Fixture::new(&server);
    std::fs::write(fixture.workspace.join("pixel.png"), one_pixel_png()).unwrap();
    let config = std::fs::read_to_string(&fixture.config)
        .unwrap()
        .replace(
            "[providers.responses.models.reasoning]\nname = \"C Responses\"\ncontext_window = 100000\nreasoning = true",
            "[providers.responses.models.reasoning]\nname = \"C Responses\"\ncontext_window = 100000\nreasoning = true\nsupports_image = true",
        )
        .replace(
            "[providers.anthropic.models.tools]\nname = \"D Anthropic\"\ncontext_window = 100000\nreasoning = true",
            "[providers.anthropic.models.tools]\nname = \"D Anthropic\"\ncontext_window = 100000\nreasoning = true\nsupports_image = true",
        );
    std::fs::write(&fixture.config, config).unwrap();

    for (model, answer) in [
        ("mock/chat", "chat image final"),
        ("responses/reasoning", "responses image final"),
        ("anthropic/tools", "anthropic image final"),
        ("google/tools", "google image final"),
    ] {
        let output = fixture.output(&["--model", model, "--print", "read provider image"]);
        assert!(
            output.status.success(),
            "{model}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(answer));
    }

    let requests = server.requests();
    assert_eq!(requests.len(), 8);
    let chat: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert!(chat.to_string().contains("image_url"));
    let responses: serde_json::Value = serde_json::from_str(&requests[3].body).unwrap();
    assert!(responses.to_string().contains("input_image"));
    let anthropic: serde_json::Value = serde_json::from_str(&requests[5].body).unwrap();
    assert!(anthropic.to_string().contains("\"source\":{\"data\":"));
    let google: serde_json::Value = serde_json::from_str(&requests[7].body).unwrap();
    assert!(google.to_string().contains("inlineData"));
}

#[test]
fn anthropic_redacted_thinking_round_trips_a_signed_omission() {
    // Round 1 emits a redacted thinking block followed by a tool call. Round
    // 2 must replay the opaque blob back as a redacted_thinking block so the
    // server can verify the safety-system decision.
    let server = MockServer::start(vec![
        anthropic_redacted_thinking_tool_response("e2e-redacted-blob", "call_x", "return 1;"),
        anthropic_text_response("done"),
    ]);
    let fixture = Fixture::new(&server);

    let output = fixture.output(&[
        "--model",
        "anthropic/tools",
        "--print",
        "redact and continue",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = second["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|m| m["role"] == "assistant")
        .expect("second turn must include the prior assistant message");
    let blocks = assistant["content"].as_array().unwrap();
    let redacted = blocks
        .iter()
        .find(|b| b["type"] == "redacted_thinking")
        .expect("redacted_thinking must round-trip as itself");
    assert_eq!(redacted["data"], "e2e-redacted-blob");
}

fn one_pixel_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0x0d, 0x49, 0x48, 0x44, 0x52, 0,
        0, 0, 1, 0, 0, 0, 1, 8, 4, 0, 0, 0, 0xb5, 0x1c, 0x0c, 0x02, 0, 0, 0, 0x0b, 0x49, 0x44,
        0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0x0f, 0, 1, 5, 1, 1, 0x27, 0x18, 0xe3, 0x66, 0,
        0, 0, 0, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}
