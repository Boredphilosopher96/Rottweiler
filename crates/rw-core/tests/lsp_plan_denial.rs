use async_trait::async_trait;
use rw_core::{PermissionApprover, PermissionGate, PermissionOutcome, PermissionRequest};
use rw_types::{ApprovalDecision, SessionMode, ToolCapability, config::PermissionDecision};

struct NeverPrompt;

#[async_trait]
impl PermissionApprover for NeverPrompt {
    async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
        panic!("plan-mode executable tools must be denied before prompting")
    }
}

#[tokio::test]
async fn lsp_execute_capability_is_denied_in_plan_and_discuss_modes() {
    let gate = PermissionGate::new(PermissionDecision::Allow);
    for mode in [SessionMode::Plan, SessionMode::Discuss] {
        let outcome = gate
            .authorize_in_mode(
                PermissionRequest {
                    id: "lsp-call".to_owned(),
                    tool_name: "diagnostics".to_owned(),
                    arguments: serde_json::json!({"path":"lib.rs"}),
                    capabilities: vec![ToolCapability::ReadFilesystem, ToolCapability::Execute],
                    approval_diff: None,
                },
                &NeverPrompt,
                None,
                mode,
            )
            .await;
        assert_eq!(outcome, PermissionOutcome::Denied);
    }
}
