//! Serialized blocking receipt I/O under the accepted host operation's lifetime.
use super::{HostError, RuntimeSessionFactory, load_session_metadata_any};
use rw_core::{ClientCommand, RequestId};
use rw_store::command_receipts::{CommandReceipts, ReceiptError};
use rw_types::command_receipt::{CommandReceipt, ReceiptAdmission};

impl RuntimeSessionFactory {
    pub(super) async fn admit_receipt(
        &self,
        command: &ClientCommand,
        fingerprint: &str,
    ) -> Result<ReceiptAdmission, HostError> {
        let workspace = match command {
            ClientCommand::CreateSession { cwd, .. } => Some(cwd.clone()),
            _ => None,
        };
        let session = command.session_id().cloned();
        let operation = command.meta().request_id.clone();
        let fingerprint = fingerprint.to_owned();
        let factory = self.clone();
        let io = self.receipt_io.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _io = io;
            if let Some(workspace) = workspace {
                factory.authorize_workspace(&workspace)?;
            } else if let Some(session) = session {
                let metadata = load_session_metadata_any(&factory.options.storage_root, &session.0)
                    .map_err(|_| {
                        HostError::Persistence("receipt session metadata is unavailable".into())
                    })?;
                factory.authorize_workspace_path(&metadata.workspace)?;
            } else {
                return Err(HostError::Protocol(
                    "mutation receipt has no workspace authority".into(),
                ));
            }
            let mut store = CommandReceipts::open(
                &factory.options.storage_root.join("command-receipts.sqlite"),
            )
            .map_err(|error| receipt_error(&error))?;
            store
                .admit(&operation, &fingerprint)
                .map_err(|error| receipt_error(&error))
        })
        .await
        .map_err(|_| HostError::Persistence("command admission worker failed".into()))?
    }

    pub(super) async fn finish_receipt(
        &self,
        operation: &RequestId,
        fingerprint: &str,
        receipt: CommandReceipt,
    ) -> Result<CommandReceipt, HostError> {
        let operation = operation.clone();
        let fingerprint = fingerprint.to_owned();
        let root = self.options.storage_root.clone();
        let io = self.receipt_io.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || {
            let _io = io;
            let mut store = CommandReceipts::open(&root.join("command-receipts.sqlite"))
                .map_err(|error| receipt_error(&error))?;
            store
                .complete(&operation, &fingerprint, &receipt)
                .map_err(|error| receipt_error(&error))?;
            Ok(receipt)
        })
        .await
        .map_err(|_| HostError::Persistence("command completion worker failed".into()))?
    }
}
fn receipt_error(error: &ReceiptError) -> HostError {
    match error {
        ReceiptError::Conflict => HostError::Protocol(
            "operation identity was reused for another command or outcome".into(),
        ),
        _ => HostError::Persistence("durable command receipt is unavailable".into()),
    }
}
