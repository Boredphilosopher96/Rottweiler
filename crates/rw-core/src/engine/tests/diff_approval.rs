#![cfg(test)]

use crate::engine::MAX_APPROVAL_DIFF_BYTES;
use crate::engine::builtin_hook_dispatcher;
use crate::engine::diff_binding;
use crate::engine::pending_event::PendingEvent;
use crate::engine::session::SessionActor;
use crate::engine::tests::fixtures::models::ScriptedModel;
use crate::engine::tests::fixtures::support::collect_turn;
use crate::engine::tests::fixtures::support::config;
use crate::engine::tests::fixtures::support::next_matching;
use crate::engine::tests::fixtures::support::stop_script;
use crate::engine::tests::fixtures::support::tool_script;
use rw_tools::ToolLimits;
use rw_tools::ToolRegistry;
use rw_tools::WriteTool;
use rw_types::ApprovalBinding;
use rw_types::ApprovalDecision;
use rw_types::EngineEvent;
use rw_types::config::PermissionDecision;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn diff_approval_rejects_tampered_binding_without_consuming_the_prompt() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("bound.txt"), "before").expect("fixture");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "write",
                json!({"path": "bound.txt", "content": "after"}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("register write");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("write").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    let correct = diff_binding(request.approval_diff.as_ref().expect("diff"));
    assert!(
        !handle
            .approve_bound(
                request.id.clone(),
                rw_types::ToolInvocationId("previous-invocation".to_owned()),
                ApprovalDecision::AllowOnce,
                Some(correct.clone())
            )
            .await
            .expect("stale invocation rejected")
    );
    assert!(
        !handle
            .approve_bound(
                request.id.clone(),
                request.invocation_id.clone(),
                ApprovalDecision::AllowOnce,
                None,
            )
            .await
            .expect("missing approval binding")
    );
    for binding in [
        ApprovalBinding {
            proposal_id: "0".repeat(64),
            ..correct.clone()
        },
        ApprovalBinding {
            arguments_hash: "0".repeat(64),
            ..correct.clone()
        },
        ApprovalBinding {
            base_hash: "0".repeat(64),
            ..correct.clone()
        },
        ApprovalBinding {
            diff_hash: "0".repeat(64),
            ..correct.clone()
        },
    ] {
        assert!(
            !handle
                .approve_bound(
                    request.id.clone(),
                    request.invocation_id.clone(),
                    ApprovalDecision::AllowOnce,
                    Some(binding),
                )
                .await
                .expect("tampered approval response")
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("bound.txt")).expect("unchanged"),
        "before"
    );
    assert!(
        handle
            .approve_bound(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::AllowOnce,
                Some(correct)
            )
            .await
            .expect("bound approval")
    );
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::TurnFinished { .. })
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(root.path().join("bound.txt")).expect("written"),
        "after"
    );
}

#[tokio::test]
async fn mutation_diff_is_retained_when_policy_does_not_open_an_approval_dialog() {
    let root = TempDir::new().expect("tempdir");
    std::fs::write(root.path().join("inline.txt"), "before").expect("fixture");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "write",
                json!({"path": "inline.txt", "content": "after"}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("register write");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Allow,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("write").await.expect("message");
    let turn = collect_turn(&mut events).await;
    assert!(
        turn.iter()
            .all(|event| !matches!(event.kind, PendingEvent::PermissionRequested { .. }))
    );
    let diff = turn
        .iter()
        .find(|event| matches!(event.kind, PendingEvent::ToolDiffReady { .. }))
        .expect("retained diff");
    assert!(matches!(
        &diff.wire,
        EngineEvent::ToolDiffReady { diff, .. }
            if diff.path == "inline.txt"
                && diff.unified_diff.contains("-before")
                && diff.unified_diff.contains("+after")
    ));
    assert_eq!(
        std::fs::read_to_string(root.path().join("inline.txt")).expect("written"),
        "after"
    );
}

#[tokio::test]
async fn truncated_diff_cannot_be_approved_by_any_client() {
    let root = TempDir::new().expect("tempdir");
    let path = root.path().join("large.txt");
    std::fs::write(&path, "before").expect("fixture");
    let content = "x".repeat(MAX_APPROVAL_DIFF_BYTES + 1024);
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "write",
                json!({"path": "large.txt", "content": content}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("register write");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("write").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    let diff = request.approval_diff.as_ref().expect("diff");
    assert!(diff.truncated);
    let binding = diff_binding(diff);
    for decision in [
        ApprovalDecision::AllowOnce,
        ApprovalDecision::AllowSession,
        ApprovalDecision::AllowProject,
    ] {
        assert!(
            !handle
                .approve_bound(
                    request.id.clone(),
                    request.invocation_id.clone(),
                    decision,
                    Some(binding.clone())
                )
                .await
                .expect("truncated allow rejection")
        );
    }
    assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), "before");
    assert!(
        handle
            .approve_bound(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::Deny,
                Some(binding)
            )
            .await
            .expect("deny truncated proposal")
    );
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolCallFinished { .. })
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(path).expect("still unchanged"),
        "before"
    );
}

#[tokio::test]
async fn diff_approval_revalidates_current_base_before_mutation() {
    let root = TempDir::new().expect("tempdir");
    let path = root.path().join("race.txt");
    std::fs::write(&path, "approved base").expect("fixture");
    let model = Arc::new(ScriptedModel::new([
        tool_script(
            &[(
                "call",
                "write",
                json!({"path": "race.txt", "content": "agent write"}),
            )],
            &[],
        ),
        stop_script("done", &[]),
    ]));
    let mut tools = ToolRegistry::new();
    tools
        .register(Arc::new(WriteTool::new(ToolLimits::default())))
        .expect("register write");
    let handle = SessionActor::spawn(config(
        root.path(),
        model,
        Arc::new(tools),
        PermissionDecision::Ask,
        builtin_hook_dispatcher().expect("hooks"),
    ))
    .expect("actor");
    let mut events = handle.subscribe().expect("subscription");
    handle.send_message("write").await.expect("message");
    let event = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested { request, .. } = event.kind else {
        unreachable!("matching event")
    };
    let binding = diff_binding(request.approval_diff.as_ref().expect("diff"));
    let approved_base_hash = binding.base_hash.clone();
    std::fs::write(&path, "concurrent user edit").expect("race mutation");
    assert!(
        !handle
            .approve_bound(
                request.id,
                request.invocation_id.clone(),
                ApprovalDecision::AllowProject,
                Some(binding)
            )
            .await
            .expect("stale approval")
    );
    let refreshed = next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::PermissionRequested { .. })
    })
    .await;
    let PendingEvent::PermissionRequested {
        request: refreshed, ..
    } = refreshed.kind
    else {
        unreachable!("matching event")
    };
    let refreshed_binding = diff_binding(refreshed.approval_diff.as_ref().expect("new diff"));
    assert_ne!(refreshed_binding.base_hash, approved_base_hash);
    assert!(
        handle
            .approve_bound(
                refreshed.id,
                refreshed.invocation_id.clone(),
                ApprovalDecision::Deny,
                Some(refreshed_binding),
            )
            .await
            .expect("deny refreshed approval")
    );
    next_matching(&mut events, |kind| {
        matches!(kind, PendingEvent::ToolCallFinished { .. })
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(path).expect("race winner preserved"),
        "concurrent user edit"
    );
}
