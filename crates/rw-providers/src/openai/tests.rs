use std::{collections::BTreeMap, sync::Arc};

use rw_types::{
    Block, ImageRef, Role, ToolCallId, ToolOutput, ToolOutputPart, Turn, TurnMeta,
    config::ThinkingLevel,
};
use serde_json::json;
use url::Url;

use crate::{
    AuthMaterial, CacheBreakpointSupport, FinishReason, NativeWebSearchRequest, NetworkPolicy,
    ProviderErrorKind, ProviderEvent, ProviderRequest, Secret, StaticAuth, TokenUsage, ToolChoice,
    ToolDefinition, sse::SseEvent,
};

use super::{
    OpenAiChatRequestProfile, OpenAiCompatibleConfig, OpenAiCompatibleProvider, OpenAiState,
    OpenAiWireMode, ResponsesReasoningSignature, apply_auth_request_shape,
    apply_subscription_request_shape, build_chat_request, build_responses_request,
    decode_responses_reasoning_signature, discovery_endpoint, encode_responses_reasoning_signature,
    openai_stream_error, parse_usage, responses_items,
};

#[test]
fn production_chatgpt_catalog_keeps_protocol_version_query() {
    let inference = Url::parse(crate::OPENAI_SUBSCRIPTION_RESPONSES_ENDPOINT)
        .unwrap_or_else(|error| panic!("built-in inference URL must parse: {error}"));
    let catalog = discovery_endpoint(&inference, true)
        .unwrap_or_else(|error| panic!("built-in catalog URL must compose: {error}"));

    assert_eq!(
        catalog.as_str(),
        format!(
            "{}?client_version={}",
            crate::OPENAI_SUBSCRIPTION_MODELS_ENDPOINT,
            crate::OPENAI_SUBSCRIPTION_MODELS_COMPATIBILITY_VERSION
        )
    );
}

#[test]
fn chat_tool_fragments_and_usage_normalize() {
    let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
    let values = [
        json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call-1","function":{"name":"read","arguments":"{\"path\":"}}]},"finish_reason":null}]}),
        json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"model":"fixture","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":4}}),
    ];
    let mut events = Vec::new();
    for value in values {
        events.extend(
            state
                .handle(&SseEvent {
                    event: None,
                    data: value.to_string(),
                })
                .unwrap_or_else(|error| panic!("chunk must parse: {error}")),
        );
    }
    events.extend(
        state
            .handle(&SseEvent {
                event: None,
                data: "[DONE]".to_owned(),
            })
            .unwrap_or_else(|error| panic!("done must parse: {error}")),
    );
    assert!(events.contains(&ProviderEvent::ToolCallEnd {
        id: "call-1".to_owned(),
        arguments: json!({"path":"a.rs"}),
    }));
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::Finished { .. })
    ));
}

#[test]
fn chat_tool_start_requires_an_id() {
    let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
    let frames = [
        json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"function":{"name":"read","arguments":"{\"path\":"}}]},"finish_reason":null}]}),
        json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":2,"function":{"arguments":"\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}),
    ];
    let Err(error) = state.handle(&SseEvent {
        event: None,
        data: frames[0].to_string(),
    }) else {
        panic!("missing tool id must be rejected");
    };
    assert_eq!(error.kind, ProviderErrorKind::Protocol);
}

#[test]
fn legacy_function_call_without_an_id_is_rejected() {
    let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
    let frame = json!({"model":"fixture","choices":[{"index":0,"delta":{"function_call":{"name":"shell"}},"finish_reason":null}]});
    let Err(error) = state.handle(&SseEvent {
        event: None,
        data: frame.to_string(),
    }) else {
        panic!("legacy tool calls omit an id");
    };
    assert_eq!(error.kind, ProviderErrorKind::Protocol);
}

