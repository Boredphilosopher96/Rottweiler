use super::*;

fn client(id: &str) -> BoundClient {
    BoundClient {
        client_id: ClientId(id.into()),
    }
}
fn create(request: &str) -> ClientCommand {
    ClientCommand::CreateSession {
        meta: meta("untrusted", request),
        cwd: "workspace".into(),
        model: None,
    }
}
fn host(factory: Arc<StubFactory>) -> EngineHost {
    EngineHost::new(
        EngineHostConfig {
            max_sessions: 2,
            max_deduplicated_requests: 1,
        },
        factory,
        Arc::new(StubQueries::default()),
    )
    .expect("host")
}

#[tokio::test]
async fn durable_mutation_receipt_survives_cache_eviction_and_host_rebinding() {
    let factory = Arc::new(StubFactory::new());
    let first = host(factory.clone());
    assert_eq!(
        first
            .dispatch(client("first"), create("stable-create"))
            .await
            .outcome,
        CommandOutcome::Accepted {}
    );
    first
        .dispatch(
            client("first"),
            ClientCommand::ListModels {
                meta: meta("untrusted", "evict"),
                session_id: None,
                refresh: false,
            },
        )
        .await;
    assert!(
        !first
            .dedupe
            .lock()
            .expect("ledger")
            .entries
            .contains_key(&(ClientId("first".into()), RequestId("stable-create".into())))
    );
    // Both cache eviction and a new authenticated connection must reach the
    // durable receipt rather than allocate another session.
    assert_eq!(
        first
            .dispatch(client("reconnected"), create("stable-create"))
            .await
            .outcome,
        CommandOutcome::Accepted {}
    );
    assert_eq!(factory.next.load(Ordering::Acquire), 2);
    first.shutdown_sessions().await.expect("first host closed");
    let restarted = host(factory.clone());
    let mut events = restarted
        .subscribe(client("restarted"), None, None)
        .await
        .expect("events");
    assert_eq!(
        restarted
            .dispatch(client("restarted"), create("stable-create"))
            .await
            .outcome,
        CommandOutcome::Accepted {}
    );
    let mut ack = tokio::time::timeout(Duration::from_secs(2), events.recv())
        .await
        .expect("delivered")
        .expect("event")
        .expect("ack");
    assert_eq!(
        ack.command_meta_mut()
            .expect("connection receipt")
            .client_id,
        ClientId("restarted".into())
    );
    assert_eq!(
        factory.next.load(Ordering::Acquire),
        2,
        "one session allocation across hosts"
    );
    restarted
        .shutdown_sessions()
        .await
        .expect("restarted host closed");
}

#[tokio::test]
async fn indeterminate_mutation_cannot_repeat_effects_or_change_its_identity() {
    let factory = Arc::new(StubFactory::new());
    let mut command = create("interrupted");
    command.meta_mut().client_id = ClientId(String::new());
    let fingerprint = super::super::read::command_hash(&command).expect("fingerprint");
    assert!(matches!(
        factory
            .admit_command_receipt(&command, &fingerprint)
            .await
            .expect("durable intent"),
        rw_types::command_receipt::ReceiptAdmission::Admitted
    ));
    let restarted = host(factory.clone());
    let outcome = restarted
        .dispatch(client("restarted"), command.clone())
        .await
        .outcome;
    assert!(
        matches!(outcome, CommandOutcome::Rejected { error } if error.code == "operation_indeterminate")
    );
    if let ClientCommand::CreateSession { cwd, .. } = &mut command {
        *cwd = "different".into();
    }
    assert!(matches!(
        restarted
            .dispatch(client("different-client"), command)
            .await
            .outcome,
        CommandOutcome::Rejected { .. }
    ));
    assert_eq!(
        factory.next.load(Ordering::Acquire),
        1,
        "no mutation after indeterminate admission"
    );
    restarted.shutdown_sessions().await.expect("closed");
}
