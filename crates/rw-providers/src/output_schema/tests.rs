#![allow(clippy::expect_used)]
use super::*;
use crate::{BoxEventStream, FinishReason, ProviderEvent, ProviderRequest, ToolChoice};
use futures_util::StreamExt;

fn schema() -> OutputSchema {
    OutputSchema::Object {
        fields: vec![
            OutputField {
                name: "count".into(),
                schema: OutputSchema::Integer {},
            },
            OutputField {
                name: "labels".into(),
                schema: OutputSchema::Array {
                    items: Box::new(OutputSchema::String {}),
                },
            },
            OutputField {
                name: "optional".into(),
                schema: OutputSchema::Nullable {
                    value: Box::new(OutputSchema::Boolean {}),
                },
            },
        ],
    }
}
pub(super) fn request() -> ProviderRequest {
    ProviderRequest {
        model: "fixture".into(),
        turns: vec![],
        tools: vec![],
        tool_choice: ToolChoice::None {},
        output: OutputContract::JsonSchema {
            name: "result".into(),
            schema: schema(),
        },
        max_output_tokens: 100,
        temperature: None,
        thinking: rw_types::config::ThinkingLevel::Off,
        cache_hint: None,
    }
}
#[test]
fn finite_schema_requires_complete_unique_fields_and_bounded_allocations() {
    let request = request();
    request.output.validate().expect("valid");
    let mut value = serde_json::to_value(&request).expect("JSON");
    value.as_object_mut().expect("request").remove("output");
    assert!(serde_json::from_value::<ProviderRequest>(value).is_err());
    let mut duplicate = schema();
    if let OutputSchema::Object { fields } = &mut duplicate {
        fields.push(fields[0].clone());
    }
    assert!(
        OutputContract::JsonSchema {
            name: "result".into(),
            schema: duplicate
        }
        .validate()
        .is_err()
    );
    let mut fields = Vec::with_capacity(100_000);
    fields.push(OutputField {
        name: "x".into(),
        schema: OutputSchema::Boolean {},
    });
    assert!(
        OutputContract::JsonSchema {
            name: "result".into(),
            schema: OutputSchema::Object { fields }
        }
        .validate()
        .is_err()
    );
    let mut nested = OutputSchema::Null {};
    for _ in 0..MAX_OUTPUT_SCHEMA_DEPTH {
        nested = OutputSchema::Array {
            items: Box::new(nested),
        };
    }
    assert!(
        OutputContract::JsonSchema {
            name: "result".into(),
            schema: OutputSchema::Object {
                fields: vec![OutputField {
                    name: "x".into(),
                    schema: nested
                }]
            }
        }
        .validate()
        .is_err()
    );
}
#[test]
fn borrowed_validator_rejects_duplicates_extra_keys_rounding_and_trailing_documents() {
    let schema = schema();
    for count in ["1", "1.0", "1200e-2", "0e-999", "-12"] {
        validate::validate(
            &schema,
            &format!(r#"{{"count":{count},"labels":["é"],"optional":null}}"#),
        )
        .expect("integer is exact");
    }
    for count in ["1.0000000000000000001", "1e-9", "1e999", "true", "null"] {
        assert!(
            validate::validate(
                &schema,
                &format!(r#"{{"count":{count},"labels":[],"optional":null}}"#)
            )
            .is_err(),
            "{count}"
        );
    }
    for text in [
        r#"{"count":1,"count":2,"labels":[],"optional":null}"#,
        r#"{"count":1,"labels":[],"optional":null,"extra":0}"#,
        r#"{"count":1,"labels":[]}"#,
        r#"{"count":1,"labels":[],"optional":null} {}"#,
        r#"{"count":1,"labels":[false],"optional":null}"#,
    ] {
        assert!(validate::validate(&schema, text).is_err(), "{text}");
    }
    let many = format!(
        r#"{{"count":1,"labels":[{}],"optional":null}}"#,
        vec!["\"x\""; MAX_STRUCTURED_OUTPUT_NODES].join(",")
    );
    assert!(validate::validate(&schema, &many).is_err());
}
#[tokio::test]
async fn fragmented_unicode_output_commits_only_after_complete_validation() {
    let request = request();
    let output = OutputValidation::prepare(&request, true).expect("admitted");
    let fragments = [r#"{"count":2,"labels":[""#, "é🌲", r#""],"optional":true}"#];
    let mut events: Vec<_> = fragments
        .into_iter()
        .map(|s| Ok(ProviderEvent::TextDelta { text: s.into() }))
        .collect();
    events.push(Ok(ProviderEvent::Finished {
        reason: FinishReason::Stop,
    }));
    let stream = output.attach(BoxEventStream::new(futures_util::stream::iter(events)));
    OutputValidation::require_installed(
        &stream,
        fingerprint(&request.output).expect("fingerprint"),
    )
    .expect("owner");
    let result: Vec<_> = stream.collect().await;
    assert_eq!(
        result,
        vec![
            Ok(ProviderEvent::TextDelta {
                text: fragments.concat()
            }),
            Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop
            })
        ]
    );
}
#[tokio::test]
async fn invalid_truncated_refused_and_cancelled_outputs_never_publish_success_or_partial_json() {
    for (text, tail) in [
        (
            "{}",
            Some(Ok(ProviderEvent::Finished {
                reason: FinishReason::Stop,
            })),
        ),
        (
            r#"{"count":1,"labels":[],"optional":null}"#,
            Some(Ok(ProviderEvent::Finished {
                reason: FinishReason::Length,
            })),
        ),
        (
            "refusal",
            Some(Ok(ProviderEvent::Finished {
                reason: FinishReason::ContentFilter,
            })),
        ),
        (
            "{",
            Some(Err(crate::ProviderError::new(
                crate::ProviderErrorKind::Cancelled,
                "cancelled",
            ))),
        ),
        ("{", None),
    ] {
        let owner = OutputValidation::prepare(&request(), true).expect("admit");
        let events =
            std::iter::once(Ok(ProviderEvent::TextDelta { text: text.into() })).chain(tail);
        let result: Vec<_> = owner
            .attach(BoxEventStream::new(futures_util::stream::iter(events)))
            .collect()
            .await;
        assert_eq!(result.len(), 1);
        assert!(result[0].is_err());
    }
}
#[test]
fn incompatible_tools_and_unsupported_dialects_reject_before_dispatch() {
    let mut request = request();
    assert_eq!(
        OutputValidation::preflight(&request, false)
            .expect_err("unsupported")
            .kind,
        crate::ProviderErrorKind::Unsupported
    );
    request.tool_choice = ToolChoice::Auto {};
    assert_eq!(
        OutputValidation::preflight(&request, true)
            .expect_err("tools")
            .kind,
        crate::ProviderErrorKind::InvalidRequest
    );
    request.output = OutputContract::Text {};
    OutputValidation::prepare(&request, false).expect("text needs no schema support");
}
#[test]
fn both_openai_dialects_project_strict_schema_without_extra_properties() {
    let request = request();
    for mode in [
        crate::OpenAiWireMode::ChatCompletions,
        crate::OpenAiWireMode::Responses,
    ] {
        let mut body = serde_json::Map::new();
        wire::apply(&request.output, &mut body, mode);
        let format = if mode == crate::OpenAiWireMode::Responses {
            &body["text"]["format"]
        } else {
            &body["response_format"]["json_schema"]
        };
        assert_eq!(format["strict"], true);
        assert_eq!(format["schema"]["additionalProperties"], false);
        assert_eq!(
            format["schema"]["required"],
            serde_json::json!(["count", "labels", "optional"])
        );
    }
}
