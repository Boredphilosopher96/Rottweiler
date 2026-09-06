//! Exact effective child bindings for a separately validated live family target.
use crate::{journal_service::JournalService, projection_budget::ProjectionBudget};
use rw_core::{HostError, recovery::SubagentLifecycleIndex};
use rw_types::{
    SessionId,
    family_controls::{ChildControlTarget, ChildReadScopeResult},
    session_read::{SessionReadAncestor, SessionReadScope},
};
use std::sync::Arc;

pub(super) async fn resolve(
    journals: Arc<JournalService>,
    root: SessionId,
    target: ChildControlTarget,
) -> Result<ChildReadScopeResult, HostError> {
    target.validate().map_err(storage)?;
    let admission = journals.admit_read().map_err(storage)?;
    let (result, _source) =
        rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
            let mut budget = ProjectionBudget::new();
            let mut parent = root.clone();
            let mut admission = Some(admission);
            let mut previous = None;
            let mut ancestry = Vec::with_capacity(target.ancestry.len());
            for hop in &target.ancestry {
                let order = journals
                    .child_projection_order(&parent.0)
                    .map_err(storage)?;
                let _order = order
                    .lock()
                    .map_err(|_| storage("child projection owner poisoned"))?;
                let source = if let Some(source) = previous.take() {
                    journals.retarget(source, &parent.0).map_err(storage)?
                } else {
                    admission
                        .take()
                        .ok_or_else(|| storage("family source admission missing"))?
                        .capture(&parent.0)
                        .map_err(storage)?
                };
                let mut index = SubagentLifecycleIndex::open(&source.view).map_err(storage)?;
                let mut ready = index.is_current(&source.view).map_err(storage)?;
                while !ready && budget.take_batch() {
                    ready = !index.advance(&source.view).map_err(storage)?;
                }
                if !ready {
                    return Ok((
                        ChildReadScopeResult::CatchingUp {
                            session_id: parent,
                            through: index.through().map_err(storage)?,
                            target: source.view.last_sequence(),
                        },
                        Some(source),
                    ));
                }
                let view = index.snapshot(&source.view).map_err(storage)?;
                let binding = view
                    .binding(&hop.subagent_id)
                    .map_err(storage)?
                    .filter(|binding| binding.session_id == hop.session_id)
                    .ok_or_else(|| {
                        storage("live child has no matching effective canonical binding")
                    })?;
                ancestry.push(SessionReadAncestor {
                    subagent_id: hop.subagent_id.clone(),
                    session_id: hop.session_id.clone(),
                    source_sequence: binding.spawned,
                });
                parent.clone_from(&hop.session_id);
                drop(view);
                drop(index);
                previous = Some(source);
            }
            let scope = SessionReadScope::Descendant {
                root_session_id: root,
                ancestry,
            };
            scope.root(&target.session_id).map_err(storage)?;
            Ok::<_, HostError>((ChildReadScopeResult::Ready { scope }, previous))
        })
        .await
        .map_err(storage)??;
    Ok(result)
}
fn storage(error: impl std::fmt::Display) -> HostError {
    HostError::Query(format!("family source query failed: {error}"))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::resolve;
    use crate::journal_service::JournalService;
    use rw_store::session::SessionEventLog;
    use rw_types::{
        EngineEvent, EventMeta, SequenceId, SessionId, SubagentId,
        family_controls::{ChildControlHop, ChildControlTarget, ChildReadScopeResult},
        session_read::SessionReadScope,
    };
    fn meta(session: &str, sequence: u64) -> EventMeta {
        EventMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            session_id: SessionId(session.into()),
            sequence_id: SequenceId(sequence),
            emitted_at: "2026-09-04T00:00:00.000Z".into(),
            caused_by: None,
        }
    }
    fn hop(parent: &str, child: &str) -> ChildControlHop {
        ChildControlHop {
            subagent_id: SubagentId(format!("{parent}-agent")),
            session_id: SessionId(child.into()),
        }
    }
    #[tokio::test]
    async fn retained_terminal_bindings_resolve_exact_multi_hop_scope() {
        let root = tempfile::tempdir().expect("root");
        for (parent, child) in [("parent", "child"), ("child", "grandchild")] {
            let mut log = SessionEventLog::open(root.path(), parent).expect("journal");
            let id = hop(parent, child);
            log.append(EngineEvent::SubagentSpawned {
                meta: meta(parent, 0),
                subagent_id: id.subagent_id.clone(),
                child_session_id: id.session_id.clone(),
                task: "work".into(),
            })
            .expect("spawn");
            log.append(EngineEvent::SubagentFinished {
                meta: meta(parent, 1),
                subagent_id: id.subagent_id.clone(),
                result: rw_core::interrupted_subagent_recovery_result(&rw_core::SubagentHandle {
                    subagent_id: id.subagent_id,
                    session_id: id.session_id,
                }),
            })
            .expect("terminal");
        }
        let journals = JournalService::new(root.path()).expect("owner");
        let target = ChildControlTarget {
            ancestry: vec![hop("parent", "child"), hop("child", "grandchild")],
            session_id: SessionId("grandchild".into()),
        };
        let result = resolve(journals.clone(), SessionId("parent".into()), target.clone())
            .await
            .expect("query");
        let ChildReadScopeResult::Ready {
            scope:
                SessionReadScope::Descendant {
                    root_session_id,
                    ancestry,
                },
        } = result
        else {
            panic!("scope")
        };
        assert_eq!(root_session_id.0, "parent");
        assert_eq!(ancestry.len(), 2);
        assert!(
            ancestry
                .iter()
                .all(|hop| hop.source_sequence == SequenceId(0))
        );
        let mut wrong = target;
        wrong.ancestry[0].session_id = SessionId("foreign".into());
        assert!(
            resolve(journals, SessionId("parent".into()), wrong)
                .await
                .is_err()
        );
    }
}