#[test]
fn duplicate_chat_tool_index_is_rejected() {
    let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
    for (id, name) in [("call-1", "read"), ("call-2", "write")] {
        let frame = json!({"model":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"function":{"name":name}}]}}]});
        let result = state.handle(&SseEvent {
            event: None,
            data: frame.to_string(),
        });
        if id == "call-1" {
            assert!(result.is_ok());
        } else {
            let Err(error) = result else {
                panic!("duplicate index must be rejected");
            };
            assert_eq!(error.kind, ProviderErrorKind::Protocol);
        }
    }
}

#[test]
fn duplicate_responses_tool_index_is_rejected() {
    let mut state = OpenAiState::new(OpenAiWireMode::Responses);
    for id in ["call-1", "call-2"] {
        let frame = json!({
            "type":"response.output_item.added",
            "output_index":0,
            "item":{"type":"function_call","call_id":id,"name":"read"}
        });
        let result = state.handle(&SseEvent {
            event: None,
            data: frame.to_string(),
        });
        if id == "call-1" {
            assert!(result.is_ok());
        } else {
            let Err(error) = result else {
                panic!("duplicate index must be rejected");
            };
            assert_eq!(error.kind, ProviderErrorKind::Protocol);
        }
    }
}

#[test]
fn compatible_content_arrays_refusals_and_length_normalize() {
    let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
    let frames = [
        json!({"model":"fixture","choices":[{"index":0,"delta":{"content":[{"type":"text","text":"hello "},{"type":"text","text":{"value":"world"}}]},"finish_reason":null}]}),
        json!({"model":"fixture","choices":[{"index":0,"delta":{"refusal":"cannot continue"},"finish_reason":"length"}]}),
    ];
    let mut events = Vec::new();
    for frame in frames {
        events.extend(
            state
                .handle(&SseEvent {
                    event: None,
                    data: frame.to_string(),
                })
                .unwrap_or_else(|error| panic!("content chunk must parse: {error}")),
        );
    }
    events.extend(
        state
            .handle(&SseEvent {
                event: None,
                data: "[DONE]".to_owned(),
            })
            .unwrap_or_else(|error| panic!("content completion must parse: {error}")),
    );

    assert!(events.contains(&ProviderEvent::TextDelta {
        text: "hello world".to_owned(),
    }));
    assert!(events.contains(&ProviderEvent::TextDelta {
        text: "cannot continue".to_owned(),
    }));
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::Finished {
            reason: FinishReason::Length
        })
    ));
}

#[test]
fn chat_request_shape_is_selected_by_connection_profile() {
    let request = ProviderRequest {
        model: "fixture".to_owned(),
        turns: Vec::new(),
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };

    let openai = build_chat_request(&request, false, OpenAiChatRequestProfile::OpenAi);
    assert_eq!(openai["max_completion_tokens"], 128);
    assert_eq!(openai["stream_options"]["include_usage"], true);
    assert!(openai.get("max_tokens").is_none());

    let compatible = build_chat_request(&request, false, OpenAiChatRequestProfile::Compatible);
    assert_eq!(compatible["max_tokens"], 128);
    assert!(compatible.get("max_completion_tokens").is_none());
    assert!(compatible.get("stream_options").is_none());
}

