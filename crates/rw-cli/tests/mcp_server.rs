#![allow(clippy::expect_used)]

use std::{fs, path::Path, process::Stdio};

use rmcp::{
    ServiceExt as _,
    model::CallToolRequestParams,
    transport::{ConfigureCommandExt as _, TokioChildProcess},
};
use rw_providers::{FinishReason, ProviderEvent};
use serde_json::{Value, json};
use tempfile::TempDir;

fn arguments(value: &Value) -> rmcp::model::JsonObject {
    value.as_object().cloned().expect("object arguments")
}

fn structured(result: &rmcp::model::CallToolResult) -> &Value {
    assert_eq!(
        result.is_error,
        Some(false),
        "tool returned an error: {:?}",
        result.content
    );
    result
        .structured_content
        .as_ref()
        .unwrap_or_else(|| panic!("tool returned no structured result: {:?}", result.content))
}

fn private_directory(path: &Path) {
    fs::create_dir_all(path).expect("private directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private permissions");
    }
}

async fn client(
    workspace: &Path,
    home: &Path,
    script: &Path,
) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let transport = TokioChildProcess::new(
        tokio::process::Command::new(env!("CARGO_BIN_EXE_rw")).configure(|command| {
            command
                .env_clear()
                .env("HOME", home)
                .env("ROTTWEILER_HOME", home)
                .current_dir(workspace)
                .arg("--in-memory-replay-script")
                .arg(script)
                .arg("mcp-server")
                .arg("stdio")
                .arg("--workspace")
                .arg(workspace)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit());
        }),
    )
    .expect("spawn rw MCP server");
    ().serve(transport).await.expect("initialize MCP client")
}

#[tokio::test]
async fn another_agent_drives_real_rw_process_without_seeing_foreign_sessions() {
    let root = TempDir::new().expect("root");
    let workspace = root.path().join("workspace");
    let home = root.path().join("home");
    private_directory(&workspace);
    private_directory(&home);
    fs::write(workspace.join("visible.txt"), "mcp-visible\n").expect("workspace fixture");
    let script = root.path().join("provider.json");
    fs::write(
        &script,
        serde_json::to_vec(&vec![vec![
            ProviderEvent::TextDelta {
                text: "accepted by replay agent".to_owned(),
            },
            ProviderEvent::Finished {
                reason: FinishReason::Stop,
            },
        ]])
        .expect("provider script"),
    )
    .expect("write provider script");

    let mut first = client(&workspace, &home, &script).await;
    assert_eq!(first.list_all_tools().await.expect("tool list").len(), 4);
    let foreign = first
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_create")
                .with_arguments(arguments(&json!({}))),
        )
        .await
        .expect("create foreign session");
    let foreign_id = structured(&foreign)["id"]
        .as_str()
        .expect("foreign id")
        .to_owned();
    first.close().await.expect("close first server");

    let mut second = client(&workspace, &home, &script).await;
    let read = second
        .call_tool(
            CallToolRequestParams::new("rottweiler_tools_call").with_arguments(arguments(&json!({
                "name": "read",
                "arguments": {"path": "visible.txt"}
            }))),
        )
        .await
        .expect("call approved read tool");
    assert_eq!(read.is_error, Some(false));
    assert_eq!(structured(&read)["content"], "mcp-visible");

    let created = second
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_create")
                .with_arguments(arguments(&json!({"title":"owned"}))),
        )
        .await
        .expect("create owned session");
    let owned_id = structured(&created)["id"]
        .as_str()
        .expect("owned id")
        .to_owned();
    assert_ne!(foreign_id, owned_id);

    let listed = second
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_list")
                .with_arguments(arguments(&json!({}))),
        )
        .await
        .expect("list sessions");
    let sessions = structured(&listed).as_array().expect("session list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], owned_id);
    assert!(sessions.iter().all(|session| session["id"] != foreign_id));

    let sent = second
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_send").with_arguments(arguments(
                &json!({"session_id": owned_id, "message": "hello from another agent"}),
            )),
        )
        .await
        .expect("send owned message");
    assert_eq!(sent.is_error, Some(false));

    let denied = second
        .call_tool(
            CallToolRequestParams::new("rottweiler_sessions_send").with_arguments(arguments(
                &json!({"session_id": foreign_id, "message": "steal driver"}),
            )),
        )
        .await
        .expect("foreign send result");
    assert_eq!(denied.is_error, Some(true));
    second.close().await.expect("close second server");
}
