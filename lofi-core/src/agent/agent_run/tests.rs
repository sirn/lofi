#![allow(clippy::unwrap_used)]

use super::super::tool_executor::upgrade_exec_image;
use super::messages::{
    close_orphaned_tool_uses, close_unanswered_tool_uses, count_image_blocks, image_payload_bytes,
    strip_image_blocks, MAX_REQUEST_IMAGE_BYTES,
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

fn assistant_with_tool_uses(ids: &[&str]) -> Message {
    Message {
        origin: None,
        role: Role::Assistant,
        blocks: ids
            .iter()
            .map(|id| ContentBlock::ToolUse {
                id: (*id).to_string(),
                name: "exec".to_string(),
                input: json!({"code": "x"}),
            })
            .collect(),
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

fn tool_result(id: &str, content: &str) -> Message {
    Message {
        origin: None,
        role: Role::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: content.to_string(),
            is_error: false,
            images: Vec::new(),
        }],
        kind: PromptKind::default(),
    }
}

#[test]
fn a_region_of_several_result_messages_is_one_message() {
    let mut messages = vec![
        assistant_with_tool_uses(&["tool-1", "tool-2"]),
        tool_result("tool-1", "a"),
        tool_result("tool-2", "b"),
    ];
    close_unanswered_tool_uses(&mut messages);
    assert_eq!(messages.len(), 2, "one result message for one turn");
    let ids: Vec<&str> = messages[1]
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        ids,
        vec!["tool-1", "tool-2"],
        "the real answers survive once"
    );
}

#[test]
fn a_mixed_legacy_message_keeps_its_result_and_its_text() {
    let mut messages = vec![
        assistant_with_tool_uses(&["tool-1"]),
        Message {
            origin: None,
            role: Role::User,
            blocks: vec![
                ContentBlock::Text {
                    text: "note".to_string(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "tool-1".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                    images: Vec::new(),
                },
            ],
            kind: PromptKind::default(),
        },
    ];
    close_unanswered_tool_uses(&mut messages);
    assert_eq!(messages.len(), 3, "the results and the text part ways");
    assert_eq!(messages[1].role, Role::Tool);
    assert_eq!(messages[2].role, Role::User);
    let results: Vec<String> = messages[1]
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["ok"], "one answer, and it is the real one");
    assert!(matches!(
        &messages[2].blocks[0],
        ContentBlock::Text { text } if text == "note"
    ));
}

#[test]
fn a_legacy_result_message_becomes_the_tool_role() {
    let mut messages = vec![
        assistant_with_tool_uses(&["tool-1"]),
        Message {
            origin: None,
            role: Role::User,
            blocks: vec![ContentBlock::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "ok".to_string(),
                is_error: false,
                images: Vec::new(),
            }],
            kind: PromptKind::default(),
        },
    ];
    close_unanswered_tool_uses(&mut messages);
    assert_eq!(messages.len(), 2);
    assert_eq!(
        messages[1].role,
        Role::Tool,
        "the provider serializes results by role"
    );
    let mut repaired = messages.clone();
    close_unanswered_tool_uses(&mut repaired);
    assert_eq!(repaired, messages, "a second repair changes nothing");
}

#[test]
fn a_result_for_another_turn_is_not_carried_into_this_one() {
    // A region may hold a result whose call belongs to an earlier turn. It
    // cannot be paired on the wire, so the repair drops it rather than send a
    // duplicate under a call this turn never made.
    let mut messages = vec![
        assistant_with_tool_uses(&["tool-1"]),
        tool_result("tool-1", "a"),
        assistant_with_tool_uses(&["tool-2"]),
        tool_result("tool-1", "a"),
    ];
    close_unanswered_tool_uses(&mut messages);
    let ids: Vec<&str> = messages[3]
        .blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec!["tool-2"], "only this turn's call is answered");
    let mut repaired = messages.clone();
    close_unanswered_tool_uses(&mut repaired);
    assert_eq!(repaired, messages, "a second repair changes nothing");
}

#[test]
fn strays_keep_the_kind_and_origin_of_their_message() {
    let origin = |model: &str| {
        Some(lofi_types::ModelOrigin {
            provider: "plexus".to_string(),
            model: model.to_string(),
            api: lofi_types::Api::AnthropicMessages,
        })
    };
    let legacy = |kind: PromptKind,
                  origin: Option<lofi_types::ModelOrigin>,
                  result: &str,
                  text: &str| Message {
        origin,
        role: Role::User,
        blocks: vec![
            ContentBlock::ToolResult {
                tool_use_id: result.to_string(),
                content: "ok".to_string(),
                is_error: false,
                images: Vec::new(),
            },
            ContentBlock::Text {
                text: text.to_string(),
            },
        ],
        kind,
    };
    let mut messages = vec![
        assistant_with_tool_uses(&["tool-1", "tool-2"]),
        legacy(PromptKind::User, origin("parent"), "tool-1", "typed"),
        legacy(PromptKind::Notice, origin("job"), "tool-2", "job finished"),
    ];
    close_unanswered_tool_uses(&mut messages);
    assert_eq!(messages.len(), 4, "one result message and two strays");
    assert_eq!(messages[1].role, Role::Tool);
    assert_eq!(messages[1].blocks.len(), 2);
    let stray = |index: usize| {
        (
            messages[index].kind,
            messages[index].origin.as_ref().map(|o| o.model.clone()),
        )
    };
    assert_eq!(stray(2), (PromptKind::User, Some("parent".to_string())));
    assert_eq!(stray(3), (PromptKind::Notice, Some("job".to_string())));
    assert!(matches!(
        &messages[3].blocks[0],
        ContentBlock::Text { text } if text == "job finished"
    ));
}

#[test]
fn no_user_message_carries_a_result_after_the_repair() {
    // The provider mappers serialize results only for the tool role, so a
    // repaired history that leaves one in a user message sends a turn whose
    // call has no answer on the wire.
    let mut messages = vec![
        assistant_with_tool_uses(&["tool-1"]),
        tool_result("tool-1", "a"),
        tool_result("tool-2", "b"),
        assistant_with_tool_uses(&["tool-3"]),
        Message {
            origin: None,
            role: Role::User,
            blocks: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tool-3".to_string(),
                    content: "ok".to_string(),
                    is_error: false,
                    images: Vec::new(),
                },
                ContentBlock::Text {
                    text: "note".to_string(),
                },
            ],
            kind: PromptKind::default(),
        },
        assistant_with_tool_uses(&["tool-4"]),
    ];
    close_unanswered_tool_uses(&mut messages);
    let strayed: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::User
                && message
                    .blocks
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        })
        .map(|(index, _)| index)
        .collect();
    assert!(
        strayed.is_empty(),
        "results left in user messages: {strayed:?}"
    );
}

#[test]
fn a_closed_call_joins_the_results_that_are_there() {
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
        tool_result("tool-2", "ok"),
    ];
    close_unanswered_tool_uses(&mut messages);
    assert_eq!(
        messages.len(),
        2,
        "one result message for one assistant turn"
    );
    assert_eq!(messages[1].blocks.len(), 2);
    let closed = messages[1].blocks.iter().find_map(|block| match block {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
            ..
        } if tool_use_id == "tool-1" => Some((content.clone(), *is_error)),
        _ => None,
    });
    let Some((content, is_error)) = closed else {
        panic!("tool-1 gets a result");
    };
    assert!(is_error, "the missing call is closed as an error");
    assert!(content.contains("cancelled"));
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
