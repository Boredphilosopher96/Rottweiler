#![allow(clippy::expect_used)]
use super::*;

fn admitted(pool: &Arc<Semaphore>) -> OutputValidation {
    let contract = OutputContract::JsonSchema {
        name: "test".into(),
        schema: OutputSchema::Object { fields: vec![] },
    };
    contract.validate().expect("schema");
    let OutputContract::JsonSchema { schema, .. } = &contract else {
        panic!("fixture")
    };
    OutputValidation::admit(&contract, schema, pool).expect("working claim")
}
#[test]
fn aggregate_working_owner_bounds_live_streams_and_refunds_unpolled_drop() {
    let pool = Arc::new(Semaphore::new(POOL_BYTES));
    let mut streams = Vec::new();
    for _ in 0..POOL_BYTES / WORK_BYTES as usize {
        streams.push(admitted(&pool).attach(BoxEventStream::new(futures_util::stream::pending())));
    }
    assert_eq!(pool.available_permits(), 0);
    let contract = OutputContract::JsonSchema {
        name: "test".into(),
        schema: OutputSchema::Object { fields: vec![] },
    };
    let OutputContract::JsonSchema { schema, .. } = &contract else {
        panic!("fixture")
    };
    assert!(OutputValidation::admit(&contract, schema, &pool).is_err());
    streams.pop();
    assert_eq!(pool.available_permits(), WORK_BYTES as usize);
    drop(streams);
    assert_eq!(pool.available_permits(), POOL_BYTES);
}
#[tokio::test]
async fn abandoned_cpu_waiter_cannot_refund_schema_or_text_before_worker_retires() {
    let pool = Arc::new(Semaphore::new(WORK_BYTES as usize));
    let mut work = admitted(&pool).work.expect("structured work");
    work.text.push_str("{}");
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let (retired, retired_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        rw_resources::run_blocking(rw_resources::ResourceClass::Cpu, move || {
            entered.send(()).expect("entered");
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("release");
            let result = work.validate();
            drop(result);
            let _ = retired.send(());
        })
        .await
    });
    entered_rx.await.expect("physical worker");
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(
        pool.available_permits(),
        0,
        "worker still owns source and allowance"
    );
    release.send(()).expect("release worker");
    retired_rx.await.expect("body retired");
    assert_eq!(pool.available_permits(), WORK_BYTES as usize);
}
