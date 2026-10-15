use super::*;
use indexmap::IndexMap;
use serde::Serialize;

#[test]
fn compact_block_native_records_only_from_tool_call() {
    let rec = NativeToolRecord {
        parent: "t1".into(),
        call_id: 1,
        name: "read".into(),
        args: "a.rs".into(),
        result: String::new(),
        is_error: false,
    };
    let call = CompactBlock::ToolCall {
        id: "t1".into(),
        code: String::new(),
        label: None,
        native: vec![rec.clone()],
    };
    assert_eq!(call.native_records().len(), 1);
    assert_eq!(call.native_records()[0].name, "read");
    assert!(CompactBlock::User { text: "hi".into() }
        .native_records()
        .is_empty());
    assert!(CompactBlock::Assistant { text: "ok".into() }
        .native_records()
        .is_empty());
    assert!(CompactBlock::ToolResult {
        id: "t1".into(),
        text: String::new(),
        is_error: false,
    }
    .native_records()
    .is_empty());
}

fn round_trip<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(value, &back);
}

#[test]
fn api_as_str_parse_roundtrip() {
    for api in [
        Api::OpenAiCompletions,
        Api::OpenAiResponses,
        Api::AnthropicMessages,
    ] {
        assert_eq!(Api::parse(api.as_str()), Some(api));
    }
    assert_eq!(Api::parse("bogus"), None);
    assert_eq!(Api::OpenAiCompletions.label(), "OpenAI Chat Completions");
}

#[test]
fn run_model_label_includes_service_tier() {
    let m = RunModel {
        provider: "openai".into(),
        id: "gpt-5.6-sol".into(),
        thinking: ThinkingLevel::High,
        service_tier: ServiceTier::Flex,
    };
    assert_eq!(m.label(), "openai/gpt-5.6-sol:high@flex");

    let auto = RunModel {
        service_tier: ServiceTier::Auto,
        ..m.clone()
    };
    assert_eq!(auto.label(), "openai/gpt-5.6-sol:high");

    let off = RunModel {
        thinking: ThinkingLevel::Off,
        service_tier: ServiceTier::Priority,
        ..m.clone()
    };
    assert_eq!(off.label(), "openai/gpt-5.6-sol@priority");
}

#[test]
fn run_model_parse_round_trips_service_tier() {
    for label in [
        "openai/gpt-5.6-sol",
        "openai/gpt-5.6-sol:high",
        "openai/gpt-5.6-sol:high@flex",
        "openai/gpt-5.6-sol@priority",
    ] {
        let parsed: RunModel = label.into();
        assert_eq!(parsed.label(), label, "round trip failed for {label}");
    }
}

#[test]
fn run_model_custom_tier_round_trips() {
    let parsed: RunModel = "openai/gpt-5.6-sol:high@vip".into();
    assert_eq!(parsed.id, "gpt-5.6-sol");
    assert_eq!(parsed.thinking, ThinkingLevel::High);
    assert_eq!(parsed.service_tier, ServiceTier::Custom("vip".into()));
    assert_eq!(parsed.label(), "openai/gpt-5.6-sol:high@vip");
}

#[test]
fn service_tier_parse_and_round_trip() {
    for (s, tier) in [
        ("auto", ServiceTier::Auto),
        ("flex", ServiceTier::Flex),
        ("priority", ServiceTier::Priority),
    ] {
        assert_eq!(ServiceTier::parse(s), Some(tier.clone()));
        assert_eq!(tier.as_str(), s);
    }
    assert_eq!(ServiceTier::parse("bogus"), None);
    let custom = ServiceTier::Custom("vip".into());
    assert_eq!(custom.as_str(), "vip");
    round_trip(&custom);
    let json = serde_json::to_string(&custom).unwrap();
    assert_eq!(json, "\"vip\"");
}

#[test]
fn api_serde_snake_case() {
    let json = serde_json::to_string(&Api::AnthropicMessages).unwrap();
    assert_eq!(json, "\"anthropic_messages\"");
    let back: Api = serde_json::from_str(&json).unwrap();
    assert_eq!(back, Api::AnthropicMessages);
}

