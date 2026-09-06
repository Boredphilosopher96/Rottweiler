#![cfg(test)]
use super::Arc;
use super::Block;
use super::CacheBreakpointSupport;
use super::CacheHint;
use super::Cost;
use super::ModelDriver;
use super::PermissionDecision;
use super::PermissionGate;
use super::PromptCacheBreakpoint;
use super::PromptShapeJournal;
use super::PromptShapeRecord;
use super::Provider;
use super::ProviderModel;
use super::ProviderRequest;
use super::Role;
use super::ScriptProvider;
use super::SessionActor;
use super::SessionActorConfig;
use super::SessionId;
use super::SystemEventClock;
use super::ThinkingLevel;
use super::ToolChoice;
use super::ToolDefinition;
use super::Turn;
use super::TurnId;
use super::TurnMeta;
use super::builtin_command_registry;
use super::builtin_hook_dispatcher;
use super::cache_breakpoints_for_hint;
use super::hash_serialized;
use super::historical_tool_registry;
use super::prompt_request_fingerprint;
use super::tempdir;
use super::test_provider_admission;
use super::validate_historical_prompt_shape;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn historical_anthropic_prompt_shape_restores_cache_and_tool_schema_offline() {
    let root = tempdir().expect("prompt metadata root");
    let session_id = "historical-anthropic";
    std::fs::create_dir_all(root.path().join("sessions").join(session_id))
        .expect("session directory");
    let journal =
        Arc::new(PromptShapeJournal::open(root.path(), session_id).expect("prompt-shape journal"));
    journal.set_active_turn(TurnId("1".to_owned()));
    journal.set_prompt_source(&TurnId("1".into()), rw_types::SequenceId(5));
    let system = Turn {
        role: Role::System,
        blocks: vec![Block::Text {
            text: "stable historical policy".to_owned(),
        }],
        meta: TurnMeta::default(),
    };
    let user = Turn {
        role: Role::User,
        blocks: vec![Block::Text {
            text: "HISTORICAL_PROMPT_SECRET".to_owned(),
        }],
        meta: TurnMeta::default(),
    };
    let tool = ToolDefinition {
        name: "historic_read".to_owned(),
        description: "Historical read schema".to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"undeclared_path": {"type": "string"}},
            "required": ["undeclared_path"]
        }),
    };
    let request = ProviderRequest {
        model: "fast".to_owned(),
        turns: vec![system.clone(), user.clone()],
        tools: vec![tool.clone()],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 512,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: Some(rw_providers::CacheHint {
            stable_prefix_turns: 1,
            tools_in_prefix: true,
        }),
    };
    journal
        .record_request("fast", &request, CacheBreakpointSupport::Explicit)
        .expect("record prompt shape");
    let metadata = std::fs::read(
        root.path()
            .join("sessions")
            .join(session_id)
            .join("prompt-shapes.sqlite3"),
    )
    .expect("prompt-shape metadata");
    assert!(
        !metadata
            .windows(b"HISTORICAL_PROMPT_SECRET".len())
            .any(|window| window == b"HISTORICAL_PROMPT_SECRET")
    );
    let (profile, record) = journal
        .shape_for_turn(1)
        .expect("historical shape lookup")
        .expect("historical shape");
    assert_eq!(profile.cache_support, CacheBreakpointSupport::Explicit);
    assert_eq!(profile.cache_hint, request.cache_hint);
    assert_eq!(
        profile.cache_breakpoints,
        vec![PromptCacheBreakpoint {
            after_item_id: Some("system:0".to_owned()),
        }]
    );
    assert_eq!(profile.tools, vec![tool]);
    assert_eq!(
        journal
            .latest_shape()
            .expect("latest prompt shape")
            .expect("recorded latest prompt shape"),
        (profile.clone(), record.clone())
    );

    let provider: Arc<dyn Provider> = Arc::new(
        ScriptProvider::new("anthropic-history".to_owned(), Vec::new(), 0)
            .with_cache_support(profile.cache_support),
    );
    let model: Arc<dyn ModelDriver> = Arc::new(
        ProviderModel::new(
            provider,
            rw_core::CompactionConfig::default(),
            rw_core::BudgetConfig::default(),
        )
        .expect("fixture concrete model"),
    );
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let source = crate::session_runtime::test_history::open(
        root.path(),
        &SessionId(session_id.into()),
        None,
        vec![user],
    )
    .await
    .expect("prompt source");
    let actor = SessionActor::spawn(SessionActorConfig {
        ui: std::sync::Arc::new(rw_core::ui::EmptyUiRegistry),
        ui_tool_source: std::sync::Arc::new(rw_core::ui::UnavailableUiToolSource),
        budget_session_id: SessionId(session_id.to_owned()),
        session_id: SessionId(session_id.to_owned()),
        workspace_root: workspace,
        additional_workspace_roots: Vec::new(),
        workspace_generation: 0,
        initial_session_context: vec![system],
        startup_notifications: Vec::new(),
        model_alias: profile.model_alias.clone(),
        model,
        tools: historical_tool_registry(&profile).expect("historical tools"),
        permissions: Arc::new(PermissionGate::new(PermissionDecision::Allow)),
        hooks: Arc::new(builtin_hook_dispatcher().expect("hooks")),
        commands: Arc::new(builtin_command_registry().expect("commands")),
        modes: Arc::new(rw_ext::ModeRegistry::builtins().expect("built-in modes")),
        history: source.history,
        event_sink: source.sink,
        event_clock: Arc::new(SystemEventClock),
        provider_admission: test_provider_admission(),
        secret_redactor: Arc::new(rw_core::NoopSecretRedactor),
        checkpoints: Arc::new(rw_core::NoopMutationCheckpointCoordinator),
        folder_trust: Arc::new(rw_core::NoopFolderTrustController),
        workspace_roots: Arc::new(rw_core::NoopWorkspaceRootController),
        extension_development: Arc::new(rw_core::NoopSessionExtensionController),
        resources: Arc::new(rw_core::NoopSessionResources),
        recovered: source.recovered,
        max_turns: 1,
        identical_tool_failure_limit: 1,
        max_output_tokens: 512,
        thinking: ThinkingLevel::Off,
        event_capacity: 32,
    })
    .expect("historical prompt actor");
    let dump = actor.dump_prompt(None).await.expect("historical dump");
    assert_eq!(dump.tools[0].input_schema, profile.tools[0].input_schema);
    assert_eq!(dump.cache_breakpoints.len(), 1);
    let tools = dump
        .tools
        .iter()
        .map(|tool| ToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        })
        .collect::<Vec<_>>();
    validate_historical_prompt_shape(&dump, &tools, &profile, &record)
        .expect("recorded prompt shape must validate");
    assert_eq!(
        prompt_request_fingerprint(
            &dump.model_alias.0,
            &dump.turns,
            &tools,
            profile.cache_hint,
            profile.cache_support,
            &profile.cache_breakpoints,
        )
        .expect("dump fingerprint"),
        record.request_fingerprint
    );
    assert_ne!(
        prompt_request_fingerprint(
            &dump.model_alias.0,
            &dump.turns,
            &tools,
            profile.cache_hint,
            CacheBreakpointSupport::Automatic,
            &profile.cache_breakpoints,
        )
        .expect("provider-managed cache fingerprint"),
        record.request_fingerprint,
        "explicit and provider-managed cache modes must not share a fingerprint"
    );

    let mut mismatched_profile = profile.clone();
    mismatched_profile.cache_hint = Some(CacheHint {
        stable_prefix_turns: 2,
        tools_in_prefix: true,
    });
    mismatched_profile.cache_breakpoints = cache_breakpoints_for_hint(
        mismatched_profile.cache_hint,
        mismatched_profile.cache_support,
    );
    let mismatched_record = PromptShapeRecord {
        source: record.source,
        profile_id: hash_serialized(&mismatched_profile).expect("mismatched profile id"),
        request_fingerprint: prompt_request_fingerprint(
            &dump.model_alias.0,
            &dump.turns,
            &tools,
            mismatched_profile.cache_hint,
            mismatched_profile.cache_support,
            &mismatched_profile.cache_breakpoints,
        )
        .expect("mismatched fingerprint"),
    };
    let error =
        validate_historical_prompt_shape(&dump, &tools, &mismatched_profile, &mismatched_record)
            .expect_err("a different stable boundary must fail closed");
    assert!(error.to_string().contains("recorded cache behavior"));
}

