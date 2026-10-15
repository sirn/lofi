#![allow(clippy::unwrap_used)]
#![allow(clippy::wildcard_imports)]

use super::*;

fn mc() -> ModelConfig {
    ModelConfig {
        name: None,
        api_type: None,
        reasoning: None,
        supports_image: None,
        context_window: None,
        max_tokens: None,
        thinking_levels: vec![
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
        ],
        thinking_level: None,
        service_tiers: Vec::new(),
        service_tier: None,
        auto_continue: lofi_types::AutoContinueConfig::default(),
        base_url: None,
        input_price: None,
        output_price: None,
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    }
}

fn mc_levels(levels: &[ThinkingLevel]) -> ModelConfig {
    let mut m = mc();
    m.thinking_levels = levels.to_vec();
    m
}

fn mc_default(level: ThinkingLevel) -> ModelConfig {
    let mut m = mc();
    m.thinking_level = Some(level);
    m
}

fn mc_tiers(tiers: &[ServiceTier]) -> ModelConfig {
    let mut m = mc();
    m.service_tiers = tiers.to_vec();
    m
}

fn mc_tier(tier: ServiceTier) -> ModelConfig {
    let mut m = mc();
    m.service_tier = Some(tier.clone());
    m.service_tiers = vec![tier];
    m
}

fn models(entries: &[(&str, ModelConfig)]) -> IndexMap<String, ModelConfig> {
    let mut map = IndexMap::new();
    for (k, v) in entries {
        map.insert((*k).to_string(), v.clone());
    }
    map
}

fn provider(
    api: Api,
    key: Option<&str>,
    models: IndexMap<String, ModelConfig>,
    thinking_level: Option<ThinkingLevel>,
) -> ProviderConfig {
    let mut mappings = IndexMap::new();
    mappings.insert(
        api.as_str().to_string(),
        ApiTypeMapping {
            path: None,
            pricing_field_mappings: None,
        },
    );
    ProviderConfig {
        api_type: Some(api),
        api_types: mappings,
        base_url: Some("https://api.example.com".to_string()),
        pricing_convention: PricingConvention::PerToken,
        pricing_field_mappings: PricingFieldMappings::default(),
        env_name: None,
        api_key: key.map(str::to_string),
        headers: None,
        models,
        auto_models: None,
        no_auth: false,
        response_start_timeout_ms: 90_000,
        thinking_level,
        thinking_levels: Vec::new(),
        service_tier: None,
        service_tiers: Vec::new(),
        auto_continue: lofi_types::AutoContinueConfig::default(),
    }
}

fn build(providers: IndexMap<String, ProviderConfig>) -> (Config, ModelRegistry) {
    let cfg = Config {
        providers,
        ..Config::default()
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    (cfg, reg)
}

fn one_provider(p: ProviderConfig) -> IndexMap<String, ProviderConfig> {
    let mut m = IndexMap::new();
    m.insert("openai".to_string(), p);
    m
}

#[test]
fn select_model_explicit_qualified() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(m.provider, "openai");
    assert_eq!(level, ThinkingLevel::Medium);
}

#[test]
fn select_model_explicit_level_suffix() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:xhigh")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(level, ThinkingLevel::XHigh);
}

#[test]
fn select_model_off_level_always_allowed() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_levels(&[]))]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:off")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(level, ThinkingLevel::Off);
}

#[test]
fn select_model_rejects_bare_model() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("gpt-4o")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_rejects_unknown_level() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o:bogus")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_explicit_tier_suffix() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[(
            "gpt-4o",
            mc_tiers(&[ServiceTier::Flex, ServiceTier::Priority]),
        )]),
        None,
    )));
    let (m, level) = select_model(&reg, &cfg, Some("openai/gpt-4o:high@flex")).unwrap();
    assert_eq!(m.id, "gpt-4o");
    assert_eq!(level, ThinkingLevel::High);
    assert_eq!(m.service_tier, ServiceTier::Flex);
}

