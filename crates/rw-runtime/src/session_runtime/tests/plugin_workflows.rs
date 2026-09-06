//! Durable SDK tasks resume from committed state and use source-bound rich actions.
#![allow(clippy::expect_used)]
use super::plugin_command_session::{compose_fixture_session, configure_plugin};
use rw_types::{
    ClientCommand, ClientId, CommandMeta, CommandOutcome, EngineEvent, PROTOCOL_VERSION, RequestId,
    SessionId, ToolInvocationId,
    extension_ui::{UiActionRequest, UiActionTarget, UiPanelSnapshot, UiPresentation},
};
use std::time::Duration;

#[tokio::test]
async fn sdk_task_reopen_preserves_receipt_and_rebinds_rich_actions() {
    let _admission = crate::native_fixture::admit().await;
    let root = tempfile::tempdir().expect("root");
    let storage = root.path().join("storage");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&storage).expect("storage");
    std::fs::create_dir(&workspace).expect("workspace");
    let workspace = workspace
        .canonicalize()
        .expect("canonical workspace identity");
    #[cfg(unix)]
    std::fs::set_permissions(
        &storage,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .expect("private storage");
    std::fs::write(workspace.join("broker.txt"), "task input").expect("input");
    configure_plugin(root.path(), &storage, &workspace, "task-workflow", &[]).await;
    let first = compose_fixture_session(&storage, &workspace, "persistent-task", false).await;
    let (started, reads, _) = run_status(&first.handle, "start").await;
    assert_eq!(reads, 1, "only task start reads the input");
    let task = read_task(&first.handle).await;
    assert_eq!(task["phase"], "ready");
    assert!(
        task["read_invocation"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    let old = panel(&first.handle).await;
    assert_eq!(started.owner, old.presentation.owner);
    first
        .handle
        .close()
        .await
        .expect("first generation settled");
    drop(first);

    let resumed = compose_fixture_session(&storage, &workspace, "persistent-task", true).await;
    let (summary, reads, _) = run_status(&resumed.handle, "status").await;
    assert_eq!(reads, 0, "resume does not repeat the committed input read");
    assert_eq!(read_task(&resumed.handle).await, task);
    let current = panel(&resumed.handle).await;
    assert_ne!(current.presentation.owner, old.presentation.owner);
    assert_eq!(summary.owner, current.presentation.owner);
    assert_eq!(summary.descriptor.actions[0].id, "complete");
    assert!(matches!(
        action(&resumed.handle, &old, "stale").await,
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(read_task(&resumed.handle).await, task);
    let mut events = resumed.handle.subscribe_live().expect("completion events");
    assert_eq!(
        action(&resumed.handle, &current, "complete").await,
        CommandOutcome::Accepted {}
    );
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EngineEvent::CommandFinished { name, .. } =
                events.recv().await.expect("action event")
                && name == "task-workflow"
            {
                break;
            }
        }
    })
    .await
    .expect("rich action completion");
    let completed = read_task(&resumed.handle).await;
    assert_eq!(completed["phase"], "done");
    assert_eq!(completed["read_invocation"], task["read_invocation"]);
    complete_tool_action(&resumed.handle, &completed).await;
    resumed
        .handle
        .close()
        .await
        .expect("resumed generation settled");
}

async fn read_task(handle: &rw_core::SessionHandle) -> serde_json::Value {
    handle
        .plugin_session_capability("task-workflow")
        .expect("bound namespace")
        .read_state()
        .await
        .expect("durable task snapshot")
        .entries
        .into_iter()
        .find(|entry| entry.key == "task")
        .expect("committed task")
        .value
}
async fn panel(handle: &rw_core::SessionHandle) -> UiPanelSnapshot {
    let panels = handle.ui_panels().await.expect("task panel");
    assert_eq!(panels.panels.len(), 1);
    panels.panels.into_iter().next().expect("panel")
}
async fn run_status(
    handle: &rw_core::SessionHandle,
    argument: &str,
) -> (UiPresentation, usize, ToolInvocationId) {
    let mut events = handle.subscribe_live().expect("workflow events");
    tokio::time::timeout(
        Duration::from_secs(10),
        handle.send_message(format!("/task-workflow {argument}")),
    )
    .await
    .unwrap_or_else(|_| {
        panic!(
            "workflow callback deadline during {argument}; next buffered event: {:?}",
            futures_util::FutureExt::now_or_never(events.recv())
        )
    })
    .expect("workflow command");
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut reads = 0;
        let mut summary = None;
        loop {
            match events.recv().await.expect("workflow event") {
                EngineEvent::ToolCallStarted { name, .. } if name == "read" => reads += 1,
                EngineEvent::ToolCallFinished {
                    presentation: Some(presentation),
                    invocation_id,
                    is_error,
                    ..
                } if presentation.owner.extension == "task-workflow" => {
                    assert!(!is_error);
                    assert_eq!(presentation.descriptor.id, "summary");
                    summary = Some((presentation, invocation_id));
                }
                EngineEvent::CommandFinished { name, .. } if name == "task-workflow" => {
                    let (presentation, invocation) = summary.expect("canonical rich tool result");
                    return (presentation, reads, invocation);
                }
                _ => {}
            }
        }
    })
    .await
    .expect("canonical workflow outcomes")
}
async fn action(
    handle: &rw_core::SessionHandle,
    panel: &UiPanelSnapshot,
    request: &str,
) -> CommandOutcome {
    handle
        .dispatch(ClientCommand::InvokeUiAction {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("local".into()),
                request_id: RequestId(request.into()),
            },
            session_id: SessionId("persistent-task".into()),
            request: UiActionRequest {
                owner: panel.presentation.owner.clone(),
                contribution_id: "task".into(),
                action_id: "complete".into(),
                target: UiActionTarget::Panel {
                    revision: panel.revision,
                },
            },
        })
        .await
        .expect("typed action admission")
}

async fn complete_tool_action(handle: &rw_core::SessionHandle, expected: &serde_json::Value) {
    let (presentation, reads, invocation_id) = run_status(handle, "summary").await;
    assert_eq!(reads, 0, "tool action preparation reuses the input receipt");
    let state = handle
        .plugin_session_capability("task-workflow")
        .expect("namespace");
    let before = state.read_state().await.expect("state before action");
    let mut events = handle.subscribe_live().expect("tool action events");
    let outcome = handle
        .dispatch(ClientCommand::InvokeUiAction {
            meta: CommandMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("local".into()),
                request_id: RequestId("source-tool-action".into()),
            },
            session_id: SessionId("persistent-task".into()),
            request: UiActionRequest {
                owner: presentation.owner,
                contribution_id: presentation.descriptor.id,
                action_id: "complete".into(),
                target: UiActionTarget::Tool { invocation_id },
            },
        })
        .await
        .expect("source action admission");
    assert_eq!(outcome, CommandOutcome::Accepted {});
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let EngineEvent::CommandFinished { name, .. } =
                events.recv().await.expect("action event")
                && name == "task-workflow"
            {
                break;
            }
        }
    })
    .await
    .expect("canonical source action completion");
    let after = state.read_state().await.expect("state after action");
    assert_ne!(
        after.revision, before.revision,
        "source action actually committed"
    );
    assert_eq!(&read_task(handle).await, expected);
}