#[test]
fn prompt_shape_store_pins_reused_turns_to_their_canonical_sources() {
    let root = tempdir().expect("prompt metadata root");
    let session_id = "tampered-prompt-shape";
    let session_directory = root.path().join("sessions").join(session_id);
    std::fs::create_dir_all(&session_directory).expect("session directory");
    let journal = PromptShapeJournal::open(root.path(), session_id).expect("shape journal");
    journal.set_active_turn(TurnId("1".to_owned()));
    journal.set_prompt_source(&TurnId("1".into()), rw_types::SequenceId(5));
    let request = ProviderRequest {
        model: "fast".to_owned(),
        turns: vec![Turn {
            role: Role::System,
            blocks: vec![Block::Text {
                text: "stable policy".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: Some(CacheHint {
            stable_prefix_turns: 1,
            tools_in_prefix: false,
        }),
    };
    journal
        .record_request("fast", &request, CacheBreakpointSupport::Explicit)
        .expect("record prompt shape");

    let original = journal
        .shape_at_source(1, 5)
        .expect("source lookup")
        .expect("recorded shape");
    assert_eq!(original.1.source, 5);
    journal.set_active_turn(TurnId("1".into()));
    journal.set_prompt_source(&TurnId("1".into()), rw_types::SequenceId(25));
    let mut request = request;
    request.turns[0].blocks = vec![Block::Text {
        text: "replacement request".into(),
    }];
    journal
        .record_request("fast", &request, CacheBreakpointSupport::Explicit)
        .expect("reused turn source");
    let replacement = journal
        .shape_at_source(1, 25)
        .expect("new lookup")
        .expect("new shape");
    assert_ne!(
        original.1.request_fingerprint,
        replacement.1.request_fingerprint
    );
    assert_eq!(
        journal
            .shape_at_source(1, 5)
            .expect("old source")
            .expect("old record"),
        original
    );
    assert_eq!(
        journal
            .shape_for_turn(1)
            .expect("latest turn")
            .expect("record"),
        replacement
    );
    assert!(journal.shape_at_source(2, 25).is_err());
    drop(journal);
    let reopened = PromptShapeJournal::open(root.path(), session_id).expect("reopen");
    assert_eq!(
        reopened
            .shape_at_source(1, 5)
            .expect("reopened source")
            .expect("old record"),
        original
    );
    assert_eq!(
        reopened
            .latest_shape()
            .expect("latest source")
            .expect("new record"),
        replacement
    );
}

#[test]
fn offline_provider_model_replays_subscription_and_ai_credit_accounting() {
    let capabilities = serde_json::json!({
        "tool_calling": true,
        "vision": false,
        "thinking": false,
        "cache_breakpoints": "none",
        "max_context_tokens": 128_000,
        "max_output_tokens": 16384,
        "wire_mode": "normalized_replay"
    });
    let metadata = |accounting: serde_json::Value, pricing: serde_json::Value| {
        serde_json::from_value::<rw_core::ProviderModelMetadata>(serde_json::json!({
            "capabilities": capabilities.clone(),
            "pricing": pricing,
            "accounting": accounting
        }))
        .expect("provider metadata fixture")
    };
    let usage = rw_core::ModelTokenUsage {
        input_tokens: 2,
        output_tokens: 0,
        cache_read_tokens: 1,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
    };
    let subscription: Arc<dyn Provider> = Arc::new(
        ScriptProvider::new("subscription-replay".to_owned(), Vec::new(), 0).with_model_metadata(
            metadata(
                serde_json::json!({"kind": "subscription_quota"}),
                serde_json::Value::Null,
            ),
        ),
    );
    let subscription_model = ProviderModel::new(
        subscription,
        rw_core::CompactionConfig::default(),
        rw_core::BudgetConfig::default(),
    )
    .expect("fixture concrete model");
    assert!(matches!(
        subscription_model.cost("fast", usage),
        Cost::SubscriptionQuota { used: Some(used), unit: Some(unit) }
            if used == "3" && unit == "tokens"
    ));

    let credits: Arc<dyn Provider> = Arc::new(
        ScriptProvider::new("credit-replay".to_owned(), Vec::new(), 0).with_model_metadata(
            metadata(
                serde_json::json!({
                    "kind": "ai_credits",
                    "micros_usd_per_credit": 2
                }),
                serde_json::json!({
                    "display_name": "credit fixture",
                    "input_per_million_micros_usd": 1_000_000,
                    "output_per_million_micros_usd": 1_000_000,
                    "cache_read_per_million_micros_usd": 0
                }),
            ),
        ),
    );
    let credit_model = ProviderModel::new(
        credits,
        rw_core::CompactionConfig::default(),
        rw_core::BudgetConfig::default(),
    )
    .expect("fixture concrete model");
    assert!(matches!(
        credit_model.cost("fast", usage),
        Cost::AiCredits {
            credits_micros: 1_000_000,
            nominal_amount_micros: Some(nominal),
            currency: Some(currency),
        } if nominal == "2" && currency == "USD"
    ));
}