#[test]
fn select_model_explicit_tier_without_level() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_tiers(&[ServiceTier::Priority]))]),
        None,
    )));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o@priority")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Priority);
}

#[test]
fn select_model_tier_defaults_auto_from_model_list() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_tiers(&[ServiceTier::Flex]))]),
        None,
    )));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Auto);
}

#[test]
fn select_model_tier_falls_back_to_model_default() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_tier(ServiceTier::Priority))]),
        None,
    )));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Priority);
}

#[test]
fn select_model_custom_tier_uses_declared_list() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[(
            "gpt-4o",
            mc_tiers(&[ServiceTier::Flex, ServiceTier::Custom("vip".into())]),
        )]),
        None,
    )));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o@vip")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Custom("vip".into()));

    // An undeclared custom value is rejected, so a typo never quietly sends
    // the provider default.
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o@bogus")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_rejects_tier_not_declared() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_tiers(&[ServiceTier::Flex]))]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o@priority")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_rejects_explicit_tier_when_none_declared() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o:high@flex")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_auto_tier_always_allowed_even_without_declared_list() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o:high@auto")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Auto);
}

#[test]
fn select_model_provider_tier_applies_to_model_without_own_list() {
    let mut p = provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    );
    p.service_tiers = vec![ServiceTier::Flex, ServiceTier::Priority];
    p.service_tier = Some(ServiceTier::Priority);
    let (cfg, reg) = build(one_provider(p));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Priority);
}

#[test]
fn select_model_explicit_tier_validated_against_provider_list() {
    let mut p = provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc())]),
        None,
    );
    p.service_tiers = vec![ServiceTier::Flex];
    let (cfg, reg) = build(one_provider(p));
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o@flex")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Flex);
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o@priority")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_model_list_overrides_provider_tier() {
    let mut p = provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_tiers(&[ServiceTier::Flex]))]),
        None,
    );
    p.service_tiers = vec![ServiceTier::Priority];
    p.service_tier = Some(ServiceTier::Priority);
    let (cfg, reg) = build(one_provider(p));
    // The model declares its own list, so the provider default does not
    // apply and explicit tiers are validated against the model list.
    let (m, _) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(m.service_tier, ServiceTier::Auto);
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o@priority")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_level_override_wins_over_query_suffix() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[(
            "gpt-4o",
            mc_levels(&[ThinkingLevel::Low, ThinkingLevel::High]),
        )]),
        None,
    )));
    let (_m, level) =
        select_model_with_level(&reg, &cfg, Some("openai/gpt-4o:low"), ThinkingLevel::High)
            .unwrap();
    assert_eq!(level, ThinkingLevel::High);
}

#[test]
fn select_model_level_override_must_be_declared() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[("gpt-4o", mc_levels(&[ThinkingLevel::Low]))]),
        None,
    )));
    let err = select_model_with_level(&reg, &cfg, Some("openai/gpt-4o"), ThinkingLevel::High)
        .unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err:?}");
}

#[test]
fn select_model_rejects_level_not_declared() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk-test"),
        models(&[(
            "gpt-4o",
            mc_levels(&[ThinkingLevel::Low, ThinkingLevel::Medium]),
        )]),
        None,
    )));
    let err = select_model(&reg, &cfg, Some("openai/gpt-4o:high")).unwrap_err();
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn select_model_first_available_when_no_query() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let (cfg, reg) = build(providers);
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "openai");
    assert_eq!(m.id, "gpt-4o");
}

#[test]
fn select_model_uses_default_model_when_no_query() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        default_model: Some("anthropic/claude".to_string()),
        providers,
        ..Config::default()
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.id, "claude");
}

#[test]
fn select_model_uses_default_provider_when_no_query() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        default_provider: Some("anthropic".to_string()),
        providers,
        ..Config::default()
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.id, "claude");
}

