#![allow(clippy::unwrap_used)]

use lofi_types::{
    ContentBlock, Message, Model, PromptKind, Role, ServiceTier, StreamingEvent, ThinkingLevel,
};

use super::super::ProtocolIr;
use super::events::{map_openai_responses_event, ResponsesMapperState};
use super::request::{build_openai_responses_request, to_openai_responses_input};
use super::OpenAiResponsesIr;

use serde_json::json;

fn model() -> Model {
    Model {
        id: "gpt-4o".to_string(),
        name: "gpt".to_string(),
        provider: "p".to_string(),
        api: lofi_types::Api::OpenAiResponses,
        reasoning: false,
        thinking: lofi_types::ThinkingLevel::Off,
        service_tier: lofi_types::ServiceTier::Auto,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    }
}

#[test]
fn request_uses_input_array() {
    let req = build_openai_responses_request(&model(), &[], &[]);
    assert_eq!(req["model"], "gpt-4o");
    assert_eq!(req["stream"], true);
    assert_eq!(req["store"], false);
    assert!(req.get("input").is_some());
    assert_eq!(
        req["prompt_cache_key"].as_str().map(str::len),
        Some("lofi:".len() + 16)
    );
    assert!(req.get("tools").is_none());
    assert!(req.get("reasoning").is_none());
    assert_eq!(req["include"], json!(["reasoning.encrypted_content"]));
}

#[test]
fn request_cache_key_is_stable_while_history_is_appended() {
    let system = Message {
        origin: None,
        role: lofi_types::Role::System,
        blocks: vec![lofi_types::ContentBlock::Text {
            text: "stable instructions".to_string(),
        }],
        kind: PromptKind::default(),
    };
    let first_user = Message {
        origin: None,
        role: lofi_types::Role::User,
        blocks: vec![lofi_types::ContentBlock::Text {
            text: "initial prompt".to_string(),
        }],
        kind: PromptKind::default(),
    };
    let appended = Message {
        origin: None,
        role: lofi_types::Role::Assistant,
        blocks: vec![lofi_types::ContentBlock::Text {
            text: "new response".to_string(),
        }],
        kind: PromptKind::default(),
    };
    let first =
        build_openai_responses_request(&model(), &[system.clone(), first_user.clone()], &[]);
    let second = build_openai_responses_request(&model(), &[system, first_user, appended], &[]);
    assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
}

#[test]
fn request_cache_key_changes_when_prefix_is_rebuilt() {
    let message = |role, text: &str| Message {
        origin: None,
        role,
        blocks: vec![lofi_types::ContentBlock::Text {
            text: text.to_string(),
        }],
        kind: PromptKind::default(),
    };
    let first = build_openai_responses_request(
        &model(),
        &[
            message(lofi_types::Role::System, "shared system"),
            message(lofi_types::Role::User, "prefix one"),
        ],
        &[],
    );
    let second = build_openai_responses_request(
        &model(),
        &[
            message(lofi_types::Role::System, "shared system"),
            message(lofi_types::Role::User, "prefix two"),
        ],
        &[],
    );
    assert_ne!(first["prompt_cache_key"], second["prompt_cache_key"]);
}

#[test]
fn request_forwards_configured_reasoning_effort_verbatim() {
    for (level, expected) in [
        (ThinkingLevel::XHigh, "xhigh"),
        (ThinkingLevel::Custom("minimal".to_string()), "minimal"),
    ] {
        let mut model = model();
        model.thinking = level;
        let req = build_openai_responses_request(&model, &[], &[]);
        assert_eq!(
            req["reasoning"],
            json!({"effort": expected, "summary": "auto"})
        );
    }
}

#[test]
fn request_forwards_service_tier_but_omits_auto() {
    for tier in [ServiceTier::Flex, ServiceTier::Priority] {
        let mut model = model();
        model.service_tier = tier.clone();
        let req = build_openai_responses_request(&model, &[], &[]);
        assert_eq!(req["service_tier"], tier.as_str());
    }
    assert!(build_openai_responses_request(&model(), &[], &[])
        .get("service_tier")
        .is_none());
}

#[test]
fn maps_text_delta() {
    let ev = json!({"type":"response.output_text.delta","delta":"hi"});
    assert_eq!(
        map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
        vec![StreamingEvent::TextDelta("hi".to_string())]
    );
}

