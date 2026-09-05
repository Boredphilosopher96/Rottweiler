use crate::InitDepth;
use crate::apply_init_plan;
use crate::engine::AgentLoopError;
use crate::engine::mutation_checkpoints::MutationCheckpointOutcome;
use crate::engine::session::SessionActorConfig;
use crate::engine::task_ownership;
use crate::engine::turn::TurnSignal;
use crate::engine::turn::validate_mutation_scope;
use crate::plan_init;
use rw_tools::CancellationToken;
use rw_tools::MutationScope;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) fn start_workspace_initialization(
    config: Arc<SessionActorConfig>,
    tasks: &task_ownership::ActorTasks,
    depth: InitDepth,
    mutation_turn: u64,
    call_id: String,
    signals: mpsc::UnboundedSender<TurnSignal>,
) {
    let name = match depth {
        InitDepth::Root => "init",
        InitDepth::Deep => "deep-init",
    };
    let errors = signals.clone();
    let workspace = config.workspace_root.clone();
    let session_id = config.session_id.clone();
    let checkpoints = Arc::clone(&config.checkpoints);
    if let Err(error) = tasks.spawn(config, CancellationToken::default(), async move {
        let result = async {
            let plan = tokio::task::spawn_blocking(move || {
                plan_init(&workspace, depth, crate::DEFAULT_INIT_FILE_BUDGET_BYTES)
            })
            .await
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?
            .map_err(|error| AgentLoopError::Persistence(error.to_string()))?;
            let scope = MutationScope::Paths(plan.files().keys().cloned().collect());
            validate_mutation_scope(&scope)?;
            let checkpoint = checkpoints
                .begin(&session_id, mutation_turn, &call_id, &scope)
                .await?;
            let applied = tokio::task::spawn_blocking(move || apply_init_plan(&plan)).await;
            let applied = match applied {
                Ok(result) => {
                    result.map_err(|error| AgentLoopError::Persistence(error.to_string()))
                }
                Err(error) => Err(AgentLoopError::Persistence(error.to_string())),
            };
            let outcome = if applied.is_ok() {
                MutationCheckpointOutcome::Completed
            } else {
                MutationCheckpointOutcome::Failed
            };
            checkpoints.finish(&checkpoint, outcome).await?;
            let created = applied?;
            let generated = created
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            Ok(format!(
                "generated {} instruction file(s): {generated}",
                created.len()
            ))
        }
        .await;
        if let Err(AgentLoopError::EffectsUnsettled(message)) = &result {
            let _ = signals.send(TurnSignal::EffectsUnsettled {
                message: message.clone(),
            });
        }
        let _ = signals.send(TurnSignal::InitializationComplete { name, result });
    }) {
        let _ = errors.send(TurnSignal::EffectsUnsettled {
            message: error.to_string(),
        });
    }
}
