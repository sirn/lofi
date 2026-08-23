use super::*;

fn insert_str_normalizes_line_endings() {
    let mut a = app();
    a.insert_str("a\r\nb\rc");
    assert_eq!(a.input, "a\nb\nc");
}

#[test]
fn model_picker_open_preselects_current() {
    let mut a = app(); // model_label = "openai/gpt-4o"
    a.model_choices = vec![
        lofi_types::ModelChoice {
            provider: "anthropic".into(),
            id: "claude".into(),
            name: "Claude".into(),
            thinking_levels: vec![ThinkingLevel::Medium],
            service_tiers: vec![],
            supports_image: false,
            context_window: Some(200_000),
        },
        lofi_types::ModelChoice {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            name: String::new(),
            thinking_levels: vec![],
            service_tiers: vec![],
            supports_image: false,
            context_window: Some(128_000),
        },
    ];
    a.open_model_picker();
    let picker = a.model_picker.as_ref().unwrap();
    assert_eq!(picker.choices.len(), 2);
    assert_eq!(picker.selected, 1);
    assert!(a.modal_open());
}

#[test]
fn model_picker_open_empty_notifies() {
    let mut a = app();
    a.model_choices = Vec::new();
    a.open_model_picker();
    assert!(a.model_picker.is_none());
    assert!(a.notify.is_some());
}

#[test]
fn model_picker_confirm_sets_pending_switch() {
    let mut a = app();
    a.model_choices = vec![
        lofi_types::ModelChoice {
            provider: "anthropic".into(),
            id: "claude".into(),
            name: "Claude".into(),
            thinking_levels: vec![],
            service_tiers: vec![],
            supports_image: false,
            context_window: None,
        },
        lofi_types::ModelChoice {
            provider: "openai".into(),
            id: "gpt-4o".into(),
            name: String::new(),
            thinking_levels: vec![],
            service_tiers: vec![],
            supports_image: false,
            context_window: None,
        },
    ];
    a.open_model_picker();
    a.model_picker.as_mut().unwrap().selected = 0;
    a.model_picker_confirm();
    assert_eq!(a.pending_model_switch.as_deref(), Some("anthropic/claude"));
    assert!(a.model_picker.is_none());
}

#[test]
fn apply_model_switch_updates_label_and_ctx_limit() {
    let mut a = app();
    a.ctx_limit = 0; // falls back to DEFAULT_CTX_LIMIT until a model reports one
    let model = lofi_types::Model {
        id: "claude".into(),
        name: "Claude".into(),
        provider: "anthropic".into(),
        api: lofi_types::Api::AnthropicMessages,
        reasoning: true,
        thinking: ThinkingLevel::XHigh,
        service_tier: ServiceTier::Auto,
        supports_image: true,
        context_window: Some(200_000),
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    };
    a.apply_model_switch(&model, ThinkingLevel::XHigh, ServiceTier::Auto);
    assert_eq!(a.model_label, "anthropic/claude");
    assert_eq!(a.thinking_label.as_deref(), Some(":xhigh"));
    assert_eq!(a.ctx_limit, 200_000);
    assert_eq!(a.thinking, ThinkingLevel::XHigh);
}

#[test]
fn apply_model_switch_with_no_context_window_uses_default() {
    let mut a = app();
    a.ctx_limit = 100_000;
    let model = lofi_types::Model {
        id: "local".into(),
        name: "Local".into(),
        provider: "ollama".into(),
        api: lofi_types::Api::OpenAiResponses,
        reasoning: false,
        thinking: ThinkingLevel::Off,
        service_tier: ServiceTier::Auto,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    };
    a.apply_model_switch(&model, ThinkingLevel::Off, ServiceTier::Auto);
    assert_eq!(a.ctx_limit, DEFAULT_CTX_LIMIT);
}

#[test]
fn thinking_picker_open_preselects_current() {
    let mut a = app(); // model_label = "openai/gpt-4o", thinking = Medium
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    a.open_thinking_picker();
    let picker = a.thinking_picker.as_ref().unwrap();
    assert_eq!(
        picker.levels,
        vec![
            ThinkingLevel::Off,
            ThinkingLevel::Medium,
            ThinkingLevel::High
        ]
    );
    assert_eq!(picker.selected, 1);
    assert!(a.modal_open());
}