#[test]
fn maps_tool_use_lifecycle() {
    let mut state = ResponsesMapperState::default();
    let added = json!({
        "type":"response.output_item.added",
        "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
    });
    assert_eq!(
        map_openai_responses_event(&added, &mut state).unwrap(),
        vec![StreamingEvent::ToolUseStart {
            id: "call_1".to_string(),
            name: "exec".to_string()
        }]
    );
    let d = json!({
        "type":"response.function_call_arguments.delta",
        "item_id":"fc_1",
        "delta":"{\"x\":1}"
    });
    assert_eq!(
        map_openai_responses_event(&d, &mut state).unwrap(),
        vec![StreamingEvent::ToolUseInputDelta {
            id: "call_1".to_string(),
            delta: "{\"x\":1}".to_string()
        }]
    );
    let done = json!({
        "type":"response.output_item.done",
        "item":{"type":"function_call","call_id":"call_1","name":"exec","arguments":"{\"x\":1}"}
    });
    assert_eq!(
        map_openai_responses_event(&done, &mut state).unwrap(),
        vec![
            StreamingEvent::ToolUseInputComplete {
                id: "call_1".to_string(),
                input: "{\"x\":1}".to_string()
            },
            StreamingEvent::ToolUseEnd {
                id: "call_1".to_string()
            }
        ]
    );
}

#[test]
fn completed_tool_item_recovers_arguments_when_deltas_are_absent() {
    let done = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"function_call",
            "id":"fc_1",
            "call_id":"call_1",
            "name":"exec",
            "arguments":"{\"code\":\"return 1\"}"
        }
    });

    let events = map_openai_responses_event(&done, &mut ResponsesMapperState::default()).unwrap();
    assert_eq!(
        events,
        vec![
            StreamingEvent::ToolUseStart {
                id: "call_1".to_string(),
                name: "exec".to_string(),
            },
            StreamingEvent::ToolUseInputComplete {
                id: "call_1".to_string(),
                input: "{\"code\":\"return 1\"}".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "call_1".to_string(),
            },
        ]
    );
    assert_eq!(
        crate::assemble_message(&events).blocks,
        vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "exec".to_string(),
            input: json!({"code": "return 1"}),
        }]
    );
}

#[test]
fn argument_done_event_recovers_missing_deltas() {
    let mut state = ResponsesMapperState::default();
    let added = json!({
        "type":"response.output_item.added",
        "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
    });
    map_openai_responses_event(&added, &mut state).unwrap();
    let done = json!({
        "type":"response.function_call_arguments.done",
        "item_id":"fc_1",
        "name":"exec",
        "arguments":"{\"code\":\"return 1\"}"
    });

    assert_eq!(
        map_openai_responses_event(&done, &mut state).unwrap(),
        vec![StreamingEvent::ToolUseInputComplete {
            id: "call_1".to_string(),
            input: "{\"code\":\"return 1\"}".to_string(),
        }]
    );
}

#[test]
fn completed_tool_item_replaces_streamed_arguments() {
    let mut state = ResponsesMapperState::default();
    let added = json!({
        "type":"response.output_item.added",
        "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
    });
    let delta = json!({
        "type":"response.function_call_arguments.delta",
        "item_id":"fc_1",
        "delta":"{\"code\":\"return 1\"}"
    });
    map_openai_responses_event(&added, &mut state).unwrap();
    map_openai_responses_event(&delta, &mut state).unwrap();
    let done = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"function_call",
            "id":"fc_1",
            "call_id":"call_1",
            "name":"exec",
            "arguments":"{\"code\":\"return 1\"}"
        }
    });

    assert_eq!(
        map_openai_responses_event(&done, &mut state).unwrap(),
        vec![
            StreamingEvent::ToolUseInputComplete {
                id: "call_1".to_string(),
                input: "{\"code\":\"return 1\"}".to_string(),
            },
            StreamingEvent::ToolUseEnd {
                id: "call_1".to_string(),
            }
        ]
    );
}

