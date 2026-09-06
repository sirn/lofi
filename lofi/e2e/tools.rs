use std::fmt::Write;

use crate::support::{
    anthropic_text_response, anthropic_tool_response, anthropic_usage_response,
    fragmented_tool_response, google_text_response, google_thinking_usage_response,
    google_tool_response, parallel_tool_response, responses_response, responses_tool_response,
    text_response, transcript_text, Fixture, MockResponse, MockServer, WAIT,
};

#[test]
fn openai_chat_executes_fragmented_tool_arguments_and_returns_the_result() {
    let server = MockServer::start(vec![
        fragmented_tool_response(
            "fragmented-call",
            r#"return { marker: "fragmented tool result" };"#,
        ),
        text_response("fragmented tool final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("run fragmented tool call");
    tui.wait_for("fragmented tool final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = second["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(assistant["tool_calls"][0]["id"], "fragmented-call");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(result["tool_call_id"], "fragmented-call");
    assert!(result["content"]
        .as_str()
        .unwrap()
        .contains("fragmented tool result"));
}

#[test]
fn openai_chat_executes_parallel_tool_calls_and_returns_both_results() {
    let server = MockServer::start(vec![
        parallel_tool_response(&[
            ("parallel-a", r#"return { marker: "parallel result a" };"#),
            ("parallel-b", r#"return { marker: "parallel result b" };"#),
        ]),
        text_response("parallel tool final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&[]);

    tui.submit("run parallel tool calls");
    tui.wait_for("parallel tool final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = second["messages"].as_array().unwrap();
    let assistant = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
    let results: Vec<_> = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().any(|result| {
        result["tool_call_id"] == "parallel-a"
            && result["content"]
                .as_str()
                .is_some_and(|text| text.contains("parallel result a"))
    }));
    assert!(results.iter().any(|result| {
        result["tool_call_id"] == "parallel-b"
            && result["content"]
                .as_str()
                .is_some_and(|text| text.contains("parallel result b"))
    }));
}

#[test]
fn openai_responses_executes_tool_call_and_returns_function_output() {
    let server = MockServer::start(vec![
        responses_tool_response(
            "responses-call",
            r#"return { marker: "responses tool result" };"#,
        ),
        responses_response("responses final thinking", "responses tool final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning:high"]);

    tui.submit("run responses tool call");
    tui.wait_for("responses tool final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let input = second["input"].as_array().unwrap();
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap();
    assert_eq!(call["call_id"], "responses-call");
    let result = input
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .unwrap();
    assert_eq!(result["call_id"], "responses-call");
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("responses tool result"));
}

#[test]
fn openai_responses_reports_unawaited_nested_tool_failure() {
    let server = MockServer::start(vec![
        responses_tool_response(
            "responses-unawaited-failure",
            r#"lofi.bash({ cmd: "exit 7" });"#,
        ),
        responses_response(
            "responses failure thinking",
            "responses failure final answer",
        ),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning:high"]);

    tui.submit("run unawaited failing command");
    tui.wait_for("responses failure final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let output = second["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "function_call_output")
        .and_then(|item| item["output"].as_str())
        .expect("exec result in the second provider request");
    let result: serde_json::Value = serde_json::from_str(output).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["errors"][0]["tool"], "bash");
    assert_eq!(result["errors"][0]["result"]["code"], 7);

    let events = fixture.events();
    let block = events
        .iter()
        .filter(|event| event["type"] == "message" && event["role"] == "tool")
        .flat_map(|event| event["blocks"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result")
        .expect("exec result in the transcript");
    assert_eq!(block["is_error"], true);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(block["content"].as_str().unwrap()).unwrap(),
        result
    );
}

#[test]
fn anthropic_executes_tool_call_and_returns_tool_result() {
    let server = MockServer::start(vec![
        anthropic_tool_response(
            "anthropic-call",
            r#"return { marker: "anthropic tool result" };"#,
        ),
        anthropic_text_response("anthropic tool final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "anthropic/tools"]);

    tui.submit("run anthropic tool call");
    tui.wait_for("anthropic tool final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].path.ends_with("/v1/messages"));
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let messages = second["messages"].as_array().unwrap();
    let call = messages
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_use")
        .unwrap();
    assert_eq!(call["id"], "anthropic-call");
    let result = messages
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .find(|block| block["type"] == "tool_result")
        .unwrap();
    assert_eq!(result["tool_use_id"], "anthropic-call");
    assert!(result["content"]
        .as_str()
        .unwrap()
        .contains("anthropic tool result"));
}

#[test]
fn google_executes_function_call_and_returns_function_response() {
    let server = MockServer::start(vec![
        google_tool_response("google-call", r#"return { marker: "google tool result" };"#),
        google_text_response("google tool final answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "google/tools"]);

    tui.submit("run google tool call");
    tui.wait_for("google tool final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .path
        .contains("models/tools:streamGenerateContent?alt=sse"));
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let parts: Vec<_> = second["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .collect();
    let call = parts
        .iter()
        .find(|part| part.get("functionCall").is_some())
        .unwrap();
    assert_eq!(call["functionCall"]["name"], "exec");
    let result = parts
        .iter()
        .find(|part| part.get("functionResponse").is_some())
        .unwrap();
    assert_eq!(result["functionResponse"]["name"], "exec");
    assert!(result.to_string().contains("google tool result"));
    assert!(
        second
            .to_string()
            .contains("Z29vZ2xlLXNpZ25hdHVyZS1tYXJrZXI="),
        "second Google request: {second}"
    );
    assert_eq!(
        second["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        8192
    );
}

#[test]
fn google_streams_thinking_usage_and_replays_its_signature() {
    let server = MockServer::start(vec![
        google_thinking_usage_response(
            "google thinking stream marker",
            "google thinking answer marker",
        ),
        google_text_response("google replay answer marker"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "google/tools:low"]);

    tui.submit("google thinking prompt marker");
    tui.wait_for("google thinking answer marker", WAIT);
    tui.submit("google replay prompt marker");
    tui.wait_for("google replay answer marker", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]
        .path
        .contains("models/tools:streamGenerateContent?alt=sse"));
    let first: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert!(first["systemInstruction"]["parts"][0]["text"]
        .as_str()
        .unwrap()
        .contains("You are"));
    assert_eq!(first["tools"][0]["functionDeclarations"][0]["name"], "exec");
    assert_eq!(
        first["generationConfig"]["thinkingConfig"]["includeThoughts"],
        true
    );
    assert_eq!(
        first["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        2048
    );

    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let parts: Vec<_> = second["contents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|content| content["parts"].as_array().into_iter().flatten())
        .collect();
    let thought = parts
        .iter()
        .find(|part| part["thought"] == true)
        .expect("signed thought in replay request");
    assert_eq!(thought["text"], "google thinking stream marker");
    assert_eq!(
        thought["thoughtSignature"],
        "Z29vZ2xlLXRob3VnaHQtc2lnbmF0dXJl"
    );

    let events = fixture.events();
    let turn_end = events
        .iter()
        .find(|event| event["type"] == "turn_end" && event["usage"]["output_tokens"] == 8)
        .expect("Google usage in transcript");
    assert_eq!(turn_end["usage"]["input_tokens"], 9);
    assert_eq!(turn_end["usage"]["cache_read_tokens"], 4);
    let transcript = transcript_text(&events);
    assert!(transcript.contains("google thinking stream marker"));
    assert!(transcript.contains("Z29vZ2xlLXRob3VnaHQtc2lnbmF0dXJl"));
}

#[test]
fn anthropic_replays_signed_thinking_and_sets_cache_breakpoints() {
    let server = MockServer::start(vec![
        anthropic_usage_response(
            "anthropic thinking marker",
            "anthropic signature marker",
            "anthropic first answer",
        ),
        anthropic_text_response("anthropic second answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "anthropic/tools:high"]);

    tui.submit("anthropic first prompt");
    tui.wait_for("anthropic first answer", WAIT);
    tui.submit("anthropic second prompt");
    tui.wait_for("anthropic second answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].path.ends_with("/v1/messages"));
    assert!(requests[0]
        .headers
        .to_ascii_lowercase()
        .contains("anthropic-version: 2023-06-01"));
    let first: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(first["thinking"]["type"], "adaptive");
    assert_eq!(first["output_config"]["effort"], "high");
    assert_eq!(first["tools"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(first["system"][0]["cache_control"]["type"], "ephemeral");

    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let blocks: Vec<_> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .collect();
    let thinking = blocks
        .iter()
        .find(|block| block["type"] == "thinking")
        .unwrap();
    assert_eq!(thinking["thinking"], "anthropic thinking marker");
    assert_eq!(thinking["signature"], "anthropic signature marker");
    let latest_user = second["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(
        latest_user["content"][0]["cache_control"]["type"],
        "ephemeral"
    );

    let events = fixture.events();
    let first_turn = events
        .iter()
        .find(|event| event["type"] == "turn_end" && event["usage"]["output_tokens"] == 7)
        .expect("Anthropic usage in transcript");
    assert_eq!(first_turn["usage"]["input_tokens"], 11);
    assert_eq!(first_turn["usage"]["cache_read_tokens"], 3);
    assert_eq!(first_turn["usage"]["cache_write_tokens"], 2);
}

#[test]
fn compaction_and_resume_keep_latest_tool_cycle_valid() {
    let server = MockServer::start(vec![
        text_response("compact setup answer one"),
        text_response("compact setup answer two"),
        text_response("compact setup answer three"),
        fragmented_tool_response(
            "compact-tool-call",
            r#"return { marker: "compact tool result" };"#,
        ),
        text_response("compact tool answer"),
        text_response("compact resumed answer"),
    ]);
    let fixture = Fixture::new(&server);
    let mut first = fixture.spawn(&[]);

    for (prompt, answer) in [
        ("compact setup prompt one", "compact setup answer one"),
        ("compact setup prompt two", "compact setup answer two"),
        ("compact setup prompt three", "compact setup answer three"),
    ] {
        first.submit(prompt);
        first.wait_for(answer, WAIT);
        first.clear_output();
    }
    first.submit("compact tool prompt");
    first.wait_for("compact tool answer", WAIT);
    first.submit("/compact");
    first.wait_for("Compacted", WAIT);
    first.submit("/quit");
    first.wait_exit();

    let mut resumed = fixture.spawn(&["--continue"]);
    resumed.submit("compact resumed prompt");
    resumed.wait_for("compact resumed answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 6);
    let resumed_request: serde_json::Value =
        serde_json::from_str(&requests.last().unwrap().body).unwrap();
    let messages = resumed_request["messages"].as_array().unwrap();
    assert!(messages.iter().any(|message| {
        message["role"] == "user"
            && message["content"]
                .as_str()
                .is_some_and(|text| text.contains("This summary captures work done"))
    }));
    let call = messages
        .iter()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .unwrap();
    assert_eq!(call["tool_calls"][0]["id"], "compact-tool-call");
    let result = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(result["tool_call_id"], "compact-tool-call");
    assert!(result["content"]
        .as_str()
        .unwrap()
        .contains("compact tool result"));
    let transcript = transcript_text(&fixture.events());
    assert!(transcript.contains(r#""type":"compaction""#));
}

fn parallel_responses_response(calls: &[(&str, &str)]) -> MockResponse {
    let mut events = String::new();
    for (index, (call_id, code)) in calls.iter().enumerate() {
        let item_id = format!("item-{call_id}");
        let added = serde_json::json!({
            "type": "response.output_item.added",
            "item": {
                "type": "function_call",
                "id": item_id,
                "call_id": call_id,
                "name": "exec",
                "arguments": "",
            },
        });
        let delta = serde_json::json!({
            "type": "response.function_call_arguments.delta",
            "item_id": item_id,
            "delta": serde_json::json!({ "code": code }).to_string(),
        });
        let done = serde_json::json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": item_id,
                "call_id": call_id,
                "name": "exec",
                "arguments": serde_json::json!({ "code": code }).to_string(),
            },
        });
        for event in [added, delta, done] {
            write!(events, "data: {event}\n\n").unwrap();
        }
        if index == 0 {
            events.push_str(
                "data: {\"type\":\"response.output_text.delta\",\"delta\":\"interleaved\"}\n\n",
            );
        }
    }
    events.push_str(
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n\ndata: [DONE]\n\n",
    );
    MockResponse::sse(events)
}

fn malformed_responses_tool_response(call_id: &str) -> MockResponse {
    let item_id = format!("item-{call_id}");
    let added = serde_json::json!({
        "type": "response.output_item.added",
        "item": {
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": "exec",
            "arguments": "",
        },
    });
    let delta = serde_json::json!({
        "type": "response.function_call_arguments.delta",
        "item_id": item_id,
        "delta": "{not-json",
    });
    let done = serde_json::json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "id": item_id,
            "call_id": call_id,
            "name": "exec",
            "arguments": "{not-json",
        },
    });
    MockResponse::sse(format!(
        "data: {added}\n\ndata: {delta}\n\ndata: {done}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":3}}}}}}\n\ndata: [DONE]\n\n"
    ))
}

fn malformed_openai_tool_response(call_id: &str) -> MockResponse {
    let event = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": "exec", "arguments": "{not-json" }
                }]
            }
        }]
    });
    MockResponse::sse(format!("data: {event}\n\ndata: [DONE]\n\n"))
}

#[test]
fn openai_responses_executes_interleaved_parallel_calls() {
    let server = MockServer::start(vec![
        parallel_responses_response(&[
            (
                "responses-parallel-a",
                r#"return { marker: "responses parallel a" };"#,
            ),
            (
                "responses-parallel-b",
                r#"return { marker: "responses parallel b" };"#,
            ),
        ]),
        responses_response(
            "responses parallel thinking",
            "responses parallel final answer",
        ),
    ]);
    let fixture = Fixture::new(&server);
    let mut tui = fixture.spawn(&["--model", "responses/reasoning"]);

    tui.submit("run interleaved responses calls");
    tui.wait_for("responses parallel final answer", WAIT);

    let requests = server.requests();
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    let input = second["input"].as_array().unwrap();
    for (call_id, marker) in [
        ("responses-parallel-a", "responses parallel a"),
        ("responses-parallel-b", "responses parallel b"),
    ] {
        assert!(input
            .iter()
            .any(|item| item["type"] == "function_call" && item["call_id"] == call_id));
        assert!(input.iter().any(|item| {
            item["type"] == "function_call_output"
                && item["call_id"] == call_id
                && item["output"]
                    .as_str()
                    .is_some_and(|text| text.contains(marker))
        }));
    }
}

#[test]
fn malformed_tool_arguments_return_an_error_to_each_provider() {
    let server = MockServer::start(vec![
        malformed_openai_tool_response("malformed-chat-call"),
        text_response("malformed chat final answer"),
        malformed_responses_tool_response("malformed-responses-call"),
        responses_response(
            "malformed responses thinking",
            "malformed responses final answer",
        ),
    ]);
    let fixture = Fixture::new(&server);

    let mut chat = fixture.spawn(&[]);
    chat.submit("run malformed chat arguments");
    chat.wait_for("malformed chat final answer", WAIT);
    let requests = server.requests();
    assert!(requests[1].body.contains("malformed-chat-call"));
    assert!(requests[1].body.contains("arguments"));

    let mut responses = fixture.spawn(&["--model", "responses/reasoning"]);
    responses.submit("run malformed responses arguments");
    responses.wait_for("malformed responses final answer", WAIT);
    let requests = server.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[3].body.contains("malformed-responses-call"));
    assert!(requests[3].body.contains("arguments"));
}
