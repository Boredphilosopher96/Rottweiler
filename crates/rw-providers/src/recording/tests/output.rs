#![allow(clippy::expect_used)]
use super::*;
use crate::{OutputContract, OutputField, OutputSchema, OutputValidation};

struct StructuredProvider {
    raw: bool,
    valid: bool,
}
#[async_trait]
impl Provider for StructuredProvider {
    fn name(&self) -> &'static str {
        "structured-fixture"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            wire_mode: if self.raw {
                WireMode::OpenAiChatCompletions
            } else {
                WireMode::NormalizedReplay
            },
            ..test_capabilities()
        }
    }
    fn supports_structured_output(&self, _: &str) -> bool {
        true
    }
    async fn settle_effects(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    async fn stream(&self, request: ProviderRequest) -> Result<BoxEventStream, ProviderError> {
        self.produce(request, None)
    }
    async fn stream_with_wire_sink(
        &self,
        request: ProviderRequest,
        sink: Arc<dyn WireFrameSink>,
    ) -> Result<BoxEventStream, ProviderError> {
        self.produce(request, Some(sink))
    }
}
impl StructuredProvider {
    fn produce(
        &self,
        request: ProviderRequest,
        sink: Option<Arc<dyn WireFrameSink>>,
    ) -> Result<BoxEventStream, ProviderError> {
        let output = OutputValidation::prepare(&request, true)?;
        let text = if self.valid {
            r#"{"ok":true}"#
        } else {
            r#"{"ok":1}"#
        };
        let items = if self.raw {
            let mut frames:Vec<RawSseFrame>= [ &text[..4], &text[4..] ].into_iter().map(|part|RawSseFrame{event:None,data:serde_json::json!({"model":"fixture-model","choices":[{"index":0,"delta":{"content":part},"finish_reason":null}]}).to_string()}).collect();
            frames.push(RawSseFrame {
                event: None,
                data:
                    serde_json::json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
                        .to_string(),
            });
            frames.push(RawSseFrame {
                event: None,
                data: "[DONE]".into(),
            });
            if let Some(sink) = sink {
                for frame in &frames {
                    sink.capture(frame.event.as_deref(), &frame.data);
                }
            }
            crate::openai::replay_sse_frames(crate::OpenAiWireMode::ChatCompletions, &frames, true)
        } else {
            vec![
                Ok(ProviderEvent::TextDelta { text: text.into() }),
                Ok(ProviderEvent::Finished {
                    reason: FinishReason::Stop,
                }),
            ]
        };
        Ok(output.attach(BoxEventStream::new(futures_util::stream::iter(items))))
    }
}
fn structured_request() -> ProviderRequest {
    ProviderRequest {
        tool_choice: ToolChoice::None {},
        output: OutputContract::JsonSchema {
            name: "result".into(),
            schema: OutputSchema::Object {
                fields: vec![OutputField {
                    name: "ok".into(),
                    schema: OutputSchema::Boolean {},
                }],
            },
        },
        ..request()
    }
}
#[tokio::test]
async fn raw_and_normalized_recordings_replay_the_same_validated_outcome() {
    for raw in [false, true] {
        for valid in [false, true] {
            let directory = unique_temp_directory("structured-output");
            let recorder = Recorder::new(
                Arc::new(StructuredProvider { raw, valid }),
                &directory,
                FixtureRedactor::default(),
            );
            let observed: Vec<_> = recorder
                .stream(structured_request())
                .await
                .expect("start")
                .collect()
                .await;
            recorder.flush().await.expect("recording settled");
            let replay = ReplayProvider::load("structured-fixture", &directory)
                .await
                .expect("load");
            let replayed: Vec<_> = replay
                .stream(structured_request())
                .await
                .expect("replay start")
                .collect()
                .await;
            assert_eq!(observed, replayed, "raw={raw} valid={valid}");
            assert_eq!(
                observed
                    .iter()
                    .any(|event| matches!(event, Ok(ProviderEvent::Finished { .. }))),
                valid
            );
            if !valid {
                assert!(
                    !observed
                        .iter()
                        .any(|event| matches!(event, Ok(ProviderEvent::TextDelta { .. })))
                );
            }
            let hash = request_hash(&structured_request()).expect("hash");
            let bytes = std::fs::read(fixture_path(&directory, "structured-fixture", &hash, 0))
                .expect("fixture");
            let fixture: RecordFixture = serde_json::from_slice(&bytes).expect("record schema");
            assert_eq!(!fixture.raw_sse.is_empty(), raw);
            if raw {
                assert!(fixture.raw_sse[0].data.contains("content"));
            }
            replay.settle_effects().await.expect("reads settled");
            std::fs::remove_dir_all(&directory).expect("cleanup");
        }
    }
}
#[tokio::test]
async fn routed_structured_contract_is_validated_before_discovery_and_cannot_be_dropped() {
    let provider: Arc<dyn Provider> = Arc::new(StructuredProvider {
        raw: false,
        valid: true,
    });
    let router = crate::ProviderRouter::new(
        std::collections::BTreeMap::from([(
            "structured".into(),
            vec!["structured-fixture/fixture-model".into()],
        )]),
        [provider],
        crate::RetryPolicy::default(),
    )
    .expect("router");
    let stream = router
        .stream_alias(
            "structured",
            structured_request(),
            crate::attempt::fixture_gate(),
        )
        .expect("route");
    OutputValidation::require_installed(
        &stream,
        crate::output_schema::fingerprint(&structured_request().output).expect("contract"),
    )
    .expect("router preserves output custody");
    let events: Vec<_> = stream.collect().await;
    assert!(events.iter().all(Result::is_ok));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(ProviderEvent::Finished { .. })))
            .count(),
        1
    );
    let mut invalid = structured_request();
    invalid.tool_choice = ToolChoice::Auto {};
    assert!(
        router
            .stream_alias("structured", invalid, crate::attempt::fixture_gate())
            .is_err()
    );
    router
        .settle_effects()
        .await
        .expect("all operations settled");
}
