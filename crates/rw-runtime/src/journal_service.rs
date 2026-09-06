//! Runtime-owned journal commit admission and acknowledged read prefixes.
mod commits;
mod retained;
use commits::JournalCommits;

use miette::{Result, miette};
use rw_store::session::journal::{JournalReadView, JournalRoot};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, RwLock, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_ACTIVE_JOURNALS: usize = 1024;
const MAX_READ_VIEWS: usize = 8;

pub(crate) struct JournalService {
    pub(crate) commits: Arc<JournalCommits>,
    retained_history: retained::HistoryRetentions,
    root: JournalRoot,
    active: Mutex<HashMap<String, Weak<JournalPublication>>>,
    child_projection_orders: Mutex<HashMap<String, Weak<Mutex<()>>>>,
    admission: Arc<Semaphore>,
}

pub(crate) struct JournalReadAdmission {
    service: Arc<JournalService>,
    permit: OwnedSemaphorePermit,
}
impl JournalReadAdmission {
    pub(crate) fn capture(self, session: &str) -> Result<JournalReadLease> {
        self.service.capture_admitted(session, self.permit)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct JournalReadLease {
    pub(crate) view: JournalReadView,
    _permit: Arc<OwnedSemaphorePermit>,
}

#[derive(Debug)]
pub(crate) struct JournalPublication {
    committed: RwLock<JournalReadView>,
}

impl JournalPublication {
    pub(crate) fn publish(&self, view: JournalReadView) {
        let previous = {
            let mut committed = self
                .committed
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *committed, view)
        };
        drop(previous);
    }

    pub(crate) fn last_sequence(&self) -> Option<rw_types::SequenceId> {
        self.committed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last_sequence()
    }

    pub(crate) fn capture(&self) -> JournalReadView {
        self.committed
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(crate) struct JournalRegistration {
    reads: Arc<JournalService>,
    session: String,
    pub(crate) publisher: Arc<JournalPublication>,
}

impl Drop for JournalRegistration {
    fn drop(&mut self) {
        let mut active = self
            .reads
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if active
            .get(&self.session)
            .is_some_and(|owner| Weak::ptr_eq(owner, &Arc::downgrade(&self.publisher)))
        {
            active.remove(&self.session);
        }
    }
}

impl JournalService {
    pub(crate) fn new(root: &Path) -> Result<Arc<Self>> {
        Ok(Arc::new(Self {
            commits: JournalCommits::new(),
            retained_history: retained::HistoryRetentions::new(),
            root: JournalRoot::open(root)
                .map_err(|error| miette!("journal root could not open: {error}"))?,
            active: Mutex::new(HashMap::new()),
            child_projection_orders: Mutex::new(HashMap::new()),
            admission: Arc::new(Semaphore::new(MAX_READ_VIEWS)),
        }))
    }

    /// All lifecycle and presentation readers serialize this session's derived writer.
    /// Acquire its lock before capturing a prefix so a waiting read cannot go behind the index.
    pub(crate) fn child_projection_order(&self, session: &str) -> Result<Arc<Mutex<()>>> {
        rw_types::SessionId::validate(session)
            .map_err(|error| miette!("child projection identity: {error}"))?;
        let mut orders = self
            .child_projection_orders
            .lock()
            .map_err(|_| miette!("child projection registry is poisoned"))?;
        orders.retain(|_, order| order.strong_count() > 0);
        if let Some(order) = orders.get(session).and_then(Weak::upgrade) {
            return Ok(order);
        }
        if orders.len() >= MAX_ACTIVE_JOURNALS {
            return Err(miette!("child projection admission exhausted"));
        }
        let order = Arc::new(Mutex::new(()));
        orders.insert(session.to_owned(), Arc::downgrade(&order));
        Ok(order)
    }

    pub(crate) fn contains_session(&self, session: &str) -> Result<bool> {
        self.root
            .contains_session(session)
            .map_err(|error| miette!("session journal is unavailable: {error}"))
    }

    pub(crate) fn register(
        self: &Arc<Self>,
        session: &str,
        view: JournalReadView,
    ) -> Result<JournalRegistration> {
        self.root
            .validate_view(session, &view)
            .map_err(|error| miette!("journal ownership does not match its root: {error}"))?;
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        active.retain(|_, owner| owner.strong_count() > 0);
        if active.contains_key(session) {
            return Err(miette!("journal already has an active read owner"));
        }
        if active.len() >= MAX_ACTIVE_JOURNALS {
            return Err(miette!("active journal admission is exhausted"));
        }
        let publisher = Arc::new(JournalPublication {
            committed: RwLock::new(view),
        });
        active.insert(session.to_owned(), Arc::downgrade(&publisher));
        Ok(JournalRegistration {
            reads: Arc::clone(self),
            session: session.to_owned(),
            publisher,
        })
    }

    pub(crate) fn retain_history(
        &self,
    ) -> Result<retained::HistoryRetention, rw_core::AgentLoopError> {
        self.retained_history.admit()
    }

    pub(crate) fn admit_read(self: &Arc<Self>) -> Result<JournalReadAdmission> {
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| miette!("journal read admission is exhausted"))?;
        Ok(JournalReadAdmission {
            service: Arc::clone(self),
            permit,
        })
    }

    pub(crate) fn capture(&self, session: &str) -> Result<JournalReadLease> {
        let permit = Arc::clone(&self.admission)
            .try_acquire_owned()
            .map_err(|_| miette!("journal read admission is exhausted"))?;
        self.capture_admitted(session, permit)
    }

    /// Move one exclusively held read credit to the next source in a serial query.
    pub(crate) fn retarget(
        &self,
        source: JournalReadLease,
        session: &str,
    ) -> Result<JournalReadLease> {
        let JournalReadLease {
            view,
            _permit: credit,
        } = source;
        drop(view);
        let permit = Arc::try_unwrap(credit)
            .map_err(|_| miette!("serial source query retained a previous read view"))?;
        self.capture_admitted(session, permit)
    }

    fn capture_admitted(
        &self,
        session: &str,
        permit: OwnedSemaphorePermit,
    ) -> Result<JournalReadLease> {
        let owner = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(session)
            .and_then(Weak::upgrade);
        let view = if let Some(owner) = owner {
            owner.capture()
        } else {
            // An opening or closing owner still holds writer.lock. Offline capture
            // rejects that race rather than interpreting unsynchronized bytes.
            self.root
                .read_view(session)
                .map_err(|error| miette!("offline journal prefix is unavailable: {error}"))?
                .ok_or_else(|| miette!("session journal does not exist"))?
        };
        Ok(JournalReadLease {
            view,
            _permit: Arc::new(permit),
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;
    use rw_store::session::{SessionEventLog, SessionEventPageLimits};

    #[test]
    fn active_offline_admission_and_duplicate_owner_are_explicit() {
        let root = tempfile::tempdir().expect("root");
        let reads = JournalService::new(root.path()).expect("service");
        let owner = Arc::new(Mutex::new(
            SessionEventLog::open(root.path(), "session").expect("writer"),
        ));
        assert!(
            reads.capture("session").is_err(),
            "unregistered active writer cannot become offline"
        );
        let registration = reads
            .register("session", owner.lock().expect("owner").read_view())
            .expect("registration");
        assert!(
            reads
                .register("session", owner.lock().expect("owner").read_view())
                .is_err()
        );
        owner
            .lock()
            .expect("owner")
            .append(&serde_json::json!({"value": 1}))
            .expect("append");
        registration
            .publisher
            .publish(owner.lock().expect("owner").read_view());
        let leases: Vec<_> = (0..MAX_READ_VIEWS)
            .map(|_| reads.capture("session").expect("view"))
            .collect();
        assert!(reads.capture("session").is_err());
        owner
            .lock()
            .expect("owner")
            .append(&serde_json::json!({"value": 2}))
            .expect("append");
        registration
            .publisher
            .publish(owner.lock().expect("owner").read_view());
        assert_eq!(
            leases[0]
                .view
                .page::<serde_json::Value>(None, SessionEventPageLimits::default())
                .expect("page")
                .events
                .len(),
            1
        );
        drop(leases);
        drop(registration);
        assert!(
            reads.capture("session").is_err(),
            "closing writer still excludes offline capture"
        );
        drop(owner);
        assert_eq!(
            reads
                .capture("session")
                .expect("offline")
                .view
                .last_sequence(),
            Some(rw_types::SequenceId(1))
        );
    }

    #[test]
    fn pinned_root_survives_rename_and_rejects_foreign_registration() {
        let parent = tempfile::tempdir().expect("parent");
        let root = parent.path().join("root");
        std::fs::create_dir(&root).expect("root");
        let reads = JournalService::new(&root).expect("service");
        let owner = Arc::new(Mutex::new(
            SessionEventLog::open(&root, "session").expect("writer"),
        ));
        let registration = reads
            .register("session", owner.lock().expect("owner").read_view())
            .expect("registration");
        std::fs::rename(&root, parent.path().join("moved")).expect("rename");
        std::fs::create_dir(&root).expect("replacement");
        assert!(reads.capture("session").is_ok());
        drop(registration);
        drop(owner);
        assert!(
            reads.capture("session").is_ok(),
            "offline path stays beneath original descriptor"
        );
        let foreign = Arc::new(Mutex::new(
            SessionEventLog::open(&root, "session").expect("foreign"),
        ));
        assert!(
            reads
                .register("session", foreign.lock().expect("foreign").read_view())
                .is_err()
        );
    }
    #[test]
    fn captures_published_prefix_while_writer_mutex_is_held() {
        let root = tempfile::tempdir().expect("root");
        let reads = JournalService::new(root.path()).expect("service");
        let writer = Mutex::new(SessionEventLog::open(root.path(), "session").expect("writer"));
        let mut guard = writer.lock().expect("writer mutex");
        let registration = reads
            .register("session", guard.read_view())
            .expect("registration");
        guard
            .append(serde_json::json!({"value": 1}))
            .expect("append");
        let previous = reads
            .capture("session")
            .expect("previous committed publication");
        assert_eq!(previous.view.last_sequence(), None);
        registration.publisher.publish(guard.read_view());
        assert_eq!(
            reads
                .capture("session")
                .expect("published")
                .view
                .last_sequence(),
            Some(rw_types::SequenceId(0))
        );
        assert_eq!(previous.view.last_sequence(), None);
        drop(guard);
    }
}
