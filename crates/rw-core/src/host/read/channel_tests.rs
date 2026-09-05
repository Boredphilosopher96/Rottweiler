#![allow(clippy::expect_used)]
use super::*;
use rw_types::{CommandMeta, SessionId};

fn bound() -> BoundClient {
    BoundClient {
        client_id: ClientId("authenticated".into()),
    }
}
fn request(id: &str, session: &str) -> ClientCommand {
    ClientCommand::ListCommands {
        meta: CommandMeta {
            protocol_version: rw_types::PROTOCOL_VERSION,
            client_id: ClientId("untrusted-wire-value".into()),
            request_id: RequestId(id.into()),
        },
        session_id: SessionId(session.into()),
    }
}
async fn accepted(command: ClientCommand) -> Result<(CommandOutcome, Vec<EngineEvent>), HostError> {
    assert_eq!(command.meta().client_id, bound().client_id);
    Ok((CommandOutcome::Accepted, Vec::new()))
}
fn rejected_with(reply: &HostReply, code: &str) -> bool {
    matches!(&reply.outcome, CommandOutcome::Rejected { error } if error.code == code)
}

#[tokio::test]
async fn standalone_channel_has_no_mutation_capability() {
    let channel = HostReadChannel::new(2).expect("channel");
    let command = ClientCommand::CreateSession {
        meta: request("control", "session").meta().clone(),
        cwd: "/workspace".into(),
        model: None,
    };
    let reply = channel
        .dispatch(bound(), command, |_| async {
            panic!("read channel invoked a mutation backend")
        })
        .await;
    assert!(rejected_with(&reply, "read_only"));
    assert!(matches!(
        serde_json::from_slice::<CommandReply>(&reply.bytes).expect("reply"),
        CommandReply::Command { .. }
    ));
}

#[tokio::test]
async fn retained_body_clones_keep_identity_and_client_admission() {
    let channel = HostReadChannel::new(1).expect("channel");
    let first = channel
        .dispatch(bound(), request("one", "session"), accepted)
        .await;
    let retained = first.bytes.clone();
    drop(first);
    let second = channel
        .dispatch(bound(), request("two", "session"), accepted)
        .await;
    let busy = channel
        .dispatch(bound(), request("three", "session"), accepted)
        .await;
    assert!(rejected_with(&busy, "read_busy"));
    drop(second);
    // Churn beyond the one-entry retention cap cannot evict a response's identity.
    for index in 0..8 {
        drop(
            channel
                .dispatch(
                    bound(),
                    request(&format!("churn-{index}"), "session"),
                    accepted,
                )
                .await,
        );
    }
    let conflict = channel
        .dispatch(bound(), request("one", "foreign"), accepted)
        .await;
    assert!(rejected_with(&conflict, "request_id_conflict"));
    drop(conflict);
    drop(retained);
    let admitted = channel
        .dispatch(bound(), request("three", "session"), accepted)
        .await;
    assert_eq!(admitted.outcome, CommandOutcome::Accepted);
}

#[tokio::test]
async fn cancelled_backend_releases_all_read_admission() {
    let channel = HostReadChannel::new(2).expect("channel");
    let (started, receiver) = tokio::sync::oneshot::channel();
    let worker = channel.clone();
    let task = tokio::spawn(async move {
        worker
            .dispatch(bound(), request("pending", "session"), |_| async move {
                started.send(()).expect("start signal");
                std::future::pending().await
            })
            .await
    });
    receiver.await.expect("backend started");
    assert_eq!(
        channel.admission.global.available_permits(),
        MAX_ACTIVE_READS - 1
    );
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    assert_eq!(
        channel.admission.global.available_permits(),
        MAX_ACTIVE_READS
    );
    assert_eq!(
        channel.admission.bytes.available_permits(),
        MAX_RETAINED_REPLY_UNITS
    );
    let reply = channel
        .dispatch(bound(), request("pending", "session"), accepted)
        .await;
    assert_eq!(reply.outcome, CommandOutcome::Accepted);
}