#[test]
fn thinking_picker_open_no_levels_notifies() {
    let mut a = app();
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    a.open_thinking_picker();
    assert!(a.thinking_picker.is_none());
    assert!(a.notify.is_some());
}

#[test]
fn thinking_picker_confirm_sets_pending_switch() {
    let mut a = app(); // model_label = "openai/gpt-4o"
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![ThinkingLevel::Medium, ThinkingLevel::High],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    a.open_thinking_picker();
    a.thinking_picker.as_mut().unwrap().selected = 2;
    a.thinking_picker_confirm();
    assert_eq!(
        a.pending_model_switch.as_deref(),
        Some("openai/gpt-4o:high")
    );
    assert!(a.thinking_picker.is_none());
}

#[test]
fn policy_picker_defaults_to_ask_manual_without_auto_mode() {
    let mut a = app();
    a.policy_override = Some(lofi_core::PolicyOverride::default());
    a.auto_mode_configured = false;
    a.open_policy_picker();
    let picker = a.policy_picker.as_ref().unwrap();
    assert_eq!(
        picker.modes,
        vec![
            lofi_types::BashApprovalMode::AllowAll,
            lofi_types::BashApprovalMode::AskManual,
            lofi_types::BashApprovalMode::DenyAll
        ]
    );
    assert_eq!(picker.selected, 1);
    assert!(a.modal_open());
}

#[test]
fn policy_picker_includes_and_defaults_to_ask_auto_when_configured() {
    let mut a = app();
    a.policy_override = Some(lofi_core::PolicyOverride::default());
    a.auto_mode_configured = true;
    a.open_policy_picker();
    let picker = a.policy_picker.as_ref().unwrap();
    assert_eq!(
        picker.modes,
        vec![
            lofi_types::BashApprovalMode::AllowAll,
            lofi_types::BashApprovalMode::AskManual,
            lofi_types::BashApprovalMode::AskAuto,
            lofi_types::BashApprovalMode::DenyAll
        ]
    );
    assert_eq!(picker.selected, 2);
}

#[test]
fn policy_picker_confirm_writes_the_shared_override() {
    let mut a = app();
    let override_handle = lofi_core::PolicyOverride::default();
    a.policy_override = Some(override_handle.clone());
    a.auto_mode_configured = false;
    a.open_policy_picker();
    a.policy_picker.as_mut().unwrap().selected = 2; // deny all
    a.policy_picker_confirm();
    assert!(a.policy_picker.is_none());
    assert_eq!(
        override_handle.current(),
        Some(lofi_types::BashApprovalMode::DenyAll)
    );
}

#[test]
fn policy_badge_hidden_at_default_and_for_a_default_pick() {
    let mut a = app();
    a.policy_override = Some(lofi_core::PolicyOverride::default());
    a.auto_mode_configured = false;
    assert_eq!(a.policy_badge(), None);

    a.open_policy_picker();
    a.policy_picker_confirm();
    assert_eq!(a.policy_badge(), None);
}

#[test]
fn policy_badge_shows_a_non_default_pick() {
    let mut a = app();
    a.policy_override = Some(lofi_core::PolicyOverride::default());
    a.auto_mode_configured = false;
    a.open_policy_picker();
    a.policy_picker.as_mut().unwrap().selected = 2; // deny all
    a.policy_picker_confirm();
    assert_eq!(a.policy_badge().as_deref(), Some("policy: deny all"));
}

#[test]
fn policy_picker_requires_an_agent() {
    let mut a = app();
    assert!(a.slash_command("/policy"));
    assert!(a.policy_picker.is_none());
    let (msg, kind) = a.notify_badge().expect("no-agent /policy notified");
    assert_eq!(kind, NotifyKind::Error);
    assert!(msg.contains("/policy"));
}

