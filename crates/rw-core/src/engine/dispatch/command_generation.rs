//! External generation preparation runs outside actor dispatch; publication does not.
use super::{DispatchContext, command_job::PreparedCommand};
use crate::engine::{
    AgentLoopError, RuntimePublication,
    commands::{SessionCommandAction, SessionCommandOutput, WorkspaceRuntimeGeneration},
    pending_event::PendingEvent,
    session::SessionActorConfig,
    session_extension::SessionExtensionSnapshot,
    turn::emit,
};
use rw_tools::ToolContext;
use std::{path::Path, sync::Arc};

pub(in crate::engine) enum PreparedChange {
    None,
    Workspace {
        generation: Box<WorkspaceRuntimeGeneration>,
        extensions: SessionExtensionSnapshot,
    },
    Extensions(SessionExtensionSnapshot),
}
impl PreparedChange {
    pub(super) fn requires_publication(&self) -> bool {
        match self {
            Self::None => false,
            Self::Workspace {
                generation,
                extensions,
            } => {
                matches!(generation.publication, RuntimePublication::Prepared(_))
                    || matches!(extensions.publication, RuntimePublication::Prepared(_))
            }
            Self::Extensions(snapshot) => {
                matches!(snapshot.publication, RuntimePublication::Prepared(_))
            }
        }
    }
    pub(super) async fn abort(&self, config: &SessionActorConfig) {
        if let Self::Workspace { generation, .. } = self {
            let _ = config
                .workspace_roots
                .abort_generation(generation.generation)
                .await;
        }
    }
}

fn snapshot(config: &SessionActorConfig) -> SessionExtensionSnapshot {
    SessionExtensionSnapshot {
        publication: RuntimePublication::Active,
        ui: config.ui.clone(),
        revision: config.workspace_generation,
        workspace_roots: Arc::from(
            std::iter::once(config.workspace_root.clone())
                .chain(config.additional_workspace_roots.iter().cloned())
                .collect::<Vec<_>>(),
        ),
        tools: config.tools.clone(),
        hooks: config.hooks.clone(),
        commands: config.commands.clone(),
    }
}

pub(super) async fn prepare_development(
    source: Option<&Path>,
    config: &SessionActorConfig,
) -> Result<PreparedCommand, AgentLoopError> {
    let current = snapshot(config);
    let extensions = if let Some(source) = source {
        config.extension_development.attach(source, current).await?
    } else {
        config.extension_development.detach().await?
    };
    Ok(PreparedCommand {
        output: SessionCommandOutput {
            message: if source.is_some() {
                "development plugin attached"
            } else {
                "development plugin detached"
            }
            .into(),
            action: SessionCommandAction::None,
        },
        change: PreparedChange::Extensions(extensions),
    })
}

pub(super) async fn prepare_output(
    mut output: SessionCommandOutput,
    config: &SessionActorConfig,
    next_turn: u64,
) -> Result<PreparedCommand, AgentLoopError> {
    let SessionCommandAction::AddWorkspaceRoot { path } = &output.action else {
        return Ok(PreparedCommand {
            output,
            change: PreparedChange::None,
        });
    };
    let current = snapshot(config);
    let generation = config
        .workspace_roots
        .append_root(
            path,
            &current.workspace_roots,
            config.workspace_generation,
            next_turn,
            config.permissions.clone(),
        )
        .await?;
    let valid = config.workspace_generation.checked_add(1) == Some(generation.generation)
        && generation.effective_from_turn == next_turn
        && generation.roots.len() == current.workspace_roots.len() + 1
        && generation
            .roots
            .iter()
            .take(current.workspace_roots.len())
            .eq(current.workspace_roots.iter())
        && generation
            .roots
            .iter()
            .all(|root| std::fs::canonicalize(root).is_ok_and(|canonical| canonical == *root));
    if !valid {
        config
            .workspace_roots
            .abort_generation(generation.generation)
            .await?;
        return Err(AgentLoopError::InvalidConfiguration(
            "workspace controller returned a non-canonical or non-append generation".into(),
        ));
    }
    // Rebase may retire native effects and call the actor. It belongs to this
    // independently owned task, before any durable marker or publication.
    let rebased = config
        .extension_development
        .rebase(SessionExtensionSnapshot {
            publication: generation.publication.clone(),
            ui: generation.ui.clone(),
            revision: generation.generation,
            workspace_roots: Arc::from(generation.roots.clone()),
            tools: generation.tools.clone(),
            hooks: generation.hooks.clone(),
            commands: generation.commands.clone(),
        })
        .await;
    let (extensions, detached) = match rebased {
        Ok(value) => value,
        Err(error) => {
            config
                .workspace_roots
                .abort_generation(generation.generation)
                .await?;
            return Err(error);
        }
    };
    output.action = SessionCommandAction::None;
    output.message = format!("added workspace root @root/{}", generation.roots.len() - 1);
    if detached {
        output
            .message
            .push_str("; development plugin detached after a registry collision");
    }
    Ok(PreparedCommand {
        output,
        change: PreparedChange::Workspace {
            generation: Box::new(generation),
            extensions,
        },
    })
}

