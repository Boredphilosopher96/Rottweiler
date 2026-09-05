#![allow(clippy::expect_used)]
use super::SegmentedJournal;
use crate::session::SessionEventPageLimits;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tracing::{
    Subscriber,
    span::{Attributes, Id, Record},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*, registry::LookupSpan};

thread_local! {
    static SYNC_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
pub(super) fn run_sync_hook() {
    let hook = SYNC_HOOK.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}
#[derive(Default)]
struct Observed {
    name: &'static str,
    parent: Option<&'static str>,
    fields: BTreeMap<String, String>,
    elapsed: Duration,
}
impl tracing::field::Visit for Observed {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_owned(), format!("{value:?}"));
    }
}
struct Active {
    started: Instant,
    observed: Observed,
}
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<Observed>>>);
impl<S> Layer<S> for Capture
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        let span = context.span(id).expect("span");
        let mut observed = Observed {
            name: attrs.metadata().name(),
            parent: span.parent().map(|parent| parent.name()),
            ..Observed::default()
        };
        attrs.record(&mut observed);
        span.extensions_mut().insert(Active {
            started: Instant::now(),
            observed,
        });
    }
    fn on_record(&self, id: &Id, record: &Record<'_>, context: Context<'_, S>) {
        let span = context.span(id).expect("span");
        let mut extensions = span.extensions_mut();
        record.record(&mut extensions.get_mut::<Active>().expect("active").observed);
    }
    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let span = context.span(&id).expect("span");
        let active = span.extensions_mut().remove::<Active>().expect("active");
        let mut observed = active.observed;
        observed.elapsed = active.started.elapsed();
        self.0.lock().expect("captures").push(observed);
    }
}
#[test]
fn slow_sync_is_attributed_to_durability_without_recording_payloads() {
    let root = tempfile::tempdir().expect("root");
    let mut journal = SegmentedJournal::open(root.path(), "trace-session").expect("journal");
    let records = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        Capture(Arc::clone(&records)).with_filter(
            tracing_subscriber::filter::Targets::new()
                .with_target("rw_performance", tracing::Level::TRACE),
        ),
    );
    tracing::subscriber::with_default(subscriber, || {
        SYNC_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(|| std::thread::sleep(Duration::from_millis(20))));
        });
        journal
            .append_batch([json!({"secret": "DO-NOT-RECORD-PAYLOAD"})])
            .expect("append");
        journal
            .read_view()
            .page::<Value>(None, SessionEventPageLimits::default())
            .expect("page");
    });
    let records = records.lock().expect("records");
    let sync = records
        .iter()
        .find(|span| span.name == "journal.sync")
        .expect("sync span");
    assert!(sync.elapsed >= Duration::from_millis(20));
    assert_eq!(sync.parent, Some("journal.append"));
    let append = records
        .iter()
        .find(|span| span.name == "journal.append")
        .expect("append span");
    assert!(append.elapsed >= sync.elapsed);
    assert_eq!(append.fields.get("events").map(String::as_str), Some("1"));
    assert!(append.fields["session_id"].contains("trace-session"));
    let page = records
        .iter()
        .find(|span| span.name == "journal.page")
        .expect("page span");
    assert_eq!(
        page.fields.get("records_decoded").map(String::as_str),
        Some("1")
    );
    assert!(page.fields["bytes_read"].parse::<u64>().expect("bytes") > 0);
    for span in records.iter() {
        for value in span.fields.values() {
            assert!(!value.contains("DO-NOT-RECORD-PAYLOAD"));
            assert!(!value.contains(&root.path().to_string_lossy().to_string()));
        }
    }
}
