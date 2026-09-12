//! Atomic derived updates amortize persistence independently of the source journal.
use super::DerivedDatabaseError;
use std::sync::Mutex;

const MAX_COMMITS: usize = 8;
const MAX_MUTATION_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct Pending {
    commits: usize,
    mutation_bytes: usize,
}

/// Bounds application commits pending a flush and releases unpinned retired pages.
/// The source journal is already durable before any of these updates are admitted.
/// Successful database close flushes remaining updates; a crash or failed close
/// rebuilds from that source. Mutation charges do not measure physical database
/// pages, and live read snapshots retain their pages independently of this policy.
#[derive(Default)]
pub(crate) struct DerivedCommitPolicy(Mutex<Pending>);

impl DerivedCommitPolicy {
    pub(crate) fn commit(
        &self,
        mut transaction: redb::WriteTransaction,
        mutation_bytes: usize,
    ) -> Result<(), DerivedDatabaseError> {
        let mut pending = self
            .0
            .lock()
            .map_err(|_| DerivedDatabaseError::Invalid("poisoned commit accounting"))?;
        let commits = pending.commits.saturating_add(1);
        let bytes = pending.mutation_bytes.saturating_add(mutation_bytes);
        let flush = commits >= MAX_COMMITS || bytes >= MAX_MUTATION_BYTES;
        transaction
            .set_durability(if flush {
                redb::Durability::Immediate
            } else {
                redb::Durability::None
            })
            .map_err(super::storage)?;
        // Failed transactions cannot reset the pending flush obligation.
        transaction.commit().map_err(super::storage)?;
        if flush {
            *pending = Pending::default();
        } else {
            *pending = Pending {
                commits,
                mutation_bytes: bytes,
            };
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