#[test]
fn content_block_round_trips() {
    round_trip(&ContentBlock::Text {
        text: "hi".to_string(),
    });
    round_trip(&ContentBlock::ToolUse {
        id: "t1".to_string(),
        name: "exec".to_string(),
        input: serde_json::json!({"code": "1+1"}),
    });
    round_trip(&ContentBlock::ToolResult {
        tool_use_id: "t1".to_string(),
        content: "2".to_string(),
        is_error: false,
        images: Vec::new(),
    });
    round_trip(&ContentBlock::ToolResult {
        tool_use_id: "t2".to_string(),
        content: "img".to_string(),
        is_error: false,
        images: vec![ToolResultImage {
            bytes: vec![0x89, 0x50],
            media_type: "image/png".to_string(),
        }],
    });
    round_trip(&ContentBlock::Thinking {
        text: "hmm".to_string(),
        signature: None,
        redacted: false,
    });
    round_trip(&ContentBlock::Thinking {
        text: "summary".to_string(),
        signature: Some("encrypted".to_string()),
        redacted: false,
    });
    round_trip(&ContentBlock::Image {
        bytes: vec![0xFF, 0xD8, 0xFF, 0xD9],
        media_type: "image/jpeg".to_string(),
    });
}

#[test]
fn image_block_serializes_bytes_as_base64() {
    let json = serde_json::to_value(&ContentBlock::Image {
        bytes: vec![1, 2, 3],
        media_type: "image/jpeg".to_string(),
    })
    .unwrap();
    assert_eq!(json["type"], "image");
    assert_eq!(json["bytes"], "AQID");
    assert_eq!(json["media_type"], "image/jpeg");
    let back: ContentBlock = serde_json::from_value(json).unwrap();
    assert_eq!(
        back,
        ContentBlock::Image {
            bytes: vec![1, 2, 3],
            media_type: "image/jpeg".to_string(),
        }
    );
}

#[test]
fn retry_config_defaults() {
    let cfg = RetryConfig::default();
    assert_eq!(cfg.max_retries, 10);
    assert_eq!(cfg.base_delay_ms, 2000);
    assert_eq!(cfg.max_delay_ms, 60_000);
}

#[test]
fn retry_config_serde_omitted_uses_defaults() {
    #[derive(serde::Deserialize)]
    struct Wrap {
        #[serde(default)]
        retry: RetryConfig,
    }
    let w: Wrap = serde_json::from_str("{}").unwrap();
    assert_eq!(w.retry, RetryConfig::default());
}

#[test]
fn retry_config_serde_partial_override() {
    #[derive(serde::Deserialize)]
    struct Wrap {
        #[serde(default)]
        retry: RetryConfig,
    }
    let w: Wrap = serde_json::from_str("{\"retry\":{\"max_retries\":0}}").unwrap();
    assert_eq!(w.retry.max_retries, 0);
    assert_eq!(w.retry.base_delay_ms, 2000);
    assert_eq!(w.retry.max_delay_ms, 60_000);
}

#[test]
fn content_block_text_tagged_shape() {
    let block = ContentBlock::Text {
        text: "hi".to_string(),
    };
    let json = serde_json::to_value(&block).unwrap();
    assert_eq!(json["type"], "text");
    assert_eq!(json["text"], "hi");
}