#[test]
fn select_model_default_model_overrides_default_provider() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        default_provider: Some("anthropic".to_string()),
        default_model: Some("openai/gpt-4o".to_string()),
        providers,
        ..Config::default()
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, None).unwrap();
    assert_eq!(m.provider, "openai");
    assert_eq!(m.id, "gpt-4o");
}

#[test]
fn select_model_explicit_query_overrides_defaults() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "anthropic".to_string(),
        provider(
            Api::AnthropicMessages,
            Some("sk"),
            models(&[("claude", mc())]),
            None,
        ),
    );
    let cfg = Config {
        default_provider: Some("openai".to_string()),
        default_model: Some("openai/gpt-4o".to_string()),
        providers,
        ..Config::default()
    };
    let reg = ModelRegistry::load(&cfg).unwrap();
    let (m, _) = select_model(&reg, &cfg, Some("anthropic/claude")).unwrap();
    assert_eq!(m.provider, "anthropic");
    assert_eq!(m.id, "claude");
}

#[test]
fn select_model_no_models_error() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        None,
        models(&[("gpt-4o", mc())]),
        None,
    )));
    let err = select_model(&reg, &cfg, None).unwrap_err();
    assert!(matches!(err, Error::NoModels(_)));
}

#[test]
fn select_model_rejects_keyless_provider_model() {
    let mut providers = IndexMap::new();
    providers.insert(
        "openai".to_string(),
        provider(
            Api::OpenAiCompletions,
            Some("sk"),
            models(&[("gpt-4o", mc())]),
            None,
        ),
    );
    providers.insert(
        "local".to_string(),
        provider(
            Api::OpenAiCompletions,
            None,
            models(&[("local-model", mc())]),
            None,
        ),
    );
    let (cfg, reg) = build(providers);
    let err = select_model(&reg, &cfg, Some("local/local-model")).unwrap_err();
    // A disabled (keyless) provider now reports as "no models" rather than
    // a config error, so the TUI can launch in no-model mode.
    assert!(matches!(err, Error::NoModels(_)));
}

#[test]
fn select_model_thinking_precedence_model_default() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk"),
        models(&[("gpt-4o", mc_default(ThinkingLevel::High))]),
        None,
    )));
    let (_, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(level, ThinkingLevel::High);
}

#[test]
fn select_model_thinking_precedence_provider_default() {
    let (cfg, reg) = build(one_provider(provider(
        Api::OpenAiCompletions,
        Some("sk"),
        models(&[("gpt-4o", mc())]),
        Some(ThinkingLevel::Low),
    )));
    let (_, level) = select_model(&reg, &cfg, Some("openai/gpt-4o")).unwrap();
    assert_eq!(level, ThinkingLevel::Low);
}

#[test]
fn add_usage_bills_cache_tokens_at_their_own_rate() {
    let model = Model {
        id: "m".to_string(),
        name: "m".to_string(),
        provider: "p".to_string(),
        api: Api::OpenAiCompletions,
        reasoning: false,
        thinking: ThinkingLevel::Off,
        service_tier: ServiceTier::Auto,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: Some(1.0),
        output_price: Some(2.0),
        cache_read_price: Some(0.1),
        cache_write_price: Some(0.5),
        per_request_price: None,
    };
    let mut stats = TurnStats::new();
    stats.add_usage(
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 1_000_000,
        },
        &model,
    );
    assert!((stats.cost - 3.70).abs() < 1e-9, "cost was {}", stats.cost);
}

#[test]
fn add_usage_falls_back_to_input_rate_when_cache_prices_unset() {
    let model = Model {
        id: "m".to_string(),
        name: "m".to_string(),
        provider: "p".to_string(),
        api: Api::OpenAiCompletions,
        reasoning: false,
        thinking: ThinkingLevel::Off,
        service_tier: ServiceTier::Auto,
        supports_image: false,
        context_window: None,
        max_tokens: None,
        base_url: None,
        input_price: Some(1.0),
        output_price: Some(2.0),
        cache_read_price: None,
        cache_write_price: None,
        per_request_price: None,
    };
    let mut stats = TurnStats::new();
    stats.add_usage(
        Usage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            cache_read_tokens: 2_000_000,
            cache_write_tokens: 1_000_000,
        },
        &model,
    );
    assert!((stats.cost - 6.0).abs() < 1e-9, "cost was {}", stats.cost);
}