pub(super) async fn apply(
    change: PreparedChange,
    context: DispatchContext<'_>,
) -> Result<(), AgentLoopError> {
    let needs_publication = change.requires_publication();
    let result = match &change {
        PreparedChange::None => Ok(()),
        PreparedChange::Extensions(snapshot) => {
            install_extensions(snapshot, &context);
            *context.config = Arc::new(context.config.with_extension_snapshot(snapshot));
            snapshot.publication.publish()
        }
        PreparedChange::Workspace {
            generation,
            extensions,
        } => apply_workspace(generation, extensions, context).await,
    };
    // The caller poisons admission when a prepared external boundary cannot be
    // published. It must never resume a config whose endpoints were retired.
    if needs_publication {
        result.map_err(|error| AgentLoopError::EffectsUnsettled(error.to_string()))
    } else {
        result
    }
}

fn install_extensions(snapshot: &SessionExtensionSnapshot, context: &DispatchContext<'_>) {
    *context
        .command_descriptors
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) =
        Arc::from(snapshot.commands.descriptors().cloned().collect::<Vec<_>>());
}

async fn apply_workspace(
    generation: &WorkspaceRuntimeGeneration,
    extensions: &SessionExtensionSnapshot,
    mut context: DispatchContext<'_>,
) -> Result<(), AgentLoopError> {
    let replacement = ToolContext::from_workspace_roots(&generation.roots)
        .map_err(|_error| AgentLoopError::ToolContext("workspace tool context could not prepare".into()))?
        .with_session_id(context.config.session_id.clone())
        .with_mcp_tool_policy(context.config.tools.mcp_tool_policy().clone());
    let result = commit_workspace(generation, &mut context).await;
    if let Err(error) = result {
        context
            .config
            .workspace_roots
            .abort_generation(generation.generation)
            .await?;
        return Err(error);
    }
    context
        .config
        .workspace_roots
        .finalize_generation(generation.generation);
    let next = context
        .config
        .with_workspace_generation(generation, &context.state.mode_id)
        .with_extension_snapshot(extensions);
    install_extensions(extensions, &context);
    *context
        .mode_registry
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = next.modes.clone();
    *context.config = Arc::new(next);
    *context.tool_context = replacement;
    // A rebase owns the one complete publication, including the root candidate.
    extensions.publication.publish()
}

async fn commit_workspace(
    generation: &WorkspaceRuntimeGeneration,
    context: &mut DispatchContext<'_>,
) -> Result<(), AgentLoopError> {
    context
        .config
        .workspace_roots
        .prepare_commit_generation(generation.generation)
        .await
        .map_err(|_error| {
            AgentLoopError::Persistence("workspace root generation could not commit".into())
        })?;
    let roots = generation
        .roots
        .iter()
        .enumerate()
        .map(|(index, _)| rw_types::WorkspaceRootDescriptor {
            index: u32::try_from(index).unwrap_or(u32::MAX),
            path: format!("@root/{index}"),
            machine_local: false,
        })
        .collect();
    emit(
        context.state,
        context.events,
        &context.config.event_sink,
        PendingEvent::WorkspaceRootsChanged {
            generation: generation.generation,
            effective_from_turn: generation.effective_from_turn,
            roots,
        },
    )
    .await
    .map(|_| ())
    .map_err(|_error| {
        AgentLoopError::Persistence("workspace root change event could not persist".into())
    })
}