#[test]
fn message_round_trips() {
    round_trip(&Message {
        origin: None,
        role: Role::User,
        blocks: vec![ContentBlock::Text {
            text: "hello".to_string(),
        }],
        kind: PromptKind::User,
    });
    // Default-User kind must skip in the wire form; explicit-Notice must round-trip.
    let user_json = serde_json::to_value(&Message {
        origin: None,
        role: Role::User,
        blocks: vec![],
        kind: PromptKind::User,
    })
    .unwrap();
    assert!(user_json.get("kind").is_none());
    let notice_json = serde_json::to_value(&Message {
        origin: None,
        role: Role::User,
        blocks: vec![],
        kind: PromptKind::Notice,
    })
    .unwrap();
    assert_eq!(notice_json["kind"], "notice");
    let parsed: Message = serde_json::from_str(r#"{"role":"user","blocks":[]}"#).unwrap();
    assert_eq!(
        parsed.kind,
        PromptKind::User,
        "absent kind defaults to User"
    );
}

#[test]
fn message_lifts_legacy_thinking_origin() {
    let parsed: Message = serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "blocks": [{
            "type": "thinking",
            "text": "summary",
            "signature": "encrypted",
            "origin": {
                "provider": "openai",
                "model": "gpt-5",
                "api": "openai_responses"
            }
        }]
    }))
    .unwrap();

    assert_eq!(
        parsed.origin,
        Some(ModelOrigin {
            provider: "openai".to_string(),
            model: "gpt-5".to_string(),
            api: Api::OpenAiResponses,
        })
    );
    let json = serde_json::to_value(parsed).unwrap();
    assert!(json["blocks"][0].get("origin").is_none());
}

#[test]
fn message_does_not_lift_conflicting_legacy_thinking_origins() {
    let parsed: Message = serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "blocks": [
            {
                "type": "thinking",
                "text": "first",
                "signature": "encrypted-1",
                "origin": {
                    "provider": "openai",
                    "model": "gpt-5",
                    "api": "openai_responses"
                }
            },
            {
                "type": "thinking",
                "text": "second",
                "signature": "encrypted-2",
                "origin": {
                    "provider": "openai",
                    "model": "gpt-5-mini",
                    "api": "openai_responses"
                }
            }
        ]
    }))
    .unwrap();

    assert_eq!(parsed.origin, None);
}

#[test]
fn message_ignores_retired_origin_on_non_thinking_blocks() {
    let parsed: Message = serde_json::from_value(serde_json::json!({
        "role": "assistant",
        "blocks": [{
            "type": "text",
            "text": "answer",
            "origin": "not a model origin"
        }]
    }))
    .unwrap();

    assert_eq!(parsed.origin, None);
    assert_eq!(
        parsed.blocks,
        vec![ContentBlock::Text {
            text: "answer".to_string()
        }]
    );
}

#[test]
fn turn_end_without_stop_reason_still_loads() {
    // Transcripts written before stop reasons were recorded carry no
    // such field; keep them loading with None.
    let parsed: SessionEvent =
        serde_json::from_str(r#"{"type":"turn_end","elapsed_ms":1,"cost":0.0,"usage":{}}"#)
            .unwrap();
    match parsed.kind {
        SessionEventKind::TurnEnd { stop_reason, .. } => assert_eq!(stop_reason, None),
        other => panic!("expected turn_end, got {other:?}"),
    }
}

#[test]
fn streaming_event_round_trips() {
    round_trip(&StreamingEvent::TextDelta("x".to_string()));
    round_trip(&StreamingEvent::ToolUseStart {
        id: "t1".to_string(),
        name: "exec".to_string(),
    });
    round_trip(&StreamingEvent::ToolUseInputComplete {
        id: "t1".to_string(),
        input: "{\"code\":\"return 1\"}".to_string(),
    });
    round_trip(&StreamingEvent::Done {
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        stop_reason: Some(StopReason::EndTurn),
    });
    round_trip(&StreamingEvent::Done {
        usage: Usage::default(),
        stop_reason: None,
    });
    round_trip(&StreamingEvent::Error("boom".to_string()));
}

#[test]
fn usage_defaults_to_zero() {
    let json = "{}";
    let usage: Usage = serde_json::from_str(json).unwrap();
    assert_eq!(usage, Usage::default());
}

#[test]
fn config_round_trips() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderConfig {
            api_type: Some(Api::OpenAiCompletions),
            api_types: IndexMap::new(),
            base_url: Some("https://api.openai.com".to_string()),
            pricing_convention: PricingConvention::PerToken,
            pricing_field_mappings: PricingFieldMappings::default(),
            env_name: Some("OPENAI_API_KEY".to_string()),
            api_key: None,
            headers: None,
            models: {
                let mut m = IndexMap::new();
                m.insert(
                    "gpt-4o".to_string(),
                    ModelConfig {
                        name: None,
                        api_type: None,
                        reasoning: None,
                        supports_image: Some(true),
                        context_window: Some(128_000),
                        max_tokens: None,
                        thinking_levels: Vec::new(),
                        thinking_level: None,
                        service_tiers: Vec::new(),
                        service_tier: None,
                        auto_continue: AutoContinueConfig::default(),
                        base_url: None,
                        input_price: None,
                        output_price: None,
                        cache_read_price: None,
                        cache_write_price: None,
                        per_request_price: None,
                    },
                );
                m
            },
            auto_models: None,
            no_auth: false,
            response_start_timeout_ms: 90_000,
            thinking_level: None,
            thinking_levels: Vec::new(),
            service_tier: None,
            service_tiers: Vec::new(),
            auto_continue: AutoContinueConfig::default(),
        },
    );
    let cfg = Config {
        providers,
        ..Config::default()
    };
    round_trip(&cfg);
}

