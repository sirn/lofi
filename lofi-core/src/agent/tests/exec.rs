#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

#[test]
fn cap_tool_result_keeps_short_unchanged() {
    assert_eq!(cap_tool_result("hello"), "hello");
}

#[test]
fn cap_tool_result_truncates_long_with_marker() {
    let big = "x".repeat(MAX_TOOL_RESULT_BYTES + 1000);
    let capped = cap_tool_result(&big);
    assert!(capped.len() < big.len());
    assert!(capped.contains("[output truncated:"));
    assert!(capped.contains(&format!("{} bytes total", big.len())));
    assert!(capped.starts_with("xxxx"));
}

#[test]
fn cap_tool_result_respects_char_boundary() {
    // Fill to just past the cap with multibyte chars so the boundary cut
    // would land mid-char if unhandled.
    let unit = "é"; // 2 bytes
    let n = MAX_TOOL_RESULT_BYTES / 2 + 50;
    let big = unit.repeat(n);
    let capped = cap_tool_result(&big);
    assert!(capped.contains("[output truncated:"));
}

#[test]
fn cap_exec_result_uses_larger_outer_limit() {
    // The exec-level cap is larger than the per-tool cap, so a payload
    // bigger than MAX_TOOL_RESULT_BYTES but under MAX_EXEC_RESULT_BYTES is
    // preserved by cap_exec_result but would be truncated by cap_tool_result.
    let mid = "x".repeat(MAX_TOOL_RESULT_BYTES + 1000);
    assert!(mid.len() < MAX_EXEC_RESULT_BYTES);
    assert_eq!(cap_exec_result(&mid), mid);
    assert_ne!(cap_tool_result(&mid), mid);
    let huge = "x".repeat(MAX_EXEC_RESULT_BYTES + 5000);
    let capped = cap_exec_result(&huge);
    assert!(capped.contains("[output truncated:"));
    assert!(capped.contains(&format!("{} bytes total", huge.len())));
}

#[test]
fn exec_schema_uses_code_contract() {
    let schema = exec_tool_schema();
    assert_eq!(schema.name, lofi_code::EXEC_TOOL_NAME);
    assert_eq!(schema.description, lofi_code::EXEC_TOOL_DESCRIPTION);
    assert_eq!(schema.input_schema, lofi_code::exec_tool_input_schema());
}

#[test]
fn parse_exec_input_extracts_fields() {
    let input = serde_json::json!({
        "code": "return 1",
        "strings": { "a": "x", "b": 2 },
        "display": { "hint": "row" }
    });
    let (code, strings, display) = parse_exec_input(&input);
    assert_eq!(code, "return 1");
    assert_eq!(strings.get("a").unwrap(), "x");
    assert_eq!(strings.get("b").unwrap(), "2");
    assert_eq!(display["hint"], "row");
}

#[test]
fn parse_exec_input_missing_code_is_empty() {
    let (code, strings, display) = parse_exec_input(&serde_json::Value::Null);
    assert!(code.is_empty());
    assert!(strings.is_empty());
    assert!(display.is_null());
}

#[test]
fn parse_exec_input_trims_surrounding_newlines() {
    let input = serde_json::json!({ "code": "\nreturn 1\n" });
    let (code, _, _) = parse_exec_input(&input);
    assert_eq!(code, "return 1");
}

#[test]
fn exec_result_display_handles_structured_success() {
    assert_eq!(
        exec_result_display(r#"{"ok":true,"value":null}"#, false),
        "null"
    );
    assert_eq!(
        exec_result_display(r#"{"ok":true,"value":"done"}"#, false),
        "done"
    );
    assert_eq!(
        exec_result_display(r#"{"ok":true,"logs":"worked\n"}"#, false),
        "worked\n"
    );
    assert_eq!(exec_result_display(r#"{"ok":true}"#, false), "");
}

#[test]
fn extract_code_prefix_streams_and_trims_leading_newline() {
    let cases = [
        (r#"{"code":"\nle"#, "le"),
        (r#"{"code":"\nlet x"#, "let x"),
        (r#"{"code":"\nlet x\nlet y"#, "let x\nlet y"),
        (r#"{"display":"z","code":"\nlet x"#, "let x"),
    ];
    for (raw, want) in cases {
        assert_eq!(extract_code_prefix(raw), want, "raw: {raw}");
    }
}

#[test]
fn code_prefix_decoder_handles_split_escape_incrementally() {
    let mut decoder = CodePrefixDecoder::default();
    assert_eq!(decoder.update(r#"{"code":"\nlet x\u"#), "let x");
    assert_eq!(decoder.update(r#"{"code":"\nlet x\u0061"}"#), "let xa");
}
