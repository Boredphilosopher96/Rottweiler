mod presentation;
use presentation::APPLY_PRESENTATION;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use rw_types::{DiffArtifact, SessionId, ToolCapability};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::registry::{
    CancellationToken, CapabilityManifest, MutationScope, Tool, ToolContext, ToolDescriptor,
    ToolError, ToolResult, WorkspaceBinding, input_schema, parse_input,
};

use super::{
    bounded_diagnostic, git_index_path, require_success, run_git, run_git_with_paths,
    validate_artifact_reference_id, validate_relative_path, validate_repository_root,
    verify_artifact,
};

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplyWorktreeDiffInput {
    /// The complete durable artifact returned by the isolated child.
    /// Exactly one of `artifact` and `artifact_id` must be supplied.
    #[serde(default)]
    pub artifact: Option<DiffArtifact>,
    /// A compact reference to an artifact retained by the authenticated parent session.
    /// Exactly one of `artifact` and `artifact_id` must be supplied.
    #[serde(default)]
    pub artifact_id: Option<String>,
}

/// A decoded artifact retains its source admission through preview or application.
pub struct AuthorizedDiffArtifact {
    artifact: DiffArtifact,
    _owner: Box<dyn Send + Sync>,
}
impl AuthorizedDiffArtifact {
    pub fn new(artifact: DiffArtifact, owner: impl Send + Sync + 'static) -> Self {
        Self {
            artifact,
            _owner: Box::new(owner),
        }
    }
    #[must_use]
    pub const fn artifact(&self) -> &DiffArtifact {
        &self.artifact
    }
}

/// Session-scoped provenance check against effective durable child results.
#[async_trait]
pub trait DiffArtifactAuthority: Send + Sync {
    /// # Errors
    /// Rejects unavailable authority or unproven owned read effects.
    async fn resolve(
        &self,
        parent_session: &SessionId,
        artifact_id: &str,
    ) -> Result<Option<AuthorizedDiffArtifact>, ToolError>;
}

/// Validate a complete artifact's digest and touched-path contract.
/// # Errors
/// Rejects malformed commit, digest or relative-path identities.
pub fn validate_diff_artifact(artifact: &DiffArtifact) -> Result<(), ToolError> {
    verify_artifact(artifact)
}

/// The only supported merge-back boundary. Core checkpoints its exact manifest
/// before execution because this tool declares filesystem mutation.
#[derive(Clone)]
pub struct ApplyWorktreeDiffTool {
    authority: Arc<dyn DiffArtifactAuthority>,
    apply_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for ApplyWorktreeDiffTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("ApplyWorktreeDiffTool").finish()
    }
}

impl ApplyWorktreeDiffTool {
    #[must_use]
    pub fn new(authority: Arc<dyn DiffArtifactAuthority>) -> Self {
        Self {
            authority,
            apply_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    async fn resolve_input(
        &self,
        context: &ToolContext,
        input: ApplyWorktreeDiffInput,
    ) -> Result<AuthorizedDiffArtifact, ToolError> {
        let session = context.session_id().ok_or_else(|| {
            ToolError::InvalidInput(
                "apply_worktree_diff requires an authenticated parent session".to_owned(),
            )
        })?;
        let (artifact_id, supplied) = match (input.artifact, input.artifact_id) {
            (Some(artifact), None) => {
                verify_artifact(&artifact)?;
                (artifact.id.clone(), Some(artifact))
            }
            (None, Some(artifact_id)) => {
                validate_artifact_reference_id(&artifact_id)?;
                (artifact_id, None)
            }
            (Some(_), Some(_)) | (None, None) => {
                return Err(ToolError::InvalidInput(
                    "apply_worktree_diff requires exactly one of artifact or artifact_id"
                        .to_owned(),
                ));
            }
        };
        let resolved = self
            .authority
            .resolve(session, &artifact_id)
            .await?
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "worktree diff was not durably produced for this parent session".to_owned(),
                )
            })?;
        verify_artifact(resolved.artifact())?;
        if supplied
            .as_ref()
            .is_some_and(|artifact| artifact != resolved.artifact())
        {
            return Err(ToolError::InvalidInput(
                "worktree diff was not durably produced for this parent session".to_owned(),
            ));
        }
        Ok(resolved)
    }
}

pub(super) fn apply_worktree_diff_input_schema() -> Value {
    let mut schema = input_schema::<ApplyWorktreeDiffInput>();
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "oneOf".to_owned(),
            json!([
                { "required": ["artifact"] },
                { "required": ["artifact_id"] }
            ]),
        );
    }
    schema
}