#[test]
fn argument_delta_after_done_is_ignored() {
    let mut state = ResponsesMapperState::default();
    let added = json!({
        "type":"response.output_item.added",
        "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
    });
    let done = json!({
        "type":"response.function_call_arguments.done",
        "item_id":"fc_1",
        "arguments":"{\"code\":\"return 1\"}"
    });
    let late = json!({
        "type":"response.function_call_arguments.delta",
        "item_id":"fc_1",
        "delta":"late"
    });
    map_openai_responses_event(&added, &mut state).unwrap();
    map_openai_responses_event(&done, &mut state).unwrap();

    assert!(map_openai_responses_event(&late, &mut state)
        .unwrap()
        .is_empty());
}

#[test]
fn completed_tool_item_repairs_partial_streamed_arguments() {
    let mut state = ResponsesMapperState::default();
    let added = json!({
        "type":"response.output_item.added",
        "item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"exec","arguments":""}
    });
    let partial = json!({
        "type":"response.function_call_arguments.delta",
        "item_id":"fc_1",
        "delta":"{\"code\":"
    });
    let done = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"function_call",
            "id":"fc_1",
            "call_id":"call_1",
            "name":"exec",
            "arguments":"{\"code\":\"return 1\"}"
        }
    });
    let mut events = map_openai_responses_event(&added, &mut state).unwrap();
    events.extend(map_openai_responses_event(&partial, &mut state).unwrap());
    events.extend(map_openai_responses_event(&done, &mut state).unwrap());

    assert_eq!(
        crate::assemble_message(&events).blocks,
        vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "exec".to_string(),
            input: json!({"code": "return 1"}),
        }]
    );
}

#[test]
fn maps_completed_reasoning_item_when_deltas_are_absent() {
    let ev = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"reasoning",
            "id":"rs_1",
            "summary":[
                {"type":"summary_text","text":"Checked the inputs."},
                {"type":"summary_text","text":"Selected the result."}
            ]
        }
    });
    assert_eq!(
        map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
        vec![StreamingEvent::ThinkingDelta(
            "Checked the inputs.\n\nSelected the result.".to_string()
        )]
    );
}

#[test]
fn completed_reasoning_item_preserves_raw_placeholder_parts() {
    let ev = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"reasoning",
            "id":"rs_1",
            "summary":[
                {"type":"summary_text","text":" **Checking**\n<!-- --> "},
                {"type":"summary_text","text":"Kept <!-- --> literally."},
                {"type":"summary_text","text":" **Done**\nResult "}
            ]
        }
    });
    assert_eq!(
        map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
        vec![StreamingEvent::ThinkingDelta(
            " **Checking**\n<!-- --> \n\nKept <!-- --> literally.\n\n **Done**\nResult "
                .to_string()
        )]
    );
}

#[test]
fn summary_part_done_separates_streamed_parts() {
    let mut state = ResponsesMapperState::default();
    let first = json!({
        "type":"response.reasoning_summary_text.delta",
        "item_id":"rs_1",
        "delta":"First"
    });
    let part_done = json!({
        "type":"response.reasoning_summary_part.done",
        "item_id":"rs_1"
    });
    let second = json!({
        "type":"response.reasoning_summary_text.delta",
        "item_id":"rs_1",
        "delta":"Second"
    });
    map_openai_responses_event(&first, &mut state).unwrap();
    assert_eq!(
        map_openai_responses_event(&part_done, &mut state).unwrap(),
        vec![StreamingEvent::ThinkingDelta("\n\n".to_string())]
    );
    assert_eq!(
        map_openai_responses_event(&second, &mut state).unwrap(),
        vec![StreamingEvent::ThinkingDelta("Second".to_string())]
    );
}

#[test]
fn completed_reasoning_item_does_not_duplicate_streamed_summary() {
    let mut state = ResponsesMapperState::default();
    let delta = json!({
        "type":"response.reasoning_summary_text.delta",
        "item_id":"rs_1",
        "delta":"Checked the inputs."
    });
    map_openai_responses_event(&delta, &mut state).unwrap();
    let done = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"reasoning",
            "id":"rs_1",
            "summary":[{"type":"summary_text","text":"Checked the inputs."}]
        }
    });
    assert!(map_openai_responses_event(&done, &mut state)
        .unwrap()
        .is_empty());
}

