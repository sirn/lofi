use serde_json::{json, Value};

use super::*;

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