#[async_trait]
impl Tool for ApplyWorktreeDiffTool {
    async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "apply_worktree_diff".to_owned(),
            description: "Explicitly apply one isolated child diff with git 3-way conflict checks."
                .to_owned(),
            input_schema: apply_worktree_diff_input_schema(),
            capabilities: CapabilityManifest::new([
                ToolCapability::ReadFilesystem,
                ToolCapability::WriteFilesystem,
                ToolCapability::Execute,
            ]),
        }
    }

    fn workspace_binding(&self) -> WorkspaceBinding {
        WorkspaceBinding::RootIndependent
    }

    fn mutation_scope(&self, input: &Value) -> MutationScope {
        // The artifact crosses a durable/model-visible boundary. Even with its
        // integrity digest, it is not authenticated as engine-created at this
        // point, so checkpoint the full workspace before parsing it with Git.
        let _ = input;
        MutationScope::OpaqueWorkspace
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let input: ApplyWorktreeDiffInput = parse_input(input)?;
        let admitted = self.resolve_input(context, input).await?;
        let artifact = admitted.artifact();
        context.cancellation.check()?;
        let _guard = self.apply_lock.lock().await;
        validate_repository_root(context.workspace_root(), &context.cancellation).await?;
        validate_patch_manifest(context.workspace_root(), artifact, &context.cancellation).await?;

        let index = git_index_path(context.workspace_root(), &context.cancellation).await?;
        let temporary_index = tempfile::NamedTempFile::new().map_err(|source| ToolError::Io {
            operation: "allocate isolated git apply index",
            path: std::env::temp_dir(),
            source,
        })?;
        std::fs::copy(&index, temporary_index.path()).map_err(|source| ToolError::Io {
            operation: "copy git index for isolated preflight",
            path: index,
            source,
        })?;
        let temporary_worktree = tempfile::tempdir().map_err(|source| ToolError::Io {
            operation: "allocate isolated git apply worktree",
            path: std::env::temp_dir(),
            source,
        })?;
        let check = run_git_with_paths(
            context.workspace_root(),
            [
                OsString::from("apply"),
                OsString::from("--3way"),
                OsString::from("--cached"),
                OsString::from("--binary"),
                OsString::from("--whitespace=nowarn"),
                OsString::from("-"),
            ],
            Some(artifact.unified_diff.as_bytes()),
            &context.cancellation,
            Some(temporary_index.path()),
            Some(temporary_worktree.path()),
        )
        .await?;
        if !check.status.success() {
            return Err(ToolError::Command(format!(
                "worktree diff conflict; parent tree was not changed: {}",
                bounded_diagnostic(&check)
            )));
        }
        let apply = run_git(
            context.workspace_root(),
            [
                OsString::from("apply"),
                OsString::from("--3way"),
                OsString::from("--binary"),
                OsString::from("--whitespace=nowarn"),
                OsString::from("-"),
            ],
            Some(artifact.unified_diff.as_bytes()),
            &context.cancellation,
        )
        .await?;
        if !apply.status.success() {
            return Err(ToolError::Command(format!(
                "worktree diff failed after checkpointed preflight: {}",
                bounded_diagnostic(&apply)
            )));
        }
        Ok(ToolResult::new(
            format!(
                "Applied isolated diff {} to {} file(s).",
                artifact.id,
                artifact.touched_files.len()
            ),
            json!({
                "artifact_id": artifact.id,
                "base_commit": artifact.base_commit,
                "touched_files": artifact.touched_files,
            }),
        )
        .with_presentation(APPLY_PRESENTATION.plan()?))
    }

    async fn approval_preview(
        &self,
        context: &ToolContext,
        input: &Value,
    ) -> Result<Option<crate::ApprovalPreview>, ToolError> {
        let input: ApplyWorktreeDiffInput = parse_input(input.clone())?;
        let admitted = self.resolve_input(context, input).await?;
        let artifact = admitted.artifact();
        let after = serde_json::to_vec_pretty(artifact)
            .map_err(|source| ToolError::Output(source.to_string()))?;
        Ok(Some(crate::ApprovalPreview {
            path: PathBuf::from(format!(".rottweiler/diff-artifacts/{}.json", artifact.id)),
            before: None,
            after,
        }))
    }
}

pub(super) async fn validate_patch_manifest(
    root: &Path,
    artifact: &DiffArtifact,
    cancellation: &CancellationToken,
) -> Result<(), ToolError> {
    let output = run_git(
        root,
        [
            OsString::from("apply"),
            OsString::from("--numstat"),
            OsString::from("-z"),
            OsString::from("--binary"),
            OsString::from("-"),
        ],
        Some(artifact.unified_diff.as_bytes()),
        cancellation,
    )
    .await?;
    require_success("inspect worktree diff manifest", &output)?;
    let mut patch_paths = BTreeSet::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let record = std::str::from_utf8(record)
            .map_err(|_| ToolError::Output("git emitted a non-UTF-8 patch path".to_owned()))?;
        let mut fields = record.splitn(3, '\t');
        let _added = fields.next();
        let _deleted = fields.next();
        let path = fields.next().ok_or_else(|| {
            ToolError::Output("git emitted malformed patch statistics".to_owned())
        })?;
        validate_relative_path(Path::new(path))?;
        patch_paths.insert(path.to_owned());
    }
    let manifest_paths = artifact
        .touched_files
        .iter()
        .map(|file| file.path.clone())
        .collect::<BTreeSet<_>>();
    if patch_paths != manifest_paths {
        return Err(ToolError::InvalidInput(
            "worktree diff manifest does not match the patch paths".to_owned(),
        ));
    }
    Ok(())
}
