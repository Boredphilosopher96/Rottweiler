use async_trait::async_trait;
use rw_core::AgentLoopError;
use rw_core::FolderTrustController;
use rw_core::FolderTrustOperation;
use rw_store::trust::FolderTrustStore;
use std::path::Path;
use std::path::PathBuf;

pub(super) fn project_approval_path(storage_root: &Path, workspace: &Path) -> PathBuf {
    let digest = blake3::hash(workspace.as_os_str().as_encoded_bytes())
        .to_hex()
        .to_string();
    storage_root
        .join("workspaces")
        .join(digest)
        .join("permission-approvals.json")
}

pub(super) struct RuntimeFolderTrustController {
    pub(super) store: FolderTrustStore,
    pub(super) workspaces: Vec<PathBuf>,
}

impl RuntimeFolderTrustController {
    pub(super) fn new(store_path: PathBuf, workspaces: Vec<PathBuf>) -> Self {
        Self {
            store: FolderTrustStore::new(store_path),
            workspaces,
        }
    }
}

pub(super) fn trust_confirmation_token(
    assessments: &[rw_store::trust::FolderTrustAssessment],
) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler-folder-trust-confirmation-v1\0");
    for assessment in assessments {
        let workspace = assessment.workspace().as_os_str().as_encoded_bytes();
        hasher.update(
            &u64::try_from(workspace.len())
                .unwrap_or(u64::MAX)
                .to_le_bytes(),
        );
        hasher.update(workspace);
        if let Some(executable_hash) = assessment.executable_hash() {
            hasher.update(executable_hash.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

pub(super) fn render_trust_assessments(
    assessments: &[rw_store::trust::FolderTrustAssessment],
) -> String {
    assessments
        .iter()
        .enumerate()
        .map(|(index, assessment)| {
            assessment.render_prompt_with_workspace(&format!("@root/{index}"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl FolderTrustController for RuntimeFolderTrustController {
    async fn execute(
        &self,
        operation: FolderTrustOperation,
    ) -> std::result::Result<String, AgentLoopError> {
        let store = self.store.clone();
        let workspaces = self.workspaces.clone();
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let trust_error = |_error: rw_store::trust::FolderTrustError| {
                AgentLoopError::Persistence("folder trust operation failed".to_owned())
            };
            let assessments = workspaces
                .iter()
                .map(|workspace| store.assess(workspace).map_err(&trust_error))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let untrustable = assessments
                .iter()
                .find_map(rw_store::trust::FolderTrustAssessment::inventory_failure);
            match operation {
                FolderTrustOperation::Status => Ok(render_trust_assessments(&assessments)),
                FolderTrustOperation::Grant { confirmation: None } => {
                    if let Some(failure) = untrustable {
                        return Err(AgentLoopError::InvalidConfiguration(format!(
                            "refusing to grant folder trust because the project extension inventory is incomplete at {}: {}",
                            failure.path().display(),
                            failure.message()
                        )));
                    }
                    let token = trust_confirmation_token(&assessments);
                    Ok(format!(
                        "{}\nreview the exact inventory and confirm with `/trust grant {token}`\n",
                        render_trust_assessments(&assessments)
                    ))
                }
                FolderTrustOperation::Grant {
                    confirmation: Some(confirmation),
                } => {
                    if let Some(failure) = untrustable {
                        return Err(AgentLoopError::InvalidConfiguration(format!(
                            "refusing to grant folder trust because the project extension inventory is incomplete at {}: {}",
                            failure.path().display(),
                            failure.message()
                        )));
                    }
                    let expected = trust_confirmation_token(&assessments);
                    if confirmation != expected {
                        return Err(AgentLoopError::InvalidConfiguration(
                            "folder trust confirmation is stale or does not match the current root inventories; run `/trust grant` again"
                                .to_owned(),
                        ));
                    }
                    store.grant_all(&assessments).map_err(&trust_error)?;
                    let current = workspaces
                        .iter()
                        .map(|workspace| store.assess(workspace).map_err(&trust_error))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(format!(
                        "{}\nfolder trust granted for all workspace roots; executable project configuration activates in the next session\n",
                        render_trust_assessments(&current)
                    ))
                }
                FolderTrustOperation::Revoke => {
                    store.revoke_all(&workspaces).map_err(&trust_error)?;
                    let current = workspaces
                        .iter()
                        .map(|workspace| store.assess(workspace).map_err(&trust_error))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(format!(
                        "{}\nfolder trust revoked for all workspace roots; executable project configuration unloads in the next session\n",
                        render_trust_assessments(&current)
                    ))
                }
            }
        })
        .await
        .map_err(|_error| {
            AgentLoopError::Persistence("folder trust operation failed".to_owned())
        })?
    }
}