#[test]
fn responses_text_reasoning_citation_and_usage_normalize() {
    let mut state = OpenAiState::new(OpenAiWireMode::Responses);
    let frames = [
        ("response.created", json!({"response":{"model":"fixture"}})),
        ("response.output_text.delta", json!({"delta":"hello"})),
        (
            "response.output_item.added",
            json!({"output_index":0,"item":{"type":"reasoning","id":"rs_fixture"}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"output_index":0,"delta":"considering"}),
        ),
        (
            "response.output_item.done",
            json!({"output_index":0,"item":{"type":"reasoning","id":"rs_fixture","summary":[{"type":"summary_text","text":"considering"}],"encrypted_content":"encrypted-fixture"}}),
        ),
        (
            "response.output_text.annotation.added",
            json!({"annotation":{"type":"url_citation","url":"https://example.test","title":"Example","start_index":0,"end_index":5}}),
        ),
        (
            "response.output_item.done",
            json!({"item":{"type":"web_search_call","action":{"sources":[{"url":"https://source.example/path","title":"Source"}]}}}),
        ),
        (
            "response.completed",
            json!({"response":{"usage":{"input_tokens":8,"output_tokens":3,"output_tokens_details":{"reasoning_tokens":1}}}}),
        ),
    ];
    let mut events = Vec::new();
    for (kind, value) in frames {
        events.extend(
            state
                .handle(&SseEvent {
                    event: Some(kind.to_owned()),
                    data: value.to_string(),
                })
                .unwrap_or_else(|error| panic!("response event must parse: {error}")),
        );
    }
    assert!(
        events
            .iter()
            .any(|event| matches!(event, ProviderEvent::Citation { .. }))
    );
    assert!(events.iter().any(|event| matches!(event, ProviderEvent::Citation { uri, .. } if uri == "https://source.example/path")));
    let reasoning_signature = events.iter().rev().find_map(|event| match event {
        ProviderEvent::ThinkingDelta {
            signature: Some(signature),
            ..
        } => decode_responses_reasoning_signature(signature),
        _ => None,
    });
    assert_eq!(
        reasoning_signature,
        Some(ResponsesReasoningSignature {
            id: "rs_fixture".to_owned(),
            encrypted_content: Some("encrypted-fixture".to_owned()),
        })
    );
    assert!(matches!(
        events.last(),
        Some(ProviderEvent::Finished { .. })
    ));
}

#[test]
fn responses_suppresses_empty_reasoning_transport_noise() {
    let mut state = OpenAiState::new(OpenAiWireMode::Responses);
    for (kind, value) in [
        (
            "response.output_item.added",
            json!({"output_index":0,"item":{"type":"reasoning","id":"rs_fixture"}}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"output_index":0,"delta":""}),
        ),
    ] {
        assert!(
            state
                .handle(&SseEvent {
                    event: Some(kind.to_owned()),
                    data: value.to_string(),
                })
                .unwrap_or_else(|error| panic!("response event must parse: {error}"))
                .is_empty()
        );
    }
    let done = state
        .handle(&SseEvent {
            event: Some("response.output_item.done".to_owned()),
            data: json!({"output_index":0,"item":{"type":"reasoning","id":"rs_fixture","encrypted_content":"opaque"}}).to_string(),
        })
        .unwrap_or_else(|error| panic!("reasoning completion must parse: {error}"));
    assert!(matches!(
        done.as_slice(),
        [ProviderEvent::ThinkingDelta {
            content,
            signature: Some(_),
        }] if content.is_empty()
    ));
}

#[test]
fn responses_native_search_uses_official_tool_and_sources_shape() {
    let request = ProviderRequest {
        model: "gpt-fixture".to_owned(),
        turns: vec![Turn {
            role: Role::User,
            blocks: vec![Block::Text {
                text: "rust lsp".to_owned(),
            }],
            meta: TurnMeta::default(),
        }],
        tools: vec![
            NativeWebSearchRequest {
                query: "rust lsp".to_owned(),
                max_results: 5,
                recency_days: None,
                allowed_domains: vec!["rust-lang.org".to_owned()],
            }
            .tool_definition()
            .unwrap_or_else(|error| panic!("native search marker must encode: {error}")),
        ],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 256,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };
    let body = build_responses_request(&request, false);
    assert_eq!(body["tools"][0]["type"], "web_search");
    assert_eq!(
        body["tools"][0]["filters"]["allowed_domains"],
        json!(["rust-lang.org"])
    );
    assert_eq!(body["include"], json!(["web_search_call.action.sources"]));
    assert!(body.to_string().contains("rust lsp"));
}

