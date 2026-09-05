use super::{RecordingWriter, WriterMessage};
use crate::ProviderErrorKind;
use std::sync::atomic::Ordering;

#[tokio::test]
async fn lost_worker_returns_sticky_failed_proof_instead_of_waiting_forever() {
    let writer = RecordingWriter::new(1);
    drop(writer.receiver.lock().expect("receiver").take());
    assert_eq!(
        writer.settle().await.expect_err("worker lost").kind,
        ProviderErrorKind::EffectsUnsettled
    );
    assert!(writer.failed.load(Ordering::Acquire));
    assert!(
        writer.reserve().await.is_err(),
        "failed owner rejects new writes"
    );
    assert_eq!(
        writer.settle().await.expect_err("sticky").kind,
        ProviderErrorKind::EffectsUnsettled
    );
}

#[tokio::test(start_paused = true)]
async fn queued_barrier_deadline_keeps_writer_owned_and_failure_sticky() {
    let writer = RecordingWriter::new(1);
    let mut receiver = writer
        .receiver
        .lock()
        .expect("receiver")
        .take()
        .expect("unstarted receiver");
    let result = writer.settle().await;
    assert_eq!(
        result.expect_err("deadline").kind,
        ProviderErrorKind::EffectsUnsettled
    );
    let Some(WriterMessage::Settled(completion)) = receiver.recv().await else {
        panic!("owned queued barrier")
    };
    let _ = completion.send(());
    assert_eq!(
        writer
            .settle()
            .await
            .expect_err("late completion does not erase failed proof")
            .kind,
        ProviderErrorKind::EffectsUnsettled
    );
}

struct PanickingProvider;
#[async_trait::async_trait]
impl crate::Provider for PanickingProvider {
    async fn settle_effects(&self) -> Result<(), crate::ProviderError> {
        panic!("injected provider proof panic")
    }
    fn name(&self) -> &str {
        "proof-panic"
    }
    fn capabilities(&self) -> crate::Capabilities {
        crate::Capabilities {
            tool_calling: false,
            vision: false,
            thinking: false,
            cache_breakpoints: crate::CacheBreakpointSupport::None,
            max_context_tokens: None,
            max_output_tokens: None,
            wire_mode: crate::WireMode::NormalizedReplay,
        }
    }
    async fn stream(
        &self,
        _: crate::ProviderRequest,
    ) -> Result<crate::BoxEventStream, crate::ProviderError> {
        Err(crate::ProviderError::new(
            ProviderErrorKind::Unsupported,
            "not invoked",
        ))
    }
}

#[tokio::test]
async fn inner_provider_panic_still_attempts_and_waits_for_recording_worker_proof() {
    use crate::Provider as _;
    let recorder = std::sync::Arc::new(super::super::Recorder::new(
        std::sync::Arc::new(PanickingProvider),
        std::env::temp_dir(),
        super::super::FixtureRedactor::default(),
    ));
    let mut receiver = recorder
        .writer
        .receiver
        .lock()
        .expect("receiver")
        .take()
        .expect("unstarted worker");
    let owner = std::sync::Arc::clone(&recorder);
    let proof = tokio::spawn(async move { owner.settle_effects().await });
    let Some(WriterMessage::Settled(completion)) = receiver.recv().await else {
        panic!("writer proof attempted after provider panic")
    };
    assert!(!proof.is_finished(), "writer still owns settlement");
    completion.send(()).expect("writer completion");
    assert_eq!(
        proof
            .await
            .expect("proof task")
            .expect_err("provider failed")
            .kind,
        ProviderErrorKind::EffectsUnsettled
    );
}
