#![allow(clippy::expect_used)]
use super::{RuntimeSessionResources, settle_both};
use rw_core::SessionResources;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;

struct Resource(Arc<AtomicBool>);
impl Drop for Resource {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn deadline_retains_and_polls_resources_transferred_out_of_the_registry() {
    let dropped = Arc::new(AtomicBool::new(false));
    let registry = Arc::new(Mutex::new(Some(Resource(dropped.clone()))));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(Notify::new());
    let work = {
        let registry = registry.clone();
        let entered = entered.clone();
        let release = release.clone();
        let completed = completed.clone();
        async move {
            let actual_resource = registry.lock().expect("registry").take().expect("resource");
            entered.notify_one();
            release.notified().await;
            drop(actual_resource);
            completed.notify_one();
            Ok(())
        }
    };
    let resources =
        RuntimeSessionResources::start(registry.clone(), work, Duration::from_millis(20), None);
    resources.request();
    entered.notified().await;
    assert!(registry.lock().expect("registry").is_none());
    assert!(resources.shutdown().await.is_err());
    assert!(
        !dropped.load(Ordering::Acquire),
        "deadline must not drop transferred resources"
    );
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .expect("same cleanup future continued after deadline");
    assert!(dropped.load(Ordering::Acquire));
    assert!(
        resources.shutdown().await.is_err(),
        "expired proof remains sticky"
    );
}

#[tokio::test]
async fn one_service_panic_does_not_skip_its_sibling_cleanup() {
    let attempted = Arc::new(AtomicBool::new(false));
    let result = settle_both(async { panic!("injected MCP cleanup panic") }, {
        let attempted = attempted.clone();
        async move {
            attempted.store(true, Ordering::Release);
            Ok(())
        }
    })
    .await;
    assert!(result.is_err());
    assert!(attempted.load(Ordering::Acquire));
}

#[tokio::test]
async fn dropping_the_resource_handle_starts_owned_cleanup() {
    let completed = Arc::new(Notify::new());
    let resources = RuntimeSessionResources::start(
        (),
        {
            let completed = completed.clone();
            async move {
                completed.notify_one();
                Ok(())
            }
        },
        Duration::from_secs(1),
        None,
    );
    drop(resources);
    tokio::time::timeout(Duration::from_secs(1), completed.notified())
        .await
        .expect("drop requested cleanup");
}
