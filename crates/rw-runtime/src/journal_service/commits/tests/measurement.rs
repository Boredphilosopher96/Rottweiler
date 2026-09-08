//! Test-only load observations around the production commit owner and real durable appends.
#![allow(clippy::expect_used)]
use super::{JournalCommits, MAX_BATCHES, MAX_BYTES, MAX_EXECUTING};
use rw_core::{AdmittedEventBatch, EventBatchPlan, SessionHandle};
use rw_store::session::{
    SessionEventPageLimits,
    journal::{JournalAppendPlan, SegmentedJournal},
};
use rw_types::{EngineEvent, EventMeta, SequenceId, SessionId, TurnId};
use std::{
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};
use tokio::task::JoinSet;

const EVENTS: usize = 16;
const DELAY: Duration = Duration::from_millis(25);
const DEADLINE: Duration = Duration::from_secs(10);
#[derive(Clone, Copy)]
struct Timing {
    admitted: Instant,
    executing: Option<Instant>,
    native: Option<Instant>,
    durable: Option<Instant>,
    acknowledged: Option<Instant>,
}
struct Stall(Arc<(Mutex<bool>, Condvar)>);
impl Stall {
    fn release(&self) {
        *self.0.0.lock().expect("stall") = true;
        self.0.1.notify_all();
    }
}
impl Drop for Stall {
    fn drop(&mut self) {
        self.release();
    }
}
#[derive(Default)]
struct Observations {
    peak_items: usize,
    peak_bytes: usize,
    peak_queued: usize,
    oldest_queued_us: u128,
    control_us: Vec<u128>,
}
impl Observations {
    fn sample(&mut self, queue: &JournalCommits, times: &Mutex<Vec<Timing>>) {
        let now = Instant::now();
        let times = times.lock().expect("timings");
        let queued = times.iter().filter(|timing| timing.executing.is_none());
        self.peak_queued = self.peak_queued.max(queued.clone().count());
        for timing in queued {
            self.oldest_queued_us = self
                .oldest_queued_us
                .max(now.duration_since(timing.admitted).as_micros());
        }
        self.peak_items = self
            .peak_items
            .max(MAX_BATCHES - queue.batches.available_permits());
        self.peak_bytes = self
            .peak_bytes
            .max(MAX_BYTES as usize - queue.bytes.available_permits());
        assert!(self.peak_items <= MAX_BATCHES && self.peak_bytes <= MAX_BYTES as usize);
    }
    async fn control(&mut self, actor: &SessionHandle) {
        let started = Instant::now();
        let command = rw_types::ClientCommand::Interrupt {
            meta: rw_types::CommandMeta {
                protocol_version: rw_types::PROTOCOL_VERSION,
                client_id: rw_types::ClientId("local".into()),
                request_id: rw_types::RequestId(format!("pressure-{}", self.control_us.len())),
            },
            session_id: actor.session_id().clone(),
        };
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), actor.dispatch(command))
                .await
                .expect("control must not wait for blocked storage")
                .expect("authenticated independent-session control"),
            rw_types::CommandOutcome::Accepted {}
        ));
        self.control_us.push(started.elapsed().as_micros());
    }
}
fn plan(session: &str) -> EventBatchPlan {
    EventBatchPlan::new(
        (0..EVENTS)
            .map(|index| EngineEvent::TextDelta {
                meta: EventMeta {
                    protocol_version: rw_core::SESSION_EVENT_VERSION,
                    session_id: SessionId(session.into()),
                    sequence_id: SequenceId(index as u64),
                    emitted_at: "2026-09-08T00:00:00Z".into(),
                    caused_by: None,
                },
                turn_id: TurnId("stream".into()),
                text: format!("delta-{index:02}:{}", "x".repeat(128)),
            })
            .collect(),
    )
    .expect("bounded stream batch")
}

