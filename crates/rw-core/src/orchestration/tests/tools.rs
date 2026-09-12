use super::super::sessions::bind_child_tools;
use super::super::tools::SpawnAgentAction;
use super::*;

#[test]
fn non_execute_children_filter_mutations_and_reject_interaction() {
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(MutatingTool)).expect("register");
    let tools = Arc::new(tools);
    let discuss = restricted_registry(&tools, &["write".to_owned()], SessionMode::Discuss)
        .expect("discuss subset");
    assert!(discuss.is_empty());
    let error = restricted_registry(&tools, &["ask_user".to_owned()], SessionMode::Execute)
        .err()
        .expect("interactive child tool must fail");
    assert!(error.to_string().contains("cannot include interactive"));
    let missing_root_bound = bind_child_tools(&ToolRegistry::new(), &tools)
        .err()
        .expect("root-bound fallback must fail");
    assert!(missing_root_bound.to_string().contains("was not rebuilt"));
}

#[test]
fn child_mcp_virtual_tools_mint_only_exact_gateway_authority() {
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(GatewayTool("tool_search")))
        .expect("search gateway");
    tools
        .register(Arc::new(GatewayTool("mcp_call")))
        .expect("call gateway");
    let tools = Arc::new(tools);

    let restricted = restricted_registry(
        &tools,
        &["mcp:github/get_issue".to_owned()],
        SessionMode::Execute,
    )
    .expect("exact MCP policy");
    assert!(restricted.descriptor("tool_search").is_some());
    assert!(restricted.descriptor("mcp_call").is_some());
    assert!(restricted.mcp_tool_policy().allows("github", "get_issue"));
    assert!(
        !restricted
            .mcp_tool_policy()
            .allows("github", "delete_issue")
    );

    for invalid in ["mcp:github/*", "tool_search", "mcp_call"] {
        assert!(
            restricted_registry(&tools, &[invalid.to_owned()], SessionMode::Execute,).is_err(),
            "{invalid} must not widen child MCP authority"
        );
    }
    assert!(
        restricted_registry(
            &tools,
            &["mcp:github/get_issue".to_owned()],
            SessionMode::Discuss,
        )
        .is_err()
    );
}

#[tokio::test]
async fn spawn_control_never_prompts_and_inherits_selected_live_model_for_builtin_children() {
    let workspace = tempfile::tempdir().expect("workspace");
    let factory = Arc::new(FakeFactory::default());
    let launches = Arc::clone(&factory.launches);
    let orchestrator = orchestrator(SubagentLimits::default(), factory);
    let mut agents = rw_ext::compose_agent_registry(&rw_ext::ExtensionCatalog::default())
        .expect("built-in agents");
    agents
        .resolve_tool_names(std::iter::empty())
        .expect("built-in tools filter to the available registry");
    let tool = SpawnAgentTool::new(
        orchestrator,
        Arc::new(agents),
        Arc::new(|| Ok(Arc::new(SelectedModel) as Arc<dyn crate::ModelDriver>)),
    );
    let sink = Arc::new(RecordingSubagentSink::default());
    let context = ToolContext::new(workspace.path())
        .expect("tool context")
        .with_session_id(SessionId("parent".to_owned()))
        .with_model_alias("openai_codex/gpt-5.6-sol")
        .with_subagent_event_sink(sink.clone());
    let gate = crate::PermissionGate::from_config(crate::PermissionConfig {
        default: PermissionDecision::Ask,
        rules: Vec::new(),
    })
    .with_workspace_roots([workspace.path()]);
    let approver = RejectingApprover(AtomicUsize::new(0));

    for (agent, isolation) in [("explore", "shared"), ("general", "shared")] {
        let input = json!({
            "action": "spawn",
            "task": "delay:1",
            "agent": agent,
            "isolation": isolation,
        });
        let capabilities = tool
            .invocation_capabilities(&input)
            .expect("spawn capabilities")
            .capabilities()
            .to_vec();
        assert!(
            capabilities.is_empty(),
            "the parent control call must not claim the child's tool authority"
        );
        let permission = crate::PermissionRequest {
            invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
            id: format!("spawn-{agent}"),
            tool_name: "spawn_agent".to_owned(),
            arguments: input.clone(),
            capabilities,
            approval_diff: None,
        };
        assert_eq!(
            gate.authorize(permission, &approver).await,
            crate::PermissionOutcome::Allowed,
            "subagent control must bypass the parent approval modal"
        );
        tool.execute(&context, input)
            .await
            .expect("built-in child uses parent model");
    }

    assert_eq!(approver.0.load(Ordering::SeqCst), 0);
    let launches = launches
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(launches.len(), 2);
    assert_eq!(launches[0].agent, "explore");
    assert_eq!(launches[1].agent, "general");
    assert!(
        launches
            .iter()
            .all(|launch| launch.model == "openai_codex/gpt-5.6-sol")
    );
    assert_eq!(
        sink.lifecycles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        4,
        "each child emits spawned and finished lifecycle events"
    );
}