#[test]
fn clipped_max_tokens_respects_headroom() {
    let root = tempdir().unwrap();
    let mut agent = agent_with(Vec::new(), root.path());
    agent.model.context_window = Some(100_000);
    agent.model.max_tokens = Some(32_000);
    // No prior usage: cap wins under headroom.
    assert_eq!(agent.clipped_max_tokens(None), Some(32_000));
    // Headroom above the cap: cap unchanged.
    assert_eq!(agent.clipped_max_tokens(Some(50_000)), Some(32_000));
    // Headroom below the cap: clip to headroom.
    assert_eq!(agent.clipped_max_tokens(Some(90_000)), Some(10_000));
    // No headroom: omit the field entirely.
    assert_eq!(agent.clipped_max_tokens(Some(100_000)), None);
}

#[test]
fn clipped_max_tokens_prefers_run_override() {
    let root = tempdir().unwrap();
    let mut agent = agent_with(Vec::new(), root.path());
    agent.model.context_window = Some(100_000);
    agent.model.max_tokens = Some(32_000);
    agent.max_output_tokens = Some(8_000);
    assert_eq!(agent.clipped_max_tokens(None), Some(8_000));
    assert_eq!(agent.clipped_max_tokens(Some(95_000)), Some(5_000));
}

#[test]
fn clipped_max_tokens_none_when_window_unknown() {
    let root = tempdir().unwrap();
    let mut agent = agent_with(Vec::new(), root.path());
    agent.model.max_tokens = Some(32_000);
    // No context_window: cannot compute headroom.
    assert_eq!(agent.clipped_max_tokens(Some(10_000)), None);
    agent.model.context_window = Some(100_000);
    agent.model.max_tokens = None;
    agent.max_output_tokens = None;
    // No cap at all.
    assert_eq!(agent.clipped_max_tokens(None), None);
}

#[tokio::test]
async fn non_vision_model_warns_once_per_turn_with_image_in_history() {
    // The omit notice must fire exactly once for the whole turn, not once
    // per tool round. Two rounds: a tool-use round then a text round.
    let dir = tempdir().unwrap();
    let tool_input = serde_json::json!({ "code": "return 1" }).to_string();
    let round1 = vec![
        StreamingEvent::ToolUseStart {
            id: "t1".to_string(),
            name: "exec".to_string(),
        },
        StreamingEvent::ToolUseInputDelta {
            id: "t1".to_string(),
            delta: tool_input,
        },
        StreamingEvent::ToolUseEnd {
            id: "t1".to_string(),
        },
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let round2 = vec![
        StreamingEvent::TextDelta("done".to_string()),
        StreamingEvent::Done {
            usage: Usage::default(),
            stop_reason: None,
        },
    ];
    let agent = agent_with(vec![round1, round2], dir.path());
    let mut messages = vec![Message {
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
        ],
        kind: PromptKind::default(),
    }];

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(64);
    agent
        .run_turn(
            &mut messages,
            TurnRequest {
                input: TurnInput::Continue,
                notices: Vec::new(),
            },
            tx,
            None,
            RunControls {
                cancel: None,
                preempt: None,
            },
        )
        .await
        .unwrap();

    let mut notices = 0usize;
    while let Ok(ev) = rx.try_recv() {
        if matches!(ev, AgentEvent::Notice(_)) {
            notices += 1;
        }
    }
    assert_eq!(
        notices, 1,
        "expected one omit notice per turn, got {notices}"
    );
    // The send-time strip leaves the durable history untouched.
    assert!(messages[0]
        .blocks
        .iter()
        .any(|b| matches!(b, ContentBlock::Image { .. })));
}
