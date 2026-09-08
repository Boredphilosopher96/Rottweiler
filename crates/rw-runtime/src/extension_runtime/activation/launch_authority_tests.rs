//! Real admitted worker custody across activation-owner destruction.
use super::*;
use std::sync::atomic::AtomicBool;

fn starting_owner(fixture: &Fixture) -> OperationOwner {
    let generation = &fixture.endpoint.generation;
    generation.resources.lock().expect("resources").lease =
        Some(fixture.budget.admit().expect("activation admission"));
    *generation.phase.lock().expect("phase") = Phase::Starting {
        deadline: Instant::now() + ACTIVATION_DEADLINE,
    };
    OperationOwner {
        generation: Arc::clone(generation),
        armed: true,
    }
}

fn available_starts(budget: &PluginRuntimeBudget) -> usize {
    // The application contract has 32 starting slots; this bounded probe holds
    // every available slot simultaneously, then returns all probe leases.
    let leases: Vec<_> = (0..32).filter_map(|_| budget.admit().ok()).collect();
    leases.len()
}

struct WorkerResult(Arc<AtomicBool>);
impl Drop for WorkerResult {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[tokio::test]
async fn admitted_launch_worker_survives_owner_and_waiter_drop_without_refunding() {
    let fixture = Fixture::new();
    let owner = starting_owner(&fixture);
    let retired = Arc::new(AtomicBool::new(false));
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, finish) = std::sync::mpsc::sync_channel(1);
    let worker = tokio::spawn({
        let generation = Arc::clone(&fixture.endpoint.generation);
        let retired = Arc::clone(&retired);
        async move {
            recipe::with_launch_authority(&generation, move |recipe| {
                std::fs::write(recipe.private_root.join("authority-effect"), b"owned")
                    .expect("physical effect");
                let _ = entered.send(());
                finish
                    .recv_timeout(Duration::from_secs(5))
                    .expect("worker release");
                Ok(WorkerResult(retired))
            })
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), ready)
        .await
        .expect("admitted worker")
        .expect("entered");
    assert!(
        fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .effects_started
    );
    drop(owner);
    assert!(matches!(
        fixture.endpoint.generation.snapshot(),
        Phase::Closed { proof: Err(_), .. }
    ));
    assert_eq!(available_starts(&fixture.budget), 31);
    worker.abort();
    let Err(error) = worker.await else {
        panic!("waiter was not dropped");
    };
    assert!(error.is_cancelled());
    assert!(!retired.load(Ordering::Acquire));
    assert_eq!(available_starts(&fixture.budget), 31);
    release.send(()).expect("finish physical work");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !retired.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("discarded worker result physically retires");
    assert_eq!(
        available_starts(&fixture.budget),
        31,
        "lost operation proof stays quarantined"
    );
}

#[tokio::test]
async fn unpolled_launch_worker_cannot_start_after_owner_reported_settlement() {
    let fixture = Fixture::new();
    let owner = starting_owner(&fixture);
    let work = recipe::with_launch_authority(&fixture.endpoint.generation, |recipe| {
        std::fs::write(recipe.private_root.join("forbidden-effect"), b"late")
            .expect("effect would escape settled owner");
        Ok(())
    });
    assert!(
        !fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .effects_started
    );
    drop(owner);
    assert!(matches!(
        fixture.endpoint.generation.snapshot(),
        Phase::Closed { proof: Ok(()), .. }
    ));
    assert_eq!(available_starts(&fixture.budget), 32);
    let error = work.await.expect_err("cancelled before physical work");
    assert_eq!(error.code, "cancelled");
    assert!(!fixture.root.path().join("forbidden-effect").exists());
    assert!(
        !fixture
            .endpoint
            .generation
            .resources
            .lock()
            .expect("resources")
            .effects_started
    );
    assert_eq!(available_starts(&fixture.budget), 32);
}
