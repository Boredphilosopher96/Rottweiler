//! Mode-independent, source-indexed task snapshots on the shared journal read budget.
use crate::journal_service::JournalService;
use rw_core::{HostError, todo_projection::TodoProjector};
use rw_types::{SessionId, todo::TodoReadResult};
use std::sync::Arc;

use crate::projection_budget::ProjectionBudget;

pub(crate) async fn read_todos(
    journals: Arc<JournalService>,
    session: SessionId,
    authorize: impl FnOnce(&mut ProjectionBudget) -> Result<(), HostError> + Send + 'static,
) -> Result<TodoReadResult, HostError> {
    SessionId::validate(&session.0).map_err(storage)?;
    // Admission is held during queued authorization, descriptor capture, index
    // work, and the completed-but-unconsumed result. Caller drop cannot detach it.
    let admission = journals.admit_read().map_err(storage)?;
    let (result, _lease) =
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let mut budget = ProjectionBudget::new();
            authorize(&mut budget)?;
            let lease = admission.capture(&session.0).map_err(storage)?;
            let result = read(&lease.view, &mut budget);
            Ok::<_, HostError>((result, lease))
        })
        .await
        .map_err(|_| HostError::Query("task projection worker failed".into()))??;
    result
}
fn read(
    source: &rw_store::session::journal::JournalReadView,
    budget: &mut ProjectionBudget,
) -> Result<TodoReadResult, HostError> {
    let mut projector = TodoProjector::open(source).map_err(storage)?;
    if projector.through().map_err(storage)? == source.last_sequence() {
        return Ok(TodoReadResult::Ready {
            todos: projector.snapshot(source).map_err(storage)?,
        });
    }
    while budget.take_batch() {
        if !projector.advance(source).map_err(storage)? {
            return Ok(TodoReadResult::Ready {
                todos: projector.snapshot(source).map_err(storage)?,
            });
        }
    }
    Ok(TodoReadResult::CatchingUp {
        through: projector.through().map_err(storage)?,
        target: source.last_sequence(),
    })
}
fn storage(error: impl std::fmt::Display) -> HostError {
    HostError::Query(format!("task state query failed: {error}"))
}

#[cfg(test)]
mod tests;