#[test]
fn responses_request_continues_only_valid_same_adapter_reasoning() {
    let opaque = encode_responses_reasoning_signature(&ResponsesReasoningSignature {
        id: "rs_previous".to_owned(),
        encrypted_content: Some("encrypted-previous".to_owned()),
    })
    .unwrap_or_else(|| panic!("valid reasoning signature must encode"));
    let turns = vec![Turn {
        role: Role::Assistant,
        blocks: vec![
            Block::Thinking {
                content: "summary".to_owned(),
                signature: Some(opaque),
            },
            Block::Thinking {
                content: "must be ignored".to_owned(),
                signature: Some("anthropic-or-corrupt-signature".to_owned()),
            },
        ],
        meta: TurnMeta::default(),
    }];
    let request = ProviderRequest {
        model: "gpt-fixture".to_owned(),
        turns: turns.clone(),
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 128,
        temperature: None,
        thinking: ThinkingLevel::Medium,
        cache_hint: None,
    };

    let body = build_responses_request(&request, true);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert_eq!(body["reasoning"]["effort"], "medium");
    assert_eq!(
        body["input"],
        json!([{
            "type": "reasoning",
            "id": "rs_previous",
            "summary": [{"type": "summary_text", "text": "summary"}],
            "encrypted_content": "encrypted-previous",
        }])
    );
    assert_eq!(responses_items(&turns[0]).len(), 1);

    let mut disabled = request;
    disabled.thinking = ThinkingLevel::Off;
    let body = build_responses_request(&disabled, true);
    assert_eq!(body["reasoning"]["effort"], "none");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    let body = build_responses_request(&disabled, false);
    assert!(body.get("reasoning").is_none());
    assert!(body.get("include").is_none());
}

#[test]
fn tool_choice_uses_each_openai_wire_shape() {
    let mut request = ProviderRequest {
        model: "gpt-fixture".to_owned(),
        turns: Vec::new(),
        tools: vec![ToolDefinition {
            name: "live_smoke_ping".to_owned(),
            description: "fixture".to_owned(),
            input_schema: json!({"type": "object"}),
        }],
        tool_choice: ToolChoice::Named {
            name: "live_smoke_ping".to_owned(),
        },
        max_output_tokens: 32,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };

    assert_eq!(
        build_chat_request(&request, false, OpenAiChatRequestProfile::OpenAi)["tool_choice"],
        json!({"type":"function","function":{"name":"live_smoke_ping"}})
    );
    assert_eq!(
        build_responses_request(&request, false)["tool_choice"],
        json!({"type":"function","name":"live_smoke_ping"})
    );
    for (choice, expected) in [
        (ToolChoice::Auto {}, "auto"),
        (ToolChoice::Required {}, "required"),
        (ToolChoice::None {}, "none"),
    ] {
        request.tool_choice = choice;
        assert_eq!(
            build_chat_request(&request, false, OpenAiChatRequestProfile::OpenAi)["tool_choice"],
            expected
        );
        assert_eq!(
            build_responses_request(&request, false)["tool_choice"],
            expected
        );
    }
}

#[test]
fn subscription_shape_moves_system_text_and_sets_codex_fields() {
    let request = ProviderRequest {
        model: "gpt-5.6".to_owned(),
        turns: vec![
            Turn {
                role: Role::System,
                blocks: vec![Block::Text {
                    text: "first system".to_owned(),
                }],
                meta: TurnMeta::default(),
            },
            Turn {
                role: Role::System,
                blocks: vec![Block::Text {
                    text: "second system".to_owned(),
                }],
                meta: TurnMeta::default(),
            },
            Turn {
                role: Role::User,
                blocks: vec![Block::Text {
                    text: "hello".to_owned(),
                }],
                meta: TurnMeta::default(),
            },
        ],
        tools: vec![ToolDefinition {
            name: "read_file".to_owned(),
            description: "read".to_owned(),
            input_schema: json!({"type":"object"}),
        }],
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 321,
        temperature: None,
        thinking: ThinkingLevel::Medium,
        cache_hint: None,
    };
    let mut body = build_responses_request(&request, true);
    apply_subscription_request_shape(&mut body, "rw-session-fixture")
        .unwrap_or_else(|error| panic!("subscription shape must apply: {error}"));

    assert_eq!(body["instructions"], "first system\n\nsecond system");
    assert_eq!(body["store"], false);
    assert_eq!(body["prompt_cache_key"], "rw-session-fixture");
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("max_output_tokens").is_none());
    assert_eq!(body["tools"][0]["strict"], false);
    assert!(
        body["input"]
            .as_array()
            .is_some_and(|items| items.iter().all(|item| item["role"] != "system"))
    );

    let mut empty = build_responses_request(
        &ProviderRequest {
            turns: Vec::new(),
            tools: Vec::new(),
            ..request
        },
        false,
    );
    apply_subscription_request_shape(&mut empty, "rw-empty")
        .unwrap_or_else(|error| panic!("empty subscription shape must apply: {error}"));
    assert_eq!(empty["instructions"], "");
}

