//! A lifecycle transition cannot discard its actual client before handoff or proof.
use crate::{McpClient, McpError};
use std::{sync::Arc, time::Duration};

pub(super) struct ClientProof {
    client: Arc<dyn McpClient>,
    transferred_or_settled: bool,
}
impl ClientProof {
    pub(super) fn new(client: Arc<dyn McpClient>) -> Self {
        Self {
            client,
            transferred_or_settled: false,
        }
    }
    pub(super) fn client(&self) -> &dyn McpClient {
        self.client.as_ref()
    }
    pub(super) fn share(&self) -> Arc<dyn McpClient> {
        Arc::clone(&self.client)
    }
    pub(super) fn handed_off(mut self) {
        self.transferred_or_settled = true;
    }
    pub(super) async fn close(mut self, timeout: Duration) -> Result<(), McpError> {
        self.client.close(timeout).await?;
        self.transferred_or_settled = true;
        Ok(())
    }
}
impl Drop for ClientProof {
    fn drop(&mut self) {
        if !self.transferred_or_settled {
            // The manager retains a failed transition and refuses reconnection.
            // Keep its actual client authority even when close returned an error,
            // panicked, or an executor dropped the transition before polling it.
            std::mem::forget(Arc::clone(&self.client));
        }
    }
}