#[test]
fn completed_reasoning_item_emits_encrypted_content_as_signature() {
    let ev = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"reasoning",
            "id":"rs_1",
            "encrypted_content":"enc_blob",
            "summary":[{"type":"summary_text","text":"Checked the inputs."}]
        }
    });
    assert_eq!(
        map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap(),
        vec![
            StreamingEvent::ThinkingDelta("Checked the inputs.".to_string()),
            StreamingEvent::ThinkingSignature("enc_blob".to_string()),
        ]
    );
}

#[test]
fn streamed_reasoning_still_captures_encrypted_content() {
    let mut state = ResponsesMapperState::default();
    let delta = json!({
        "type":"response.reasoning_summary_text.delta",
        "item_id":"rs_1",
        "delta":"Checked the inputs."
    });
    map_openai_responses_event(&delta, &mut state).unwrap();
    let done = json!({
        "type":"response.output_item.done",
        "item":{
            "type":"reasoning",
            "id":"rs_1",
            "encrypted_content":"enc_blob",
            "summary":[{"type":"summary_text","text":"Checked the inputs."}]
        }
    });
    assert_eq!(
        map_openai_responses_event(&done, &mut state).unwrap(),
        vec![StreamingEvent::ThinkingSignature("enc_blob".to_string())]
    );
}

#[test]
fn encrypted_reasoning_replays_before_function_call() {
    let msgs = [Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![
            ContentBlock::Thinking {
                text: "Need a tool.".to_string(),
                signature: Some("enc_blob".to_string()),
                redacted: false,
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "exec".to_string(),
                input: json!({"code": "1"}),
            },
        ],
        kind: PromptKind::default(),
    }];
    let input = to_openai_responses_input(&msgs);
    assert_eq!(
        input[0],
        json!({
            "type": "reasoning",
            "encrypted_content": "enc_blob",
            "summary": [{"type": "summary_text", "text": "Need a tool."}],
        })
    );
    assert_eq!(input[1]["type"], json!("function_call"));
    assert_eq!(input[1]["call_id"], json!("call_1"));
}

#[test]
fn request_converts_foreign_reasoning_without_replaying_encrypted_content() {
    let target = model();
    let mut source = target.clone();
    source.id = "gpt-5".to_string();
    let messages = [Message {
        origin: Some(lofi_types::ModelOrigin::from(&source)),
        role: Role::Assistant,
        blocks: vec![
            ContentBlock::Thinking {
                text: "Need a tool.".to_string(),
                signature: Some("enc_blob".to_string()),
                redacted: false,
            },
            ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "exec".to_string(),
                input: json!({"code": "1"}),
            },
        ],
        kind: PromptKind::default(),
    }];

    let request = OpenAiResponsesIr::build_request(&target, &messages, &[]);
    let input = request["input"].as_array().unwrap();

    assert_eq!(input.len(), 2);
    assert_eq!(input[0]["type"], "message");
    assert_eq!(input[0]["content"][0]["text"], "Need a tool.");
    assert_eq!(input[1]["type"], "function_call");
    assert!(input
        .iter()
        .all(|item| item.get("encrypted_content").is_none()));
}

#[test]
fn plaintext_thinking_is_not_replayed() {
    let msgs = [Message {
        origin: None,
        role: Role::Assistant,
        blocks: vec![
            ContentBlock::Thinking {
                text: "local only".to_string(),
                signature: None,
                redacted: false,
            },
            ContentBlock::Text {
                text: "done".to_string(),
            },
        ],
        kind: PromptKind::default(),
    }];
    let input = to_openai_responses_input(&msgs);
    assert_eq!(input.len(), 1);
    assert_eq!(input[0]["type"], json!("message"));
    assert_eq!(
        input[0]["content"][0],
        json!({"type": "output_text", "text": "done"})
    );
}

#[test]
fn failed_event_is_error() {
    let ev = json!({"type":"response.failed","error":{"message":"boom"}});
    let r = map_openai_responses_event(&ev, &mut ResponsesMapperState::default());
    assert!(r.is_err());
}

