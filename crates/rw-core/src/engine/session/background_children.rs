//! Session-owned lifecycle delivery remains open through parent cleanup.
use super::child_progress::HostedChildProgress;
use crate::engine::{
    pending_event::PendingEvent,
    turn::{TurnSignal, persist_event},
};
use async_trait::async_trait;
use rw_tools::{SubagentEventSink, SubagentLifecycleEvent, SubagentProgressEvent, ToolError};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};

pub(super) struct BackgroundChildren {
    signals: mpsc::UnboundedSender<TurnSignal>,
    progress: Arc<HostedChildProgress>,
    lifecycle: Mutex<()>,
}
impl BackgroundChildren {
    pub(super) fn shared(signals: mpsc::UnboundedSender<TurnSignal>) -> Arc<dyn SubagentEventSink> {
        Arc::new(Self {
            signals,
            progress: HostedChildProgress::new(),
            lifecycle: Mutex::new(()),
        })
    }
}
#[async_trait]
impl SubagentEventSink for BackgroundChildren {
    fn progress_budget(&self) -> rw_tools::ChildProgressBudget {
        self.progress.budget.clone()
    }
    async fn lifecycle(&self, event: SubagentLifecycleEvent) -> Result<(), ToolError> {
        // At most one durable lifecycle body waits in the actor signal queue;
        // retained-child admission bounds producers and their owned results.
        let _order = self.lifecycle.lock().await;
        let (pending, finished) = match event {
            SubagentLifecycleEvent::Spawned {
                subagent_id,
                child_session_id,
                task,
            } => {
                self.progress
                    .register(&subagent_id, &child_session_id)
                    .map_err(failure)?;
                (
                    PendingEvent::SubagentSpawned {
                        subagent_id,
                        child_session_id,
                        task,
                    },
                    None,
                )
            }
            SubagentLifecycleEvent::Finished {
                subagent_id,
                result,
            } => {
                self.progress
                    .validate_finish(&subagent_id, &result.session_id)
                    .map_err(failure)?;
                if subagent_id != result.subagent_id {
                    return Err(failure("child completion identity mismatch"));
                }
                (
                    PendingEvent::SubagentFinished {
                        subagent_id: subagent_id.clone(),
                        result: *result,
                    },
                    Some(subagent_id),
                )
            }
        };
        persist_event(&self.signals, pending)
            .await
            .map_err(failure)?;
        if let Some(child) = finished {
            self.progress.finish(&child);
        }
        Ok(())
    }
    async fn progress(&self, event: SubagentProgressEvent) -> Result<(), ToolError> {
        self.progress
            .publish_with(event, |slot| {
                self.signals
                    .send(TurnSignal::SubagentProgress(slot))
                    .is_ok()
            })
            .map_err(failure)
    }
}
fn failure(error: impl std::fmt::Display) -> ToolError {
    ToolError::Output(error.to_string())
}

impl super::SessionHandle {
    pub(in crate::engine) fn background_subagent_event_sink(&self) -> Arc<dyn SubagentEventSink> {
        Arc::clone(&self.background_children)
    }
}
