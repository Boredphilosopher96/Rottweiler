//! Retain actual invocation futures independently of their callers.
use crate::McpError;
use rw_types::McpServerId;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, watch};

pub(super) const MAX_OWNED_OPERATIONS: usize = 64;

#[derive(Default)]
pub(super) struct Operations {
    state: Mutex<State>,
    changed: Notify,
}

#[derive(Default)]
struct State {
    closed: bool,
    next: u64,
    entries: BTreeMap<u64, Entry>,
}

struct Entry {
    server: McpServerId,
    cancelled: watch::Sender<bool>,
    failed: bool,
}

pub(super) struct Owner {
    operations: Arc<Operations>,
    id: u64,
    armed: bool,
}

pub(super) struct Caller {
    operations: Arc<Operations>,
    cancelled: watch::Sender<bool>,
    armed: bool,
}

impl Operations {
    pub(super) fn ensure_open(&self) -> Result<(), McpError> {
        if self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
        {
            return Err(McpError::Policy("MCP manager is shut down".to_owned()));
        }
        Ok(())
    }
    pub(super) fn ensure_idle(&self, server: &McpServerId) -> Result<(), McpError> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.entries.values().any(|entry| &entry.server == server) {
            return Err(unsettled(server));
        }
        Ok(())
    }

    pub(super) fn cancel_server(&self, server: &McpServerId) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for entry in state
            .entries
            .values()
            .filter(|entry| &entry.server == server)
        {
            entry.cancelled.send_replace(true);
        }
        self.changed.notify_waiters();
    }
    pub(super) async fn drain_server(&self, server: &McpServerId) -> Result<(), McpError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state
                    .entries
                    .values()
                    .any(|entry| &entry.server == server && entry.failed)
                {
                    return Err(unsettled(server));
                }
                if !state.entries.values().any(|entry| &entry.server == server) {
                    return Ok(());
                }
            }
            changed.await;
        }
    }
    pub(super) fn stop(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        for entry in state.entries.values() {
            entry.cancelled.send_replace(true);
        }
        self.changed.notify_waiters();
    }

    pub(super) fn admit(
        self: &Arc<Self>,
        server: &McpServerId,
    ) -> Result<(Owner, Caller, watch::Receiver<bool>), McpError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return Err(McpError::Policy("MCP manager is shut down".to_owned()));
        }
        if state.entries.len() >= MAX_OWNED_OPERATIONS {
            return Err(McpError::Policy(
                "MCP operation capacity exhausted".to_owned(),
            ));
        }
        if state
            .entries
            .values()
            .any(|entry| &entry.server == server && (entry.failed || *entry.cancelled.borrow()))
        {
            return Err(unsettled(server));
        }
        let id = state.next;
        state.next = id
            .checked_add(1)
            .ok_or_else(|| McpError::Policy("MCP operation identity exhausted".to_owned()))?;
        let (cancelled, cancellation) = watch::channel(false);
        state.entries.insert(
            id,
            Entry {
                server: server.clone(),
                cancelled: cancelled.clone(),
                failed: false,
            },
        );
        Ok((
            Owner {
                operations: Arc::clone(self),
                id,
                armed: true,
            },
            Caller {
                operations: Arc::clone(self),
                cancelled,
                armed: true,
            },
            cancellation,
        ))
    }

    /// Only abandoned invocations require a barrier; active callers retain their own effects.
    pub(super) async fn settle(&self, timeout: Duration) -> Result<(), McpError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let pending = {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(entry) = state.entries.values().find(|entry| entry.failed) {
                    return Err(unsettled(&entry.server));
                }
                state
                    .entries
                    .values()
                    .find(|entry| *entry.cancelled.borrow())
                    .map(|entry| entry.server.clone())
            };
            let Some(server) = pending else {
                return Ok(());
            };
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return Err(unsettled(&server));
            }
        }
    }
}

impl Owner {
    pub(super) fn cancel(&self) {
        let state = self
            .operations
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.entries.get(&self.id) {
            entry.cancelled.send_replace(true);
        }
        self.operations.changed.notify_waiters();
    }
    pub(super) fn finish(mut self, proven: bool) {
        let mut state = self
            .operations
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if proven {
            state.entries.remove(&self.id);
        } else if let Some(entry) = state.entries.get_mut(&self.id) {
            entry.failed = true;
        }
        self.armed = false;
        self.operations.changed.notify_waiters();
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .operations
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = state.entries.get_mut(&self.id) {
            entry.failed = true;
        }
        self.operations.changed.notify_waiters();
    }
}
impl Caller {
    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}
impl Drop for Caller {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.send_replace(true);
            self.operations.changed.notify_waiters();
        }
    }
}

pub(super) fn unsettled(server: &McpServerId) -> McpError {
    McpError::EffectsUnsettled {
        server: server.clone(),
        message: "MCP invocation ownership has not settled".to_owned(),
    }
}
