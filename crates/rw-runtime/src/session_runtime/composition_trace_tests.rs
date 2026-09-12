//! Composition preserves span parentage and closes it on early failure.
#![allow(clippy::expect_used)]
use super::{LocalSessionOptions, compose_local_session};
use std::sync::{Arc, Mutex};
use tracing::{Event, Metadata, Subscriber, span};

struct Entry {
    metadata: &'static Metadata<'static>,
    parent: Option<u64>,
    references: usize,
    enters: usize,
    exits: usize,
    closed: usize,
}
#[derive(Default)]
struct State {
    spans: Vec<Entry>,
    active: Vec<u64>,
}
struct Capture {
    enabled: bool,
    state: Arc<Mutex<State>>,
}
impl Subscriber for Capture {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        self.enabled
            && (metadata.target() == "rw_performance" || metadata.name() == "composition-parent")
    }
    fn new_span(&self, attributes: &span::Attributes<'_>) -> span::Id {
        let mut state = self.state.lock().expect("trace state");
        let parent = attributes.parent().map(span::Id::into_u64).or_else(|| {
            attributes
                .is_contextual()
                .then(|| state.active.last().copied())
                .flatten()
        });
        state.spans.push(Entry {
            metadata: attributes.metadata(),
            parent,
            references: 1,
            enters: 0,
            exits: 0,
            closed: 0,
        });
        span::Id::from_u64(u64::try_from(state.spans.len()).expect("finite spans"))
    }
    fn record(&self, _: &span::Id, _: &span::Record<'_>) {}
    fn record_follows_from(&self, _: &span::Id, _: &span::Id) {}
    fn event(&self, _: &Event<'_>) {}
    fn enter(&self, id: &span::Id) {
        let mut state = self.state.lock().expect("trace state");
        state.spans[usize::try_from(id.into_u64()).expect("finite span id") - 1].enters += 1;
        state.active.push(id.into_u64());
    }
    fn exit(&self, id: &span::Id) {
        let mut state = self.state.lock().expect("trace state");
        assert_eq!(state.active.pop(), Some(id.into_u64()));
        state.spans[usize::try_from(id.into_u64()).expect("finite span id") - 1].exits += 1;
    }
    fn clone_span(&self, id: &span::Id) -> span::Id {
        self.state.lock().expect("trace state").spans
            [usize::try_from(id.into_u64()).expect("finite span id") - 1]
            .references += 1;
        id.clone()
    }
    fn try_close(&self, id: span::Id) -> bool {
        let mut state = self.state.lock().expect("trace state");
        let entry = &mut state.spans[usize::try_from(id.into_u64()).expect("finite span id") - 1];
        entry.references -= 1;
        if entry.references == 0 {
            entry.closed += 1;
            true
        } else {
            false
        }
    }
}

fn rejected_options() -> LocalSessionOptions {
    LocalSessionOptions {
        permission_mode: None,
        max_turns: 0,
        resume: None,
        continue_latest: false,
        replay_dir: None,
        record_replay_script: None,
        in_memory_replay_script: None,
        record_script_delay_ms: 0,
        activate_fixture_extensions: false,
        replay_provider: "unused".into(),
        model: None,
        additional_workspaces: Vec::new(),
        dangerously_trust: false,
        purpose: super::super::LocalSessionPurpose::Conversation { interactive: false },
    }
}

#[test]
fn enabled_and_disabled_composition_preserve_failure_parentage_and_span_retirement() {
    for enabled in [false, true] {
        let state = Arc::new(Mutex::new(State::default()));
        tracing::subscriber::with_default(
            Capture {
                enabled,
                state: state.clone(),
            },
            || {
                let parent = tracing::trace_span!("composition-parent");
                let _entered = parent.enter();
                drop(compose_local_session(rejected_options())); // Creation alone starts no span.
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("runtime");
                let result = runtime.block_on(compose_local_session(rejected_options()));
                let Err(error) = result else {
                    panic!("zero turns must be refused")
                };
                assert_eq!(error.to_string(), "--max-turns must be greater than zero");
            },
        );
        let state = state.lock().expect("trace state");
        assert!(state.active.is_empty());
        if enabled {
            assert_eq!(
                state.spans.len(),
                2,
                "one parent and one polled composition"
            );
            let child = &state.spans[1];
            assert_eq!(child.metadata.name(), "runtime.local.compose");
            assert_eq!(child.metadata.target(), "rw_performance");
            assert_eq!(child.metadata.level(), &tracing::Level::TRACE);
            assert_eq!(child.metadata.fields().len(), 0, "options remain private");
            assert_eq!(child.parent, Some(1));
            for entry in &state.spans {
                assert!(entry.enters > 0);
                assert_eq!(entry.enters, entry.exits);
                assert_eq!(entry.closed, 1);
                assert_eq!(entry.references, 0);
            }
        } else {
            assert!(state.spans.is_empty());
        }
    }
}
