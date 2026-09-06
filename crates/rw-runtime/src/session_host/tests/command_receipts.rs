use super::*;
use rw_core::{ClientCommand, ClientId, CommandMeta, CommandOutcome, RequestId};
use rw_types::command_receipt::{CommandReceipt, ReceiptAdmission};

#[tokio::test]
async fn mutation_receipt_reopen_reauthorizes_workspace_before_replay() {
    let root = tempdir().expect("root");
    let workspace = private_test_directory(&root.path().join("workspace"));
    let other = private_test_directory(&root.path().join("other"));
    let first = factory(root.path(), &workspace).await;
    let command = ClientCommand::CreateSession {
        meta: CommandMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            client_id: ClientId("first".into()),
            request_id: RequestId("durable-authorized".into()),
        },
        cwd: workspace.display().to_string(),
        model: None,
    };
    let fingerprint = "a".repeat(64);
    assert!(matches!(
        first
            .admit_command_receipt(&command, &fingerprint)
            .await
            .expect("admit"),
        ReceiptAdmission::Admitted
    ));
    first
        .complete_command_receipt(
            &command.meta().request_id,
            &fingerprint,
            CommandReceipt {
                outcome: CommandOutcome::Accepted {},
                events: Vec::new(),
            },
        )
        .await
        .expect("complete");
    first.shutdown().await.expect("first factory closed");
    let unauthorized = factory(root.path(), &other).await;
    assert!(
        unauthorized
            .admit_command_receipt(&command, &fingerprint)
            .await
            .is_err()
    );
    unauthorized
        .shutdown()
        .await
        .expect("restricted factory closed");
    let restarted = factory(root.path(), &workspace).await;
    assert!(matches!(
        restarted
            .admit_command_receipt(&command, &fingerprint)
            .await
            .expect("authorized replay"),
        ReceiptAdmission::Completed(CommandReceipt {
            outcome: CommandOutcome::Accepted {},
            ..
        })
    ));
    assert!(
        restarted
            .admit_command_receipt(&command, &"b".repeat(64))
            .await
            .is_err()
    );
    restarted.shutdown().await.expect("factory closed");
}