#[test]
fn action_shapes_are_rejected_at_the_input_boundary() {
    for value in [
        json!({"action":"spawn","task":"x","subagent_id":"child"}),
        json!({"action":"follow_up","subagent_id":"child","follow_up":"x","agent":"general"}),
        json!({"action":"cancel","subagent_id":"child","follow_up":"x"}),
        json!({"action":"close","subagent_id":"child","isolation":"worktree"}),
    ] {
        assert!(serde_json::from_value::<SpawnAgentAction>(value).is_err());
    }
    let follow_up: SpawnAgentAction = serde_json::from_value(json!({
        "action":"follow_up", "subagent_id":"child", "follow_up":"continue"
    }))
    .expect("explicit follow-up action");
    assert!(matches!(follow_up, SpawnAgentAction::FollowUp { .. }));
}

#[tokio::test]
async fn worst_case_model_handoff_keeps_artifact_reference_under_wire_limit() {
    let mut artifact = test_artifact();
    artifact.unified_diff = "d".repeat(MAX_SUBAGENT_DIFF_BYTES);
    rehash_test_artifact(&mut artifact);
    let artifact_id = artifact.id.clone();
    let result = SubagentResult {
        subagent_id: SubagentId("child".to_owned()),
        session_id: SessionId("child-session".to_owned()),
        status: SubagentStatus::Completed,
        final_text: "\0".repeat(MAX_SUBAGENT_FINAL_TEXT_BYTES),
        touched_files: (0..MAX_SUBAGENT_TOUCHED_FILES)
            .map(|index| format!("{}-{index}", "\"".repeat(4090)))
            .collect(),
        diff_artifact: Some(artifact),
        usage: zero_usage(),
        cost: Cost::Unavailable {
            reason: "fixture".to_owned(),
        },
        turns: 1,
        duration_millis: 1,
    };
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(FixedResultTool {
            result: model_facing_subagent_tool_result(&result),
        }))
        .expect("register");
    let tool = registry.resolve("fixed_result").expect("tool");
    let context = ToolContext::new(std::env::current_dir().expect("cwd")).expect("context");
    let output = tool.execute(&context, Value::Null).await.expect("execute");
    assert!(!output.truncated);
    assert_eq!(
        output.data["diff_artifact"]["artifact_id"].as_str(),
        Some(artifact_id.as_str())
    );
}

#[test]
fn result_schema_round_trips_cost_and_usage() {
    let result = SubagentResult {
        subagent_id: SubagentId("agent".to_owned()),
        session_id: SessionId("child".to_owned()),
        status: SubagentStatus::Completed,
        final_text: "done".to_owned(),
        touched_files: vec!["src/lib.rs".to_owned()],
        diff_artifact: None,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 1,
            reasoning_tokens: 2,
        },
        cost: Cost::Monetary {
            amount_micros: 42,
            currency: "USD".to_owned(),
        },
        turns: 2,
        duration_millis: 7,
    };
    let encoded = serde_json::to_vec(&result).expect("encode");
    let decoded: SubagentResult = serde_json::from_slice(&encoded).expect("decode");
    assert_eq!(decoded, result);
}

#[test]
fn missing_subagent_action_is_rejected() {
    for input in [
        json!({"task":"inspect","agent":"explore"}),
        json!({"subagent_id":"child","follow_up":"continue"}),
    ] {
        assert!(serde_json::from_value::<SpawnAgentAction>(input).is_err());
    }
}

#[test]
fn every_subagent_schema_variant_requires_its_action_and_own_fields() {
    let schema =
        serde_json::to_value(schemars::schema_for!(SpawnAgentAction)).expect("action schema");
    let variants = schema["oneOf"].as_array().expect("action variants");
    assert_eq!(variants.len(), 4);
    for variant in variants {
        assert!(
            variant["required"]
                .as_array()
                .expect("required fields")
                .contains(&json!("action"))
        );
        assert_eq!(variant["additionalProperties"], json!(false));
    }
    let spawn =
        serde_json::from_value::<SpawnAgentAction>(json!({"action":"spawn","task":"inspect"}))
            .expect("spawn defaults");
    assert!(
        matches!(spawn, SpawnAgentAction::Spawn { agent, isolation: SubagentIsolation::Worktree, .. } if agent == "general")
    );
    for action in ["cancel", "close"] {
        assert!(
            serde_json::from_value::<SpawnAgentAction>(
                json!({"action":action,"subagent_id":"child"})
            )
            .is_ok()
        );
        assert!(serde_json::from_value::<SpawnAgentAction>(json!({"action":action})).is_err());
    }
}