#[test]
fn service_picker_open_lists_auto_plus_declared() {
    let mut a = app(); // model_label = "openai/gpt-4o", thinking = Medium
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![
            ServiceTier::Flex,
            ServiceTier::Priority,
            ServiceTier::Flex, // deduped
        ],
        supports_image: false,
        context_window: None,
    }];
    a.open_service_picker();
    let picker = a.service_picker.as_ref().unwrap();
    assert_eq!(
        picker.tiers,
        vec![ServiceTier::Auto, ServiceTier::Flex, ServiceTier::Priority]
    );
    assert_eq!(picker.selected, 0);
    assert!(a.modal_open());
}

#[test]
fn service_picker_open_no_tiers_notifies() {
    let mut a = app();
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    a.open_service_picker();
    assert!(a.service_picker.is_none());
    assert!(a.notify.is_some());
}

#[test]
fn service_picker_confirm_sets_pending_switch() {
    let mut a = app(); // model_label = "openai/gpt-4o", thinking = Medium
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![ServiceTier::Auto, ServiceTier::Flex, ServiceTier::Priority],
        supports_image: false,
        context_window: None,
    }];
    a.open_service_picker();
    a.service_picker.as_mut().unwrap().selected = 2;
    a.service_picker_confirm();
    assert_eq!(
        a.pending_model_switch.as_deref(),
        Some("openai/gpt-4o:medium@priority")
    );
    assert!(a.service_picker.is_none());

    // Selecting "auto" drops the @tier suffix entirely.
    let mut a2 = app();
    a2.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![ServiceTier::Auto, ServiceTier::Flex],
        supports_image: false,
        context_window: None,
    }];
    a2.open_service_picker();
    a2.service_picker.as_mut().unwrap().selected = 0;
    a2.service_picker_confirm();
    assert_eq!(
        a2.pending_model_switch.as_deref(),
        Some("openai/gpt-4o:medium")
    );
}

#[test]
fn footer_shows_service_tier_when_not_auto() {
    let a = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::High,
        ServiceTier::Flex,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let r: String = a
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r.contains("openai/gpt-4o:high@flex"));

    let b = App::new(
        "openai/gpt-4o".to_string(),
        ThinkingLevel::High,
        ServiceTier::Auto,
        0,
        lofi_types::CompactionConfig::default(),
        String::new(),
    );
    let r2: String = b
        .render_footer_right()
        .spans
        .iter()
        .map(|s| s.content.as_ref().to_string())
        .collect();
    assert!(r2.contains("openai/gpt-4o:high"));
    assert!(!r2.contains('@'));
}

#[test]
fn resume_model_switch_when_model_differs() {
    let mut a = app(); // model_label = "openai/gpt-4o", thinking = Medium
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "anthropic".into(),
        id: "claude".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    let events = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "anthropic/claude:high".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    }];
    assert_eq!(
        a.resume_model_switch(&events).as_deref(),
        Some("anthropic/claude:high")
    );
}

#[test]
fn resume_model_switch_none_when_same_model() {
    let mut a = app();
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "openai".into(),
        id: "gpt-4o".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    let events = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "openai/gpt-4o:medium".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    }];
    assert!(a.resume_model_switch(&events).is_none());
}

#[test]
fn resume_model_switch_none_when_no_choices_or_no_turn() {
    let mut a = app();
    a.model_choices = Vec::new();
    let events = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::TurnEnd {
            model: "anthropic/claude:high".into(),
            elapsed_ms: 0,
            cost: 0.0,
            usage: Usage::default(),
            stop_reason: None,
        },
    }];
    assert!(a.resume_model_switch(&events).is_none());
    a.model_choices = vec![lofi_types::ModelChoice {
        provider: "x".into(),
        id: "y".into(),
        name: String::new(),
        thinking_levels: vec![],
        service_tiers: vec![],
        supports_image: false,
        context_window: None,
    }];
    let no_turn = vec![SessionEvent {
        id: "1".into(),
        parent_id: None,
        kind: SessionEventKind::Message(Message {
            role: Role::User,
            blocks: vec![ContentBlock::Text { text: "hi".into() }],
            kind: PromptKind::default(),
        }),
    }];
    assert!(a.resume_model_switch(&no_turn).is_none());
}

#[test]
