#![allow(clippy::unwrap_used)]

use serde_json::{json, Value};

use super::*;

fn init_frame(tools: &Value, server_status: &str) -> Value {
    json!({
        "tools": tools,
        "mcp_servers": [{"name": "lofi", "status": server_status}],
    })
}

#[test]
fn validate_init_accepts_the_exec_tool_and_the_lofi_server() {
    let frame = init_frame(&json!([EXEC]), "connected");
    assert!(validate_init(&frame).is_ok());
}

#[test]
fn validate_init_names_an_extra_tool() {
    let frame = init_frame(&json!([EXEC, "ToolSearch"]), "connected");
    let error = validate_init(&frame).unwrap_err().to_string();
    assert!(error.contains("ToolSearch"), "{error}");
}

#[test]
fn validate_init_names_a_missing_tool_list() {
    let frame = json!({"mcp_servers": []});
    let error = validate_init(&frame).unwrap_err().to_string();
    assert!(error.contains("null"), "{error}");
}

#[test]
fn validate_init_names_a_server_that_did_not_connect() {
    let frame = init_frame(&json!([EXEC]), "failed");
    let error = validate_init(&frame).unwrap_err().to_string();
    assert!(error.contains("failed"), "{error}");
}

#[test]
fn observed_cuts_a_long_list() {
    let tools = Value::Array((0..50).map(|i| json!(format!("tool-{i}"))).collect());
    let text = observed(&tools);
    assert!(text.ends_with('…'));
    assert_eq!(text.chars().count(), 201);
}

#[test]
fn effort_levels_include_the_cli_maximum() {
    let levels = effort_levels();
    let names: Vec<&str> = levels
        .iter()
        .map(lofi_types::ThinkingLevel::as_str)
        .collect();
    assert_eq!(names, ["low", "medium", "high", "xhigh", "max"]);
}

#[test]
fn effort_flag_omits_off_and_passes_max() {
    assert_eq!(
        effort_flag(&lofi_types::ThinkingLevel::Off).ok(),
        Some(None)
    );
    assert_eq!(
        effort_flag(&lofi_types::ThinkingLevel::Custom("max".into())).ok(),
        Some(Some("--effort=max".into()))
    );
    assert!(effort_flag(&lofi_types::ThinkingLevel::Custom("nope".into())).is_err());
}

#[test]
fn content_value_stays_a_string_without_images() {
    assert_eq!(content_value("hello", &[]), Value::String("hello".into()));
}

#[test]
fn content_value_sends_images_as_base64_blocks() {
    let block = lofi_types::ContentBlock::Image {
        bytes: vec![1, 2, 3],
        media_type: "image/png".into(),
    };
    let value = content_value("look", &[block]);
    assert_eq!(value[0], json!({"type": "text", "text": "look"}));
    assert_eq!(value[1]["type"], "image");
    assert_eq!(value[1]["source"]["type"], "base64");
    assert_eq!(value[1]["source"]["media_type"], "image/png");
    assert_eq!(value[1]["source"]["data"], "AQID");
}
