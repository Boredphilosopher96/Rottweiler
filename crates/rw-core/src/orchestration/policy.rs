use std::{fmt::Write as _, sync::Arc, time::Duration};

use rw_tools::{McpToolPolicy, ToolRegistry, ToolResult, validate_mcp_virtual_tool};
use rw_types::{
    DiffArtifact, DiffArtifactRef, SessionMode, SubagentResult, SubagentStatus, ToolCapability,
    Usage,
};
use serde_json::{Value, json};

use super::{
    MAX_ARTIFACT_REF_FILES, MAX_ARTIFACT_REF_PATH_BYTES, MAX_ARTIFACT_REF_PREVIEW_BYTES,
    MAX_MODEL_SUBAGENT_SUMMARY_BYTES, MAX_MODEL_SUBAGENT_TEXT_BYTES, MAX_SUBAGENT_DIFF_BYTES,
    MAX_SUBAGENT_FINAL_TEXT_BYTES, MAX_SUBAGENT_TOUCHED_FILES, OrchestrationError, SubagentLimits,
    SubagentRequest, SubagentSession, SubagentTurnResult,
};

pub(super) fn validate_request(request: &SubagentRequest) -> Result<(), OrchestrationError> {
    if request.task.trim().is_empty() || request.task.len() > 64 * 1024 {
        return Err(OrchestrationError::InvalidRequest(
            "task must be 1-65536 bytes".to_owned(),
        ));
    }
    if request.agent.trim().is_empty() || request.model.trim().is_empty() {
        return Err(OrchestrationError::InvalidRequest(
            "agent and model alias must not be empty".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn restricted_registry(
    tools: &Arc<ToolRegistry>,
    requested: &[String],
    mode: SessionMode,
) -> Result<Arc<ToolRegistry>, OrchestrationError> {
    if requested.iter().any(|name| name == "ask_user") {
        return Err(OrchestrationError::InvalidRequest(
            "child agent allowlists cannot include interactive `ask_user`; delegate a bounded non-interactive task"
                .to_owned(),
        ));
    }
    if requested
        .iter()
        .any(|name| matches!(name.as_str(), "tool_search" | "mcp_call"))
    {
        return Err(OrchestrationError::InvalidRequest(
            "child agents must grant exact `mcp:<server>/<tool>` entries instead of generic MCP gateway tools"
                .to_owned(),
        ));
    }
    let mut mcp_grants = Vec::new();
    for name in requested.iter().filter(|name| name.starts_with("mcp:")) {
        validate_mcp_virtual_tool(name)
            .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
        mcp_grants.push(name.clone());
    }
    if !mcp_grants.is_empty() && mode != SessionMode::Execute {
        return Err(OrchestrationError::InvalidRequest(
            "MCP tools require an execute-mode child because remote mutation capabilities are opaque"
                .to_owned(),
        ));
    }
    let allowed = requested.iter().filter(|name| {
        if name.starts_with("mcp:") {
            return false;
        }
        mode == SessionMode::Execute
            || tools.descriptor(name).is_some_and(|descriptor| {
                descriptor
                    .capabilities
                    .capabilities()
                    .iter()
                    .all(|capability| matches!(capability, ToolCapability::ReadFilesystem))
            })
            || (mode == SessionMode::Plan && name.as_str() == "submit_plan")
    });
    let mut allowed = allowed.map(String::as_str).collect::<Vec<_>>();
    if !mcp_grants.is_empty() {
        allowed.extend(["tool_search", "mcp_call"]);
    }
    let policy = McpToolPolicy::restricted(mcp_grants)
        .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))?;
    tools
        .subset(allowed)
        .map(|registry| Arc::new(registry.with_mcp_tool_policy(policy)))
        .map_err(|error| OrchestrationError::InvalidRequest(error.to_string()))
}

pub(super) fn random_id() -> Result<String, OrchestrationError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        OrchestrationError::Session(format!("child id entropy failed: {error}"))
    })?;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}")
            .map_err(|error| OrchestrationError::Session(error.to_string()))?;
    }
    Ok(encoded)
}

pub(super) fn bound_turn_result(result: &mut SubagentTurnResult) {
    truncate_utf8(&mut result.final_text, MAX_SUBAGENT_FINAL_TEXT_BYTES);
    result.touched_files.truncate(MAX_SUBAGENT_TOUCHED_FILES);
    for path in &mut result.touched_files {
        truncate_utf8(path, 4096);
    }
    if result.diff_artifact.as_ref().is_some_and(|diff| {
        diff.unified_diff.len() > MAX_SUBAGENT_DIFF_BYTES
            || diff.touched_files.len() > MAX_SUBAGENT_TOUCHED_FILES
    }) {
        result.diff_artifact = None;
        result.status = SubagentStatus::Failed;
        "isolated child diff exceeded the durable artifact bound"
            .clone_into(&mut result.final_text);
    }
}

