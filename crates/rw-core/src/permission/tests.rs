#![allow(clippy::expect_used)]

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
};

use async_trait::async_trait;
use rw_tools::{CommandSafetyClassifier, MutationScope, ToolBehavior, ToolInvocationSemantics};
use rw_types::{
    ApprovalDecision, PermissionModeDescriptor, SessionMode, ToolCapability, UnifiedDiff,
    config::{PermissionConfig, PermissionDecision, PermissionRule},
};
use serde_json::Value;

use super::*;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Decision(ApprovalDecision);

#[async_trait]
impl PermissionApprover for Decision {
    async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
        self.0.clone()
    }
}

struct CountingDeny(AtomicUsize);

#[async_trait]
impl PermissionApprover for CountingDeny {
    async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
        self.0.fetch_add(1, Ordering::SeqCst);
        ApprovalDecision::Deny
    }
}

struct ChangeWorkspaceThenApprove {
    gate: Arc<PermissionGate>,
    replacement: PathBuf,
    decision: ApprovalDecision,
}

#[async_trait]
impl PermissionApprover for ChangeWorkspaceThenApprove {
    async fn decide(&self, _request: PermissionRequest) -> ApprovalDecision {
        self.gate
            .replace_workspace_roots([&self.replacement])
            .expect("workspace replacement");
        self.decision.clone()
    }
}

fn request(command: &str, capabilities: Vec<ToolCapability>) -> PermissionRequest {
    PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "call".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({ "command": command }),
        capabilities,
        approval_diff: None,
    }
}

fn bash_request(command: &str, cwd: &Path) -> PermissionRequest {
    PermissionRequest {
        invocation_id: rw_types::ToolInvocationId("fixture-invocation".to_owned()),
        id: "exact-bash".to_owned(),
        tool_name: "bash".to_owned(),
        arguments: json!({
            "command": command,
            "cwd": cwd,
            "env": {},
            "network_domains": [],
        }),
        capabilities: vec![ToolCapability::Execute],
        approval_diff: None,
    }
}

async fn authorize_registered_file_mutation(
    gate: &PermissionGate,
    request: PermissionRequest,
    approver: &dyn PermissionApprover,
) -> PermissionOutcome {
    let path = request
        .arguments
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .expect("file mutation path");
    let semantics = ToolInvocationSemantics {
        behavior: ToolBehavior::FileMutation,
        mutation_scope: MutationScope::Paths(vec![path.clone()]),
        workspace_paths: vec![path],
    };
    gate.authorize_registered_in_mode(request, &semantics, approver, None, SessionMode::Execute)
        .await
}

fn explicit_semantics(behavior: ToolBehavior) -> ToolInvocationSemantics {
    ToolInvocationSemantics {
        behavior,
        mutation_scope: MutationScope::None,
        workspace_paths: Vec::new(),
    }
}

async fn authorize_with_behavior(
    gate: &PermissionGate,
    request: PermissionRequest,
    behavior: ToolBehavior,
    approver: &dyn PermissionApprover,
) -> PermissionOutcome {
    gate.authorize_registered_in_mode(
        request,
        &explicit_semantics(behavior),
        approver,
        None,
        SessionMode::Execute,
    )
    .await
}

async fn authorize_with_behavior_in_mode(
    gate: &PermissionGate,
    request: PermissionRequest,
    behavior: ToolBehavior,
    approver: &dyn PermissionApprover,
    ask_override: Option<PermissionOutcome>,
    mode: SessionMode,
) -> PermissionOutcome {
    gate.authorize_registered_in_mode(
        request,
        &explicit_semantics(behavior),
        approver,
        ask_override,
        mode,
    )
    .await
}

fn independent_project_store(path: &Path) -> ProjectApprovalStore {
    ProjectApprovalStore {
        path: path.to_path_buf(),
        transaction: Mutex::new(()),
        cached: RwLock::new(BTreeSet::new()),
    }
}

mod identity;
mod persistence;
mod policy;
