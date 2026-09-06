//! Conditional discovery waits use a small lane, before ordinary read bytes/workers.
use crate::{engine::control_observation, host::HostError};
use rw_types::{
    ClientCommand, ClientId,
    family_controls::{MAX_CLIENT_FAMILY_CONTROL_WAITS, MAX_FAMILY_CONTROL_WAITS},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::Semaphore;

#[derive(Debug)]
pub(super) struct FamilyWaitAdmission {
    global: Arc<Semaphore>,
    clients: Mutex<HashMap<ClientId, Weak<Semaphore>>>,
}
impl Default for FamilyWaitAdmission {
    fn default() -> Self {
        Self {
            global: Arc::new(Semaphore::new(MAX_FAMILY_CONTROL_WAITS)),
            clients: Mutex::new(HashMap::new()),
        }
    }
}
impl FamilyWaitAdmission {
    pub(super) async fn wait(
        &self,
        client: &ClientId,
        command: &ClientCommand,
    ) -> Result<(), HostError> {
        let ClientCommand::ReadFamilyControls {
            after_revision: Some(after),
            ..
        } = command
        else {
            return Ok(());
        };
        let _lease = self.acquire(client)?;
        control_observation::wait(Some(*after)).await;
        Ok(())
    }
    fn acquire(&self, client: &ClientId) -> Result<WaitLease, HostError> {
        let global = Arc::clone(&self.global)
            .try_acquire_owned()
            .map_err(|_| busy())?;
        let owner = {
            let mut clients = self
                .clients
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            clients.retain(|_, owner| owner.strong_count() > 0);
            if let Some(owner) = clients.get(client).and_then(Weak::upgrade) {
                owner
            } else {
                let owner = Arc::new(Semaphore::new(MAX_CLIENT_FAMILY_CONTROL_WAITS));
                clients.insert(client.clone(), Arc::downgrade(&owner));
                owner
            }
        };
        let client = owner.try_acquire_owned().map_err(|_| busy())?;
        Ok(WaitLease {
            _global: global,
            _client: client,
        })
    }
}
fn busy() -> HostError {
    HostError::Query("family control wait admission exhausted".into())
}

struct WaitLease {
    _global: tokio::sync::OwnedSemaphorePermit,
    _client: tokio::sync::OwnedSemaphorePermit,
}
#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use rw_types::ClientId;
    #[test]
    fn idle_family_waits_preserve_normal_read_capacity_and_release_on_drop() {
        let reads = super::super::ReadAdmission::default();
        let client = ClientId("waiting-client".into());
        let waiting = reads.family_waits.acquire(&client).expect("wait slot");
        assert!(
            reads.family_waits.acquire(&client).is_err(),
            "one conditional wait per client"
        );
        let ordinary = reads
            .acquire(&client)
            .expect("waiting does not hold ordinary read bytes or work");
        drop(waiting);
        assert!(
            reads.family_waits.acquire(&client).is_ok(),
            "abandoned wait returns its admission"
        );
        drop(ordinary);
    }
}
