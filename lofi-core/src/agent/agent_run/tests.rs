#![allow(clippy::unwrap_used)]

use super::super::tool_executor::upgrade_exec_image;
use super::messages::{
    close_orphaned_tool_uses, count_image_blocks, image_payload_bytes, strip_image_blocks,
    MAX_REQUEST_IMAGE_BYTES,
};
use super::*;
use lofi_types::PromptKind;
use serde_json::json;

fn assistant_with_tool_use(id: &str) -> Message {
    Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "exec".to_string(),
            input: json!({"code": "x"}),
        }],
        kind: PromptKind::default(),
    }
}

#[test]
fn orphaned_tool_use_gets_a_cancelled_error_result() {
    let mut messages = vec![assistant_with_tool_use("tool-1")];
    close_orphaned_tool_uses(&mut messages);
    assert_eq!(messages.len(), 2);
    let results = &messages[1];
    assert_eq!(results.role, Role::Tool);
    match &results.blocks[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } => {
            assert_eq!(tool_use_id, "tool-1");
            assert!(is_error);
            assert!(content.contains("cancelled"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn answered_tool_use_is_left_alone() {
    let mut messages = vec![
        assistant_with_tool_use("tool-1"),
        Message {
            origin: None,
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "ok".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        },
    ];
    close_orphaned_tool_uses(&mut messages);
    assert_eq!(messages.len(), 2);
    match &messages[1].blocks[0] {
        ContentBlock::ToolResult { content, .. } => assert_eq!(content, "ok"),
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn text_only_assistant_message_is_untouched() {
    let mut messages = vec![Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![ContentBlock::Text {
            text: "partial text".to_string(),
        }],
        kind: PromptKind::default(),
    }];
    close_orphaned_tool_uses(&mut messages);
    assert_eq!(messages.len(), 1);
}

#[test]
fn only_unanswered_tool_uses_are_closed() {
    let mut messages = vec![
        Message {
            origin: None,
            role: Role::Assistant,
            blocks: vec![
                ContentBlock::ToolUse {
                    id: "tool-1".to_string(),
                    name: "exec".to_string(),
                    input: json!({"code": "a"}),
                },
                ContentBlock::ToolUse {
                    id: "tool-2".to_string(),
                    name: "exec".to_string(),
                    input: json!({"code": "b"}),
                },
            ],
            kind: PromptKind::default(),
        },
        Message {
            origin: None,
            role: Role::Tool,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "ok".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        },
    ];
    close_orphaned_tool_uses(&mut messages);
    assert_eq!(messages.len(), 3);
    match &messages[2].blocks[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } => {
            assert_eq!(tool_use_id, "tool-2");
            assert!(is_error);
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
}

#[test]
fn upgrade_exec_image_normalizes_file_backed_source() {
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let file = tempfile::tempfile().unwrap();
    std::io::Write::write_all(&mut &file, TINY_PNG).unwrap();
    let image = lofi_code::ExecImage {
        file,
        bytes: TINY_PNG.len(),
    };
    let mut value = serde_json::json!({
        "type": "image",
        "media_type": "image/png",
        "bytes": TINY_PNG.len(),
    });

    let image = upgrade_exec_image(&mut value, Some(image), &lofi_types::ImageConfig::default())
        .unwrap()
        .unwrap();

    assert_eq!(image.media_type, "image/jpeg");
    assert!(!image.bytes.is_empty());
    assert_eq!(value["bytes"], serde_json::json!(image.bytes.len()));
    assert_eq!(value["media_type"], serde_json::json!("image/jpeg"));
}

#[test]
fn upgrade_exec_image_ignores_missing_image() {
    let mut value = serde_json::json!({"ok": true, "content": "hi"});
    assert!(
        upgrade_exec_image(&mut value, None, &lofi_types::ImageConfig::default())
            .unwrap()
            .is_none()
    );
    assert_eq!(value["content"], serde_json::json!("hi"));
}

#[test]
fn strip_image_blocks_replaces_images_with_text_markers() {
    let messages = vec![Message {
        origin: None,
        role: Role::User,
        blocks: vec![
            ContentBlock::Text {
                text: "look".to_string(),
            },
            ContentBlock::Image {
                bytes: vec![1, 2, 3],
                media_type: "image/jpeg".to_string(),
            },
            ContentBlock::Image {
                bytes: vec![4, 5],
                media_type: "image/png".to_string(),
            },
        ],
        kind: PromptKind::default(),
    }];
    assert_eq!(count_image_blocks(&messages), 2);
    let stripped = strip_image_blocks(&messages);
    assert_eq!(count_image_blocks(&stripped), 0);
    // The original history is untouched.
    assert_eq!(count_image_blocks(&messages), 2);
    match &stripped[0].blocks[1] {
        ContentBlock::Text { text } => {
            assert!(text.contains("image omitted"));
            assert!(text.contains("image/jpeg"));
        }
        other => panic!("expected Text marker, got {other:?}"),
    }
    // Non-image blocks are preserved verbatim.
    assert_eq!(
        stripped[0].blocks[0],
        ContentBlock::Text {
            text: "look".to_string()
        }
    );
}

#[test]
fn strip_image_blocks_downgrades_tool_result_images() {
    let messages = vec![Message {
        origin: None,
        role: Role::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: "t1".to_string(),
            content: "read image.png".to_string(),
            is_error: false,
            images: vec![lofi_types::ToolResultImage {
                bytes: vec![1, 2, 3],
                media_type: "image/png".to_string(),
            }],
        }],
        kind: PromptKind::default(),
    }];
    assert_eq!(count_image_blocks(&messages), 1);
    let stripped = strip_image_blocks(&messages);
    assert_eq!(count_image_blocks(&stripped), 0);
    match &stripped[0].blocks[0] {
        ContentBlock::ToolResult {
            content, images, ..
        } => {
            assert!(content.contains("read image.png"));
            assert!(content.contains("image omitted"));
            assert!(content.contains("image/png"));
            assert!(images.is_empty());
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    // Payload bytes count both standalone and tool-result images.
    assert!(image_payload_bytes(&messages) > 0);
    assert_eq!(image_payload_bytes(&stripped), 0);
}

#[test]
fn strip_image_blocks_is_a_noop_without_images() {
    let messages = vec![Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: "hi".to_string(),
        }],
        kind: PromptKind::default(),
    }];
    assert_eq!(count_image_blocks(&messages), 0);
    let stripped = strip_image_blocks(&messages);
    assert_eq!(stripped, messages);
}

#[test]
fn image_payload_bytes_counts_base64_length() {
    let image = |n: usize| ContentBlock::Image {
        bytes: vec![0u8; n],
        media_type: "image/jpeg".to_string(),
    };
    let messages = vec![Message {
        origin: None,
        role: Role::User,
        blocks: vec![
            ContentBlock::Text {
                text: "not counted".to_string(),
            },
            image(3), // 4 base64 bytes
            image(4), // 8 base64 bytes (4 -> ceil(4/3)=2 -> 8)
            image(1), // 4 base64 bytes
        ],
        kind: PromptKind::default(),
    }];
    assert_eq!(image_payload_bytes(&messages), 16);
    // Text and empty histories contribute nothing.
    assert_eq!(image_payload_bytes(&[]), 0);
}

#[test]
fn image_payload_budget_distinguishes_over_and_under() {
    let image = |n: usize| ContentBlock::Image {
        bytes: vec![0u8; n],
        media_type: "image/jpeg".to_string(),
    };
    // 7 MiB raw -> ~9.3 MiB base64, over the 8 MiB budget.
    let over = vec![Message {
        origin: None,
        role: Role::User,
        blocks: vec![image(7 * 1024 * 1024)],
        kind: PromptKind::default(),
    }];
    assert!(image_payload_bytes(&over) > MAX_REQUEST_IMAGE_BYTES);
    // 1 MiB raw stays well under.
    let under = vec![Message {
        origin: None,
        role: Role::User,
        blocks: vec![image(1024 * 1024)],
        kind: PromptKind::default(),
    }];
    assert!(image_payload_bytes(&under) <= MAX_REQUEST_IMAGE_BYTES);
}