#[test]
fn thinking_level_serde_lowercase() {
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::XHigh).unwrap(),
        concat!('"', "xhigh", '"')
    );
    assert_eq!(
        serde_json::to_string(&ThinkingLevel::Off).unwrap(),
        concat!('"', "off", '"')
    );
    let l: ThinkingLevel = serde_json::from_str(concat!('"', "medium", '"')).unwrap();
    assert_eq!(l, ThinkingLevel::Medium);

    let future: ThinkingLevel = serde_json::from_str(r#""minimal""#).unwrap();
    assert_eq!(future.as_str(), "minimal");
    assert_eq!(serde_json::to_string(&future).unwrap(), r#""minimal""#);
}

#[test]
fn api_default_base_url_and_path() {
    assert_eq!(
        Api::OpenAiCompletions.default_base_url(),
        "https://api.openai.com"
    );
    assert_eq!(
        Api::OpenAiResponses.default_base_url(),
        "https://api.openai.com"
    );
    assert_eq!(
        Api::AnthropicMessages.default_base_url(),
        "https://api.anthropic.com"
    );
    assert_eq!(
        Api::OpenAiCompletions.default_path(),
        "/v1/chat/completions"
    );
    assert_eq!(Api::OpenAiResponses.default_path(), "/v1/responses");
    assert_eq!(Api::AnthropicMessages.default_path(), "/v1/messages");
}

#[test]
fn api_type_accepts_kebab_case() {
    assert_eq!(Api::parse("openai-responses"), Some(Api::OpenAiResponses));
    assert_eq!(
        Api::parse("anthropic-messages"),
        Some(Api::AnthropicMessages)
    );
    assert_eq!(Api::parse("openai_responses"), Some(Api::OpenAiResponses));
}

#[test]
fn compaction_hard_threshold_reserved() {
    let cfg = CompactionConfig::default();
    assert_eq!(cfg.hard_threshold(200_000), Some(180_000));
    let with_caps = CompactionConfig {
        auto: AutoCompactConfig {
            max_context_tokens: Some(100_000),
            context_ratio: Some(0.5),
            ..AutoCompactConfig::default()
        },
        ..CompactionConfig::default()
    };
    assert_eq!(with_caps.hard_threshold(200_000), Some(180_000));
    assert_eq!(cfg.hard_threshold(10_000), None);
    assert_eq!(cfg.hard_threshold(0), None);
}

#[test]
fn disabled_auto_compaction_has_no_soft_threshold() {
    let cfg = CompactionConfig {
        auto: AutoCompactConfig {
            enable: false,
            max_context_tokens: Some(10_000),
            context_ratio: Some(0.5),
        },
        ..CompactionConfig::default()
    };
    assert_eq!(cfg.soft_threshold(100_000), None);
}

#[test]
fn compaction_soft_threshold_caps() {
    assert_eq!(CompactionConfig::default().soft_threshold(200_000), None);
    let with_cap = CompactionConfig {
        auto: AutoCompactConfig {
            max_context_tokens: Some(150_000),
            ..AutoCompactConfig::default()
        },
        ..CompactionConfig::default()
    };
    assert_eq!(with_cap.soft_threshold(1_000_000), Some(150_000));
    assert_eq!(with_cap.soft_threshold(200_000), Some(150_000));
    let with_ratio = CompactionConfig {
        auto: AutoCompactConfig {
            context_ratio: Some(0.5),
            ..AutoCompactConfig::default()
        },
        ..CompactionConfig::default()
    };
    assert_eq!(with_ratio.soft_threshold(200_000), Some(100_000));
    let bad_ratio = CompactionConfig {
        auto: AutoCompactConfig {
            context_ratio: Some(1.5),
            ..AutoCompactConfig::default()
        },
        ..CompactionConfig::default()
    };
    assert_eq!(bad_ratio.soft_threshold(200_000), None);
    let both = CompactionConfig {
        auto: AutoCompactConfig {
            max_context_tokens: Some(150_000),
            context_ratio: Some(0.5),
            ..AutoCompactConfig::default()
        },
        ..CompactionConfig::default()
    };
    assert_eq!(both.soft_threshold(400_000), Some(150_000));
}

#[test]
fn compaction_config_serde_defaults() {
    let cfg: CompactionConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.reserved_context_tokens, 20_000);
    assert_eq!(cfg.min_messages_between_hard_compacts, 6);
    assert!(cfg.auto.enable);
    assert!(cfg.auto.max_context_tokens.is_none());
    assert!(cfg.auto.context_ratio.is_none());
}