#[test]
fn responses_reasoning_without_real_id_never_gets_an_opaque_signature() {
    let mut state = OpenAiState::new(OpenAiWireMode::Responses);
    let events = state
        .handle(&SseEvent {
            event: Some("response.output_item.done".to_owned()),
            data: json!({
                "output_index": 0,
                "item": {
                    "type": "reasoning",
                    "summary": [{"type":"summary_text","text":"visible summary"}],
                    "encrypted_content": "orphaned-encrypted-content"
                }
            })
            .to_string(),
        })
        .unwrap_or_else(|error| panic!("reasoning event must parse: {error}"));
    assert_eq!(
        events,
        vec![ProviderEvent::ThinkingDelta {
            content: "visible summary".to_owned(),
            signature: None,
        }]
    );
}

#[test]
fn usage_classes_are_disjoint_for_chat_and_responses() {
    for usage in [
        json!({
            "prompt_tokens": 13,
            "completion_tokens": 7,
            "prompt_tokens_details": {"cached_tokens": 2},
            "completion_tokens_details": {"reasoning_tokens": 1}
        }),
        json!({
            "input_tokens": 17,
            "output_tokens": 6,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens_details": {"reasoning_tokens": 2}
        }),
    ] {
        let normalized = parse_usage(&usage);
        assert_eq!(
            normalized.input_tokens + normalized.cache_read_tokens,
            usage["prompt_tokens"]
                .as_u64()
                .or_else(|| usage["input_tokens"].as_u64())
                .unwrap_or_default()
        );
        assert_eq!(
            normalized.output_tokens + normalized.reasoning_tokens,
            usage["completion_tokens"]
                .as_u64()
                .or_else(|| usage["output_tokens"].as_u64())
                .unwrap_or_default()
        );
    }
    assert_eq!(
        parse_usage(&json!({
            "input_tokens": 17,
            "output_tokens": 6,
            "input_tokens_details": {"cached_tokens": 4},
            "output_tokens_details": {"reasoning_tokens": 2}
        })),
        TokenUsage {
            input_tokens: 13,
            output_tokens: 4,
            cache_read_tokens: 4,
            cache_write_tokens: 0,
            reasoning_tokens: 2,
        }
    );
}

#[test]
fn streamed_errors_are_classified_without_retrying_client_faults() {
    let fixtures = [
        ("invalid_api_key", ProviderErrorKind::Authentication),
        ("invalid_request_error", ProviderErrorKind::InvalidRequest),
        (
            "context_length_exceeded",
            ProviderErrorKind::ContextOverflow,
        ),
        ("rate_limit_exceeded", ProviderErrorKind::RateLimited),
        ("server_error", ProviderErrorKind::Server),
        ("future_error_type", ProviderErrorKind::Protocol),
    ];
    for (code, expected) in fixtures {
        let error = openai_stream_error(&json!({
            "type": "error",
            "error": {"code": code, "message": "sanitized fixture"}
        }));
        assert_eq!(error.kind, expected, "classification for {code}");
    }
    let nested = openai_stream_error(&json!({
        "type": "response.failed",
        "response": {"error": {"type": "invalid_request_error"}}
    }));
    assert_eq!(nested.kind, ProviderErrorKind::InvalidRequest);
}

