//! Compare actual journal operations with local tracing disabled and enabled.
//! Build again with `--features tracing/max_level_off` for the compiled-out baseline.
use rw_store::session::{SessionEventPageLimits, journal::SegmentedJournal};
use serde_json::{Value, json};
use std::{error::Error, hint::black_box, time::Instant};
use tracing_subscriber::fmt::format::FmtSpan;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const SAMPLES: usize = 21;
const REPETITIONS: u32 = 200;

fn main() -> Result<()> {
    let root = tempfile::tempdir()?;
    let mut journal = SegmentedJournal::open(root.path(), "trace-cost")?;
    journal.append_batch((0..16).map(|index| json!({"index": index, "text": "x".repeat(128)})))?;
    let view = journal.read_view();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(FmtSpan::CLOSE)
        .with_ansi(false)
        .with_writer(std::io::sink)
        .finish();
    let enabled = tracing::Dispatch::new(subscriber);
    let disabled = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    let mut samples = Vec::new();
    for sample in 0..SAMPLES {
        for offset in 0..2 {
            let traced = (sample + offset) % 2 == 1;
            let dispatch = if traced { &enabled } else { &disabled };
            let row = tracing::dispatcher::with_default(dispatch, || -> Result<Value> {
                let start = Instant::now();
                for _ in 0..REPETITIONS {
                    black_box(journal.append_batch(std::iter::empty::<Value>())?);
                }
                let empty_append_ns = start.elapsed().as_nanos() / u128::from(REPETITIONS);
                let start = Instant::now();
                for _ in 0..REPETITIONS {
                    black_box(view.page::<Value>(None, SessionEventPageLimits::default())?);
                }
                let page_ns = start.elapsed().as_nanos() / u128::from(REPETITIONS);
                let start = Instant::now();
                journal.append_batch([json!({"sample": sample, "traced": traced})])?;
                let durable_append_ns = start.elapsed().as_nanos();
                Ok(json!({"sample": sample, "traced": traced,
                    "empty_append_ns": empty_append_ns, "page_ns": page_ns,
                    "durable_append_ns": durable_append_ns}))
            })?;
            samples.push(row);
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "static_max_level": tracing::level_filters::STATIC_MAX_LEVEL.to_string(),
            "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
            "os": std::env::consts::OS, "architecture": std::env::consts::ARCH,
            "repetitions": REPETITIONS, "samples": samples,
            "sink": "formatted close events to io::sink; excludes terminal or disk log output"
        }))?
    );
    Ok(())
}