#[test]
fn bash_config_serde_defaults() {
    let cfg: BashConfig = serde_json::from_str("{}").unwrap();
    assert!(cfg.strip_env);
    assert!(cfg.pass_env.is_empty());
    assert!(cfg.env_file.is_none());
}

#[test]
fn shell_policy_defaults_require_confirmation() {
    let cfg: ShellPolicyConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.mode, ShellPolicyMode::Confirm);
    assert_eq!(cfg.redirects.action, PolicyAction::Ask);
}

#[test]
fn shell_policy_presets_remain_deserializable() {
    for (raw, expected) in [
        ("read_only", ShellPolicyMode::ReadOnly),
        ("workspace_write", ShellPolicyMode::WorkspaceWrite),
        ("unrestricted", ShellPolicyMode::Unrestricted),
    ] {
        let json = format!(r#"{{"mode":"{raw}"}}"#);
        let cfg: ShellPolicyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg.mode, expected);
    }
}

#[test]
fn bash_config_serde_round_trip() {
    let json = r#"{"strip_env":false,"pass_env":["GITHUB_TOKEN","NPM_TOKEN"],"env_file":"~/.config/lofi/secrets.env"}"#;
    let cfg: BashConfig = serde_json::from_str(json).unwrap();
    assert!(!cfg.strip_env);
    assert_eq!(cfg.pass_env, ["GITHUB_TOKEN", "NPM_TOKEN"]);
    assert_eq!(
        cfg.env_file.as_deref(),
        Some(std::path::Path::new("~/.config/lofi/secrets.env"))
    );
}

#[test]
fn auto_compact_config_serde_defaults() {
    let cfg: AutoCompactConfig = serde_json::from_str("{}").unwrap();
    assert!(cfg.enable);
    assert!(cfg.max_context_tokens.is_none());
    assert!(cfg.context_ratio.is_none());
}

#[test]
fn auto_models_defaults() {
    let json = "{}";
    let am: AutoModelsConfig = serde_json::from_str(json).unwrap();
    assert_eq!(am.path, "data");
    assert!(am.auth);
    assert!(!am.enabled);
    assert!(am.models_url.is_none());
}