#[test]
fn incomplete_event_maps_reason_into_done_instead_of_erroring() {
    for (reason, expected) in [
        ("max_output_tokens", lofi_types::StopReason::MaxTokens),
        ("max_tool_calls", lofi_types::StopReason::MaxTokens),
        ("content_filter", lofi_types::StopReason::Other),
    ] {
        let ev = json!({
            "type": "response.incomplete",
            "response": {"status": "incomplete", "incomplete_details": {"reason": reason}}
        });
        let mut state = ResponsesMapperState::default();
        let done = map_openai_responses_event(&ev, &mut state).unwrap();
        let StreamingEvent::Done { stop_reason, .. } = done.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(stop_reason, Some(expected), "reason {reason}");
        // The event is terminal: the EOF guard must accept it.
        assert!(OpenAiResponsesIr::on_eof(&state).is_ok());
    }
}

#[test]
fn maps_completed_usage() {
    let ev = json!({
        "type":"response.completed",
        "response":{"usage":{"input_tokens":3,"output_tokens":7,"input_tokens_details":{"cached_tokens":1}}}
    });
    let done = map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap();
    let StreamingEvent::Done { usage: u, .. } = done.into_iter().next().unwrap() else {
        panic!("expected Done");
    };
    assert_eq!(u.input_tokens, 2);
    assert_eq!(u.output_tokens, 7);
    assert_eq!(u.cache_read_tokens, 1);
}

#[test]
fn maps_incomplete_details_reason_into_done() {
    for (details, expected) in [
        (None, lofi_types::StopReason::EndTurn),
        (Some("max_output_tokens"), lofi_types::StopReason::MaxTokens),
        (Some("content_filter"), lofi_types::StopReason::Other),
    ] {
        let ev = match details {
            Some(reason) => json!({
                "type":"response.completed",
                "response":{"status":"incomplete","incomplete_details":{"reason":reason}}
            }),
            None => json!({
                "type":"response.completed",
                "response":{"status":"completed"}
            }),
        };
        let done = map_openai_responses_event(&ev, &mut ResponsesMapperState::default()).unwrap();
        let StreamingEvent::Done { stop_reason, .. } = done.into_iter().next().unwrap() else {
            panic!("expected Done");
        };
        assert_eq!(stop_reason, Some(expected), "details {details:?}");
    }
}

#[test]
fn tool_result_with_images_serializes_output_as_items() {
    let msgs = [Message {
        origin: None,
        role: Role::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: "c1".to_string(),
            content: "read image.png".to_string(),
            is_error: false,
            images: vec![lofi_types::ToolResultImage {
                bytes: vec![1, 2, 3],
                media_type: "image/png".to_string(),
            }],
        }],
        kind: PromptKind::default(),
    }];
    let input = to_openai_responses_input(&msgs);
    // The function_call_output keeps a plain-string output; the image is
    // carried on a following user `message` in the universally supported
    // position, because some Responses gateways drop images inside an
    // array-valued tool output.
    assert_eq!(input[0]["type"], json!("function_call_output"));
    assert_eq!(input[0]["output"], json!("read image.png"));
    assert_eq!(input[1]["type"], json!("message"));
    assert_eq!(input[1]["role"], json!("user"));
    assert_eq!(
        input[1]["content"][0],
        json!({
            "type": "input_image",
            "detail": "auto",
            "image_url": "data:image/png;base64,AQID",
        })
    );
}

#[test]
fn tool_result_without_images_stays_string() {
    let msgs = [Message {
        origin: None,
        role: Role::Tool,
        blocks: vec![ContentBlock::ToolResult {
            tool_use_id: "c1".to_string(),
            content: "plain".to_string(),
            is_error: false,
            images: Vec::new(),
        }],
        kind: PromptKind::default(),
    }];
    let input = to_openai_responses_input(&msgs);
    assert_eq!(input[0]["output"], json!("plain"));
}

#[test]
fn user_image_block_serializes_as_input_image() {
    let msgs = [Message {
        origin: None,
        role: Role::User,
        blocks: vec![
            ContentBlock::Text {
                text: "describe".to_string(),
            },
            ContentBlock::Image {
                bytes: vec![1, 2, 3],
                media_type: "image/jpeg".to_string(),
            },
        ],
        kind: PromptKind::default(),
    }];
    let req = build_openai_responses_request(&model(), &msgs, &[]);
    let content = &req["input"][0]["content"];
    assert_eq!(
        content[0],
        json!({"type": "input_text", "text": "describe"})
    );
    assert_eq!(
        content[1],
        json!({
            "type": "input_image",
            "image_url": "data:image/jpeg;base64,AQID",
        })
    );
}