#[test]
fn copilot_gpt_omits_wire_max_but_other_auth_keeps_it() {
    let request = ProviderRequest {
        model: "gpt-fixture".to_owned(),
        turns: Vec::new(),
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 512,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };
    let mut copilot = build_responses_request(&request, false);
    apply_auth_request_shape(
        &mut copilot,
        &AuthMaterial::GitHubCopilot {
            access_token: Secret::new("fixture"),
            user_agent: "rottweiler/fixture".to_owned(),
            initiator: "user".to_owned(),
            vision: false,
            omit_max_output_tokens: true,
        },
    );
    assert!(copilot.get("max_output_tokens").is_none());

    let mut ordinary = build_responses_request(&request, false);
    apply_auth_request_shape(&mut ordinary, &AuthMaterial::Bearer(Secret::new("fixture")));
    assert_eq!(ordinary["max_output_tokens"], json!(512));
}

#[test]
fn chat_reasoning_opaque_round_trips_through_ir_signature() {
    let mut state = OpenAiState::new(OpenAiWireMode::ChatCompletions);
    let events = state
        .handle(&SseEvent {
            event: None,
            data: json!({
                "model": "fixture",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "reasoning_content": "summary",
                        "reasoning_opaque": "opaque-fixture"
                    }
                }]
            })
            .to_string(),
        })
        .unwrap_or_else(|error| panic!("chat reasoning must parse: {error}"));
    let signature = events.into_iter().find_map(|event| match event {
        ProviderEvent::ThinkingDelta {
            signature: Some(signature),
            ..
        } => Some(signature),
        _ => None,
    });
    let signature = signature.unwrap_or_else(|| panic!("signature must be emitted"));
    let messages = super::chat_messages(&Turn {
        role: Role::Assistant,
        blocks: vec![Block::Thinking {
            content: "summary".to_owned(),
            signature: Some(signature),
        }],
        meta: TurnMeta::default(),
    });
    assert_eq!(messages[0]["reasoning_content"], json!("summary"));
    assert_eq!(messages[0]["reasoning_opaque"], json!("opaque-fixture"));
}

#[test]
fn vision_rejection_detects_images_nested_in_tool_results() {
    let provider = OpenAiCompatibleProvider::new(OpenAiCompatibleConfig {
        name: "fixture".to_owned(),
        endpoint: Url::parse("http://127.0.0.1:9/responses")
            .unwrap_or_else(|error| panic!("fixture URL must parse: {error}")),
        auth: Arc::new(StaticAuth::new(AuthMaterial::None)),
        proxy: None,
        proxy_authentication: None,
        network_policy: NetworkPolicy::Deny,
        wire_mode: OpenAiWireMode::Responses,
        chat_request_profile: OpenAiChatRequestProfile::OpenAi,
        tool_calling: true,
        cache_breakpoints: CacheBreakpointSupport::None,
        supported_reasoning_efforts: Vec::new(),
        supports_vision: false,
        max_context_tokens: None,
        max_output_tokens: None,
        headers: BTreeMap::new(),
        header_credentials: BTreeMap::new(),
        extra_body: BTreeMap::new(),
        model_ids: BTreeMap::new(),
        path_template: None,
    })
    .unwrap_or_else(|error| panic!("fixture provider must build: {error}"));
    let request = ProviderRequest {
        model: "fixture".to_owned(),
        turns: vec![Turn {
            role: Role::Tool,
            blocks: vec![Block::ToolResult {
                id: ToolCallId("call-1".to_owned()),
                output: ToolOutput::Mixed {
                    parts: vec![ToolOutputPart::Image {
                        media_type: "image/png".to_owned(),
                        data: ImageRef::InlineBase64 {
                            data: "fixture".to_owned(),
                        },
                    }],
                },
                is_error: false,
            }],
            meta: TurnMeta::default(),
        }],
        tools: Vec::new(),
        tool_choice: ToolChoice::Auto {},
        max_output_tokens: 32,
        temperature: None,
        thinking: ThinkingLevel::Off,
        cache_hint: None,
    };
    let Err(error) = provider.validate_request_capabilities(&request) else {
        panic!("nested image must be rejected");
    };
    assert_eq!(error.kind, ProviderErrorKind::Unsupported);
}
