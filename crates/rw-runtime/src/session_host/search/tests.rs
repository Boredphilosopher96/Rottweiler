use super::*;
#[test]
fn dropped_search_waiter_cancels_the_shared_physical_query_control() {
    let waiter = SearchWaiter(SessionIndexReadControl::new());
    let worker = waiter.0.clone();
    drop(waiter);
    assert!(matches!(
        worker.check(),
        Err(SessionStoreError::IndexReadInterrupted)
    ));
}