pub(super) fn model_facing_subagent_result(result: &SubagentResult) -> Value {
    let mut final_text = result.final_text.clone();
    let final_text_truncated = final_text.len() > MAX_MODEL_SUBAGENT_TEXT_BYTES;
    truncate_utf8(&mut final_text, MAX_MODEL_SUBAGENT_TEXT_BYTES);
    let mut touched_files = result.touched_files.clone();
    let touched_files_truncated = touched_files.len() > MAX_ARTIFACT_REF_FILES;
    touched_files.truncate(MAX_ARTIFACT_REF_FILES);
    for path in &mut touched_files {
        truncate_utf8(path, MAX_ARTIFACT_REF_PATH_BYTES);
    }
    json!({
        "subagent_id": result.subagent_id,
        "session_id": result.session_id,
        "status": result.status,
        "final_text": final_text,
        "final_text_truncated": final_text_truncated,
        "touched_files": touched_files,
        "touched_files_truncated": touched_files_truncated,
        "diff_artifact": result.diff_artifact.as_ref().map(diff_artifact_reference),
        "usage": result.usage,
        "cost": result.cost,
        "turns": result.turns,
        "duration_millis": result.duration_millis,
    })
}

pub(super) fn model_facing_subagent_tool_result(result: &SubagentResult) -> ToolResult {
    let mut summary = if result.final_text.is_empty() {
        format!("subagent {} finished", result.subagent_id.0)
    } else {
        result.final_text.clone()
    };
    truncate_utf8(&mut summary, MAX_MODEL_SUBAGENT_SUMMARY_BYTES);
    ToolResult::new(summary, model_facing_subagent_result(result))
}

/// Builds the canonical bounded model-facing reference for a durable diff artifact.
#[must_use]
pub fn diff_artifact_reference(artifact: &DiffArtifact) -> DiffArtifactRef {
    let mut touched_files = artifact.touched_files.clone();
    let manifest_truncated = touched_files.len() > MAX_ARTIFACT_REF_FILES;
    touched_files.truncate(MAX_ARTIFACT_REF_FILES);
    for file in &mut touched_files {
        truncate_utf8(&mut file.path, MAX_ARTIFACT_REF_PATH_BYTES);
    }
    let mut preview = artifact.unified_diff.clone();
    let preview_truncated = preview.len() > MAX_ARTIFACT_REF_PREVIEW_BYTES;
    truncate_utf8(&mut preview, MAX_ARTIFACT_REF_PREVIEW_BYTES);
    DiffArtifactRef {
        artifact_id: artifact.id.clone(),
        base_commit: artifact.base_commit.clone(),
        touched_files,
        manifest_truncated,
        patch_bytes: u64::try_from(artifact.unified_diff.len()).unwrap_or(u64::MAX),
        patch_hash: blake3::hash(artifact.unified_diff.as_bytes())
            .to_hex()
            .to_string(),
        preview,
        preview_truncated,
    }
}

pub(super) fn truncate_utf8(value: &mut String, limit: usize) {
    if value.len() <= limit {
        return;
    }
    let boundary = value
        .char_indices()
        .take_while(|(index, _)| *index <= limit)
        .last()
        .map_or(0, |(index, _)| index);
    value.truncate(boundary);
}

pub(super) fn zero_usage() -> Usage {
    Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: 0,
    }
}

pub(super) fn control_timeout(limits: SubagentLimits) -> Duration {
    limits.max_duration.min(Duration::from_secs(30))
}

pub(super) async fn bounded_cancel(
    session: &Arc<dyn SubagentSession>,
    limits: SubagentLimits,
) -> Result<(), OrchestrationError> {
    tokio::time::timeout(control_timeout(limits), session.cancel())
        .await
        .map_err(|_| OrchestrationError::Session("child cancellation timed out".to_owned()))?
}

pub(super) async fn bounded_close(
    session: &Arc<dyn SubagentSession>,
    durable_artifact: Option<&DiffArtifact>,
    limits: SubagentLimits,
) -> Result<(), OrchestrationError> {
    tokio::time::timeout(control_timeout(limits), session.close(durable_artifact))
        .await
        .map_err(|_| OrchestrationError::Session("child close timed out".to_owned()))?
}