struct AppendWork {
    journal: SegmentedJournal,
    batch: Arc<AdmittedEventBatch>,
    times: Arc<Mutex<Vec<Timing>>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    index: usize,
}
impl AppendWork {
    fn run(mut self) -> Arc<AdmittedEventBatch> {
        self.times.lock().expect("times")[self.index].native = Some(Instant::now());
        let ready = self.gate.0.lock().expect("gate");
        let (ready, _) = self
            .gate
            .1
            .wait_timeout_while(ready, DEADLINE, |ready| !*ready)
            .expect("bounded injected storage barrier");
        assert!(*ready, "controller must release the injected stall");
        drop(ready);
        // Deliberately injected service time, not a claim about the physical device.
        std::thread::sleep(DELAY);
        let prepared = JournalAppendPlan::measure(SequenceId(0), self.batch.events())
            .expect("measure exact admitted batch")
            .encode(self.batch.events())
            .expect("encode");
        self.journal
            .append_prepared(prepared)
            .expect("actual durable append");
        self.times.lock().expect("times")[self.index].durable = Some(Instant::now());
        self.batch
    }
}

impl JournalCommits {
    pub(crate) async fn measure_storage_pressure(
        self: &Arc<Self>,
        actor: &SessionHandle,
        round: usize,
    ) {
        let root = Arc::new(tempfile::tempdir().expect("journal fixture"));
        let path = root.path().to_path_buf();
        let journals =
            rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
                (0..MAX_BATCHES)
                    .map(|index| {
                        SegmentedJournal::open(&path, &format!("load-{index}"))
                            .expect("open journal")
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .expect("fixture construction settled");
        let times = Arc::new(Mutex::new(Vec::with_capacity(MAX_BATCHES)));
        let stall = Stall(Arc::new((Mutex::new(false), Condvar::new())));
        let mut tasks = JoinSet::new();
        let started = Instant::now();
        submit(self, journals, &times, &stall, &root, &mut tasks);
        assert!(
            self.reserve(&plan("overflow")).is_err(),
            "the next batch must be rejected"
        );
        await_saturation(self, &times).await;
        let mut observations = Observations::default();
        for _ in 0..16 {
            observations.sample(self, &times);
            observations.control(actor).await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(observations.peak_queued, MAX_BATCHES - MAX_EXECUTING);
        assert!(
            times
                .lock()
                .expect("times")
                .iter()
                .all(|time| time.durable.is_none())
        );
        stall.release();
        while !tasks.is_empty() {
            assert!(started.elapsed() < DEADLINE, "bounded load completion");
            observations.sample(self, &times);
            observations.control(actor).await;
            tokio::select! {
                result = tasks.join_next() => { result.expect("task").expect("commit caller"); }
                () = tokio::time::sleep(Duration::from_millis(2)) => {}
            }
        }
        let elapsed = started.elapsed();
        tokio::time::timeout(DEADLINE, async {
            while self.pending_jobs() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("all commit owners retired");
        assert_eq!(self.batches.available_permits(), MAX_BATCHES);
        assert_eq!(self.bytes.available_permits(), MAX_BYTES as usize);
        let times = times.lock().expect("times").clone();
        verify_reopened(root.path().to_path_buf()).await;
        report(round, elapsed, &times, &observations);
    }
}

fn submit(
    queue: &Arc<JournalCommits>,
    journals: Vec<SegmentedJournal>,
    times: &Arc<Mutex<Vec<Timing>>>,
    stall: &Stall,
    root: &Arc<tempfile::TempDir>,
    tasks: &mut JoinSet<()>,
) {
    for (index, journal) in journals.into_iter().enumerate() {
        let plan = plan(&format!("load-{index}"));
        let reservation = queue.reserve(&plan).expect("bounded admission");
        let batch = plan.prepare(reservation);
        times.lock().expect("times").push(Timing {
            admitted: Instant::now(),
            executing: None,
            native: None,
            durable: None,
            acknowledged: None,
        });
        let work = AppendWork {
            journal,
            batch: Arc::clone(&batch),
            times: Arc::clone(times),
            gate: Arc::clone(&stall.0),
            index,
        };
        let queue = Arc::clone(queue);
        let owner = Arc::clone(root);
        tasks.spawn(async move {
            let order = queue
                .enter(Arc::new(tokio::sync::Mutex::new(())))
                .await
                .expect("session ordering");
            let times = Arc::clone(&work.times);
            let result = queue
                .execute(owner, batch, order, async move {
                    work.times.lock().expect("times")[index].executing = Some(Instant::now());
                    Ok(rw_resources::run_blocking(
                        rw_resources::ResourceClass::Blocking,
                        move || work.run(),
                    )
                    .await
                    .expect("physical append completion"))
                })
                .await
                .expect("durable acknowledgement");
            times.lock().expect("times")[index].acknowledged = Some(Instant::now());
            drop(result);
        });
    }
}

async fn await_saturation(queue: &JournalCommits, times: &Mutex<Vec<Timing>>) {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let ready = {
                let times = times.lock().expect("times");
                queue.pending_jobs() == MAX_BATCHES
                    && times.iter().filter(|time| time.executing.is_some()).count() == MAX_EXECUTING
                    && times.iter().any(|time| time.native.is_some())
            };
            if ready {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual owner and native worker saturated");
}
async fn verify_reopened(path: std::path::PathBuf) {
    rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        for index in 0..MAX_BATCHES {
            let session = format!("load-{index}");
            let journal = SegmentedJournal::open(&path, &session).expect("reopen settled journal");
            let view = journal.read_view();
            assert_eq!(
                view.verify_all().expect("physical integrity").events,
                EVENTS as u64
            );
            let page = view
                .page::<EngineEvent>(
                    None,
                    SessionEventPageLimits {
                        max_page_events: EVENTS,
                        ..SessionEventPageLimits::default()
                    },
                )
                .expect("bounded exact page");
            let expected = plan(&session);
            for (index, (actual, expected)) in page.events.iter().zip(expected.events()).enumerate()
            {
                assert_eq!(actual.sequence, SequenceId(index as u64));
                assert_eq!(&actual.event, expected);
            }
            assert_eq!(page.events.len(), EVENTS);
        }
    })
    .await
    .expect("reopen proof settled");
}
fn report(round: usize, elapsed: Duration, times: &[Timing], observed: &Observations) {
    let queue_us = times
        .iter()
        .map(|time| {
            time.executing
                .expect("executed")
                .duration_since(time.admitted)
                .as_micros()
        })
        .collect::<Vec<_>>();
    let ack_us = times
        .iter()
        .map(|time| {
            time.acknowledged
                .expect("acknowledged")
                .duration_since(time.admitted)
                .as_micros()
        })
        .collect::<Vec<_>>();
    let durable_us = times
        .iter()
        .map(|time| {
            assert!(time.acknowledged.expect("ack") >= time.durable.expect("durable"));
            time.durable
                .expect("durable")
                .duration_since(time.native.expect("native"))
                .as_micros()
        })
        .collect::<Vec<_>>();
    println!(
        "journal_pressure {}",
        serde_json::json!({
            "schema_version": 1, "round": round, "batches": MAX_BATCHES, "events_per_batch": EVENTS,
            "max_admitted_batches": MAX_BATCHES, "max_admitted_bytes": MAX_BYTES, "max_executing": MAX_EXECUTING,
            "injected_storage_delay_us": DELAY.as_micros(), "elapsed_us": elapsed.as_micros(),
            "events_per_second_including_injected_stall": f64::from(u32::try_from(MAX_BATCHES * EVENTS).expect("bounded event count")) / elapsed.as_secs_f64(),
            "peak_admitted_batches": observed.peak_items, "peak_admitted_bytes": observed.peak_bytes,
            "peak_admitted_not_executing_batches": observed.peak_queued,
            "sampled_oldest_admitted_not_executing_us": observed.oldest_queued_us,
            "admitted_to_executing_us": queue_us, "native_start_to_durable_us": durable_us,
            "admitted_to_ack_us": ack_us, "independent_idle_interrupt_ack_us": observed.control_us,
            "reopened_exact_sequence_and_payload": true, "all_admission_refunded": true
        })
    );
}
