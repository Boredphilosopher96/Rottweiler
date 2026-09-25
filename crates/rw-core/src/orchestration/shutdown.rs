//! Parent shutdown settles all live children without deleting restart identities.
use super::{OrchestrationError, SessionState, SubagentOrchestrator, control_timeout};
use rw_types::SessionId;
use std::sync::Arc;

impl SubagentOrchestrator {
    pub(super) async fn suspend_parent(
        &self,
        parent: &SessionId,
    ) -> Result<(), OrchestrationError> {
        self.drain_queue(parent).await;
        let children = {
            let sessions = self
                .inner
                .sessions
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            sessions
                .values()
                .filter(|child| &child.parent_session_id == parent)
                .map(|child| {
                    if let Some(cancellation) = &child.cancellation {
                        cancellation.cancel();
                    }
                    (
                        child.handle.clone(),
                        Arc::clone(&child.session),
                        child.state == SessionState::Active,
                    )
                })
                .collect::<Vec<_>>()
        };
        let settled = futures_util::future::join_all(children.into_iter().map(
            |(handle, session, active)| async move {
                let mut failure = None;
                if active {
                    if let Err(error) = super::bounded_cancel(&session, self.inner.limits).await {
                        failure = Some(error.to_string());
                    }
                    match tokio::time::timeout(
                        control_timeout(self.inner.limits),
                        self.wait(&handle),
                    )
                    .await
                    {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => {
                            failure.get_or_insert_with(|| error.to_string());
                        }
                        Err(_) => {
                            failure.get_or_insert_with(|| {
                                "child completion did not settle before shutdown deadline".into()
                            });
                        }
                    }
                }
                match tokio::time::timeout(control_timeout(self.inner.limits), session.suspend())
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        failure.get_or_insert_with(|| error.to_string());
                    }
                    Err(_) => {
                        failure.get_or_insert_with(|| {
                            "child suspension did not settle before shutdown deadline".into()
                        });
                    }
                }
                if let Some(failure) = failure {
                    return Err(OrchestrationError::EffectsUnsettled(failure));
                }
                self.inner
                    .sessions
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&handle.subagent_id);
                self.inner
                    .session_depths
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&handle.session_id);
                Ok(())
            },
        ))
        .await;
        crate::engine::control_observation::changed();
        settled
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .map(|_| ())
    }
}
