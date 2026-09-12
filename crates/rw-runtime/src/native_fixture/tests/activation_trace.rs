//! Opt-in process-wide metadata evidence across independent libtest runtimes.
use std::{fmt::Write as _, io::Write as _, sync::OnceLock, time::Instant};
use tracing::{Event, Metadata, Subscriber, field::Visit, span};

pub(super) fn install() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if std::env::var_os("ROTTWEILER_TEST_ACTIVATION_TRACE").is_none() {
        return;
    }
    INSTALLED.get_or_init(|| {
        #[allow(clippy::expect_used)]
        tracing::subscriber::set_global_default(ActivationTrace(Instant::now()))
            .expect("explicit activation trace requires the libtest subscriber");
    });
}

struct ActivationTrace(Instant);
impl Subscriber for ActivationTrace {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "rw_performance"
    }
    fn new_span(&self, _attributes: &span::Attributes<'_>) -> span::Id {
        span::Id::from_u64(1)
    }
    fn record(&self, _span: &span::Id, _values: &span::Record<'_>) {}
    fn record_follows_from(&self, _span: &span::Id, _follows: &span::Id) {}
    fn enter(&self, _span: &span::Id) {}
    fn exit(&self, _span: &span::Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        if !fields.plugin_stage {
            return;
        }
        // Libtest's per-thread capture does not follow physical workers or
        // actors created by a different test runtime. Use the physical stderr.
        let line = format!(
            "activation_trace t_ms={:.3} thread={:?} {}\n",
            self.0.elapsed().as_secs_f64() * 1000.0,
            std::thread::current().id(),
            fields.text
        );
        let _ = std::io::stderr().lock().write_all(line.as_bytes());
    }
}

#[derive(Default)]
struct Fields {
    plugin_stage: bool,
    text: String,
}
impl Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "stage" {
            self.plugin_stage = value.starts_with("plugin.");
        }
        self.record_debug(field, &value);
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(
            field.name(),
            "stage" | "phase" | "plugin" | "fixture" | "admission_ms" | "elapsed_ms" | "succeeded"
        ) {
            let _ = write!(self.text, "{}={value:?} ", field.name());
        }
    }
}
