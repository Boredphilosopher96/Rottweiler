//! Session-local publication waits precede shared journal and worker admission.
use miette::{Result, miette};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::MAX_PROJECTION_WAITERS;

pub(super) type ProjectionOrders = Mutex<HashMap<String, Weak<ProjectionOrder>>>;

pub(crate) struct ProjectionOrder {
    writer: Arc<Semaphore>,
    waiters: Arc<Semaphore>,
}

pub(crate) struct ProjectionPermit {
    // The registry must retain the same order identity until physical work settles.
    _writer: OwnedSemaphorePermit,
    _owner: Arc<ProjectionOrder>,
}

impl ProjectionOrder {
    pub(crate) async fn acquire(self: Arc<Self>) -> Result<ProjectionPermit> {
        let waiting = Arc::clone(&self.waiters)
            .try_acquire_owned()
            .map_err(|_| miette!("session projection wait admission is exhausted"))?;
        let writer = Arc::clone(&self.writer)
            .acquire_owned()
            .await
            .map_err(|_| miette!("session projection owner is closed"))?;
        drop(waiting);
        Ok(ProjectionPermit {
            _owner: self,
            _writer: writer,
        })
    }
}

pub(super) fn projection_order(
    registry: &ProjectionOrders,
    session: &str,
    projection: &str,
) -> Result<Arc<ProjectionOrder>> {
    rw_types::SessionId::validate(session)
        .map_err(|error| miette!("{projection} projection identity: {error}"))?;
    let mut orders = registry
        .lock()
        .map_err(|_| miette!("{projection} projection registry is poisoned"))?;
    orders.retain(|_, order| order.strong_count() > 0);
    if let Some(order) = orders.get(session).and_then(Weak::upgrade) {
        return Ok(order);
    }
    if orders.len() >= super::MAX_ACTIVE_JOURNALS {
        return Err(miette!("{projection} projection admission exhausted"));
    }
    let order = Arc::new(ProjectionOrder {
        writer: Arc::new(Semaphore::new(1)),
        waiters: Arc::new(Semaphore::new(MAX_PROJECTION_WAITERS)),
    });
    orders.insert(session.to_owned(), Arc::downgrade(&order));
    Ok(order)
}
