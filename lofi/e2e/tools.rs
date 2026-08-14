use crate::support::{
    anthropic_text_response, anthropic_tool_response, fragmented_tool_response,
    google_text_response, google_tool_response, parallel_tool_response, responses_response,
    responses_tool_response, text_response, transcript_text, Fixture, MockServer, WAIT,
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
