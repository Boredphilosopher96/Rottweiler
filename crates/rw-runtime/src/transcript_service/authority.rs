//! A read grants no authority beyond an independently authorized root and its effective children.

use super::{ProjectionRead, TranscriptReader, page::storage};
use crate::projection_budget::ProjectionBudget;
use rw_core::{HostError, transcript::matches_child_source};
use rw_types::{SessionId, session_read::SessionReadScope};

impl TranscriptReader {
    pub(crate) fn authorize_scope(
        &self,
        target: &SessionId,
        scope: &SessionReadScope,
        budget: &mut ProjectionBudget,
    ) -> Result<(), HostError> {
        let root = scope
            .root(target)
            .map_err(|message| HostError::Protocol(message.into()))?;
        let SessionReadScope::Descendant { ancestry, .. } = scope else {
            return Ok(());
        };
        let mut parent = root;
        for child in ancestry {
            match self.projected_with_budget(parent, budget, |index, _journal| {
                matches_child_source(index, child).map_err(storage)
            })? {
                ProjectionRead::Ready(true) => parent = &child.session_id,
                ProjectionRead::Ready(false) => {
                    return Err(HostError::Protocol(
                        "historical child association is unavailable".into(),
                    ));
                }
                ProjectionRead::CatchingUp { .. } => {
                    return Err(HostError::Query(
                        "read ancestry is catching up; retry the read".into(),
                    ));
                }
            }
        }
        Ok(())
    }
}
