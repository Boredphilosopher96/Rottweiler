#![allow(clippy::expect_used)]
use super::*;
use rmcp::model::{JsonRpcMessage, RequestId, ServerNotification, ServerResult};
use serde_json::{Value, json};

fn accepted(value: &Value, method: Option<&str>) -> ServerJsonRpcMessage {
    let input = serde_json::to_vec(value).expect("wire");
    let mut admitted = false;
    let result = decode(&input, method, &mut |bytes| {
        assert!(bytes > 0 && bytes <= 128 * 1024 * 1024);
        assert!(!admitted, "one allocation admission");
        admitted = true;
        Ok(())
    })
    .expect("admitted typed message");
    assert!(admitted);
    result
}

#[test]
fn duplicate_or_conflicting_envelope_selectors_never_reach_admission() {
    for input in [
        r#"{"jsonrpc":"2.0","id":1,"id":2,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"\u0069d":2,"result":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"result":{},"result":{}}"#,
        r#"{"jsonrpc":"2.0","method":"ping","method":"roots/list"}"#,
        r#"{"jsonrpc":"2.0","method":"ping","id":1,"result":{}}"#,
        r#"{"jsonrpc":"2.0","result":{}}"#,
        r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#,
        r#"{"jsonrpc":"1.0","id":1,"result":{}}"#,
    ] {
        assert!(classify(input.as_bytes()).is_err(), "{input}");
        let mut called = false;
        assert!(
            decode(input.as_bytes(), Some("ping"), &mut |_| {
                called = true;
                Ok(())
            })
            .is_err(),
            "{input}"
        );
        assert!(!called, "reject before typed admission");
    }
}

#[test]
fn admission_denial_precedes_typed_body_validation_and_retains_error_identity() {
    let wire = br#"{"jsonrpc":"2.0","id":9,"result":{"tools":"invalid typed tools"}}"#;
    let error = decode(wire, Some("tools/list"), &mut |_| {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "global payload pool full",
        ))
    })
    .expect_err("admission denied");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    assert_eq!(error.to_string(), "global payload pool full");
    assert!(decode(wire, Some("tools/list"), &mut |_| Ok(())).is_err());
    let mut admitted = false;
    assert!(
        decode(wire, None, &mut |_| {
            admitted = true;
            Ok(())
        })
        .is_err()
    );
    assert!(!admitted, "uncorrelated response cannot allocate its body");
}

#[test]
fn response_selection_uses_request_method_not_greedy_union_order() {
    // This contains valid fields for two rmcp union variants. The correlated
    // resources/list request, not variant declaration order, owns selection.
    let wire = json!({"jsonrpc":"2.0","id":7,"result":{"tools":[],"resources":[]}});
    assert!(matches!(accepted(&wire, Some("resources/list")),
        JsonRpcMessage::Response(response) if matches!(&response.result, ServerResult::ListResourcesResult(_))));
    assert!(matches!(accepted(&wire, Some("tools/list")),
        JsonRpcMessage::Response(response) if matches!(&response.result, ServerResult::ListToolsResult(_))));
}

#[test]
fn embedded_resource_and_custom_payload_bytes_survive_direct_decode() {
    let text = "\0 exact UTF-8 λ\n".repeat(4096);
    let result = json!({"content":[{"type":"resource","resource":{
        "uri":"memory://source","text":text,"mimeType":"text/plain"}}],
        "structuredContent":{"raw":[null,true,17,{"text":text}]},"isError":false});
    let decoded = accepted(
        &json!({"jsonrpc":"2.0","id":1,"result":result}),
        Some("tools/call"),
    );
    let JsonRpcMessage::Response(response) = decoded else {
        panic!("response")
    };
    let ServerResult::CallToolResult(result) = response.result else {
        panic!("tool result")
    };
    assert_eq!(
        result.structured_content.expect("structured")["raw"][3]["text"],
        text
    );
    assert_eq!(
        serde_json::to_value(&result.content).expect("content")[0]["resource"]["text"],
        text
    );
    let custom = json!([1,{"unknown":"preserved"}]);
    let decoded = accepted(
        &json!({"jsonrpc":"2.0","id":2,"result":custom}),
        Some("custom/method"),
    );
    assert!(matches!(decoded, JsonRpcMessage::Response(response)
        if matches!(&response.result, ServerResult::CustomResult(value) if value.0 == custom)));
}

#[test]
fn mrtr_and_task_results_remain_distinct_from_complete_tool_results() {
    for method in ["tools/call", "prompts/get", "resources/read"] {
        let result = json!({"resultType":"input_required","requestState":"opaque",
            "inputRequests":{"roots":{"method":"roots/list"}}});
        assert!(
            matches!(accepted(&json!({"jsonrpc":"2.0","id":1,"result":result}), Some(method)),
            JsonRpcMessage::Response(response) if matches!(&response.result, ServerResult::InputRequiredResult(_)))
        );
    }
    let result = json!({"resultType":"task","taskId":"task-1","status":"working",
        "createdAt":"2026-09-08T00:00:00Z","lastUpdatedAt":"2026-09-08T00:00:00Z","ttlMs":null});
    assert!(
        matches!(accepted(&json!({"jsonrpc":"2.0","id":1,"result":result}), Some("tools/call")),
        JsonRpcMessage::Response(response) if matches!(&response.result, ServerResult::CreateTaskResult(_)))
    );
}

#[test]
fn protocol_errors_notifications_and_numeric_string_ids_keep_external_semantics() {
    for id in [Value::Null, json!("0007"), json!(7)] {
        let wire = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32600,"message":"remote","data":{"opaque":true}}});
        let bytes = serde_json::to_vec(&wire).expect("wire");
        let route = classify(&bytes).expect("route");
        assert_eq!(route.kind, EnvelopeKind::Error);
        assert!(matches!(
            accepted(&wire, route.id.as_ref().map(|_| "tools/call")),
            JsonRpcMessage::Error(_)
        ));
        if let Some(id) = route.id {
            assert_eq!(id.numeric_value().expect("numeric"), Some(7));
            assert!(
                id.matches_request_id(&RequestId::Number(7))
                    .expect("identity")
            );
        }
    }
    let wire = json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
    assert!(
        matches!(accepted(&wire, None), JsonRpcMessage::Notification(notification)
        if matches!(notification.notification, ServerNotification::ToolListChangedNotification(_)))
    );
}

#[test]
fn structural_and_decoded_pressure_reject_before_typed_growth() {
    let deep = format!("{}0{}", "[".repeat(65), "]".repeat(65));
    assert!(classify(deep.as_bytes()).is_err());
    let many = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":[{}]}}",
        vec!["0"; 65_536].join(",")
    );
    assert!(classify(many.as_bytes()).is_err());
    // The raw input is legal-sized. Its decoded temporary/typed graph does not
    // fit one reservation; the callback must never see a fictitiously small plan.
    let result = json!({"structuredContent":vec![json!({"x":0}); 10_000]});
    let wire = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":result})).expect("wire");
    assert!(wire.len() < 4 * 1024 * 1024);
    let mut admitted = false;
    assert!(
        decode(&wire, Some("tools/call"), &mut |_| {
            admitted = true;
            Ok(())
        })
        .is_err()
    );
    assert!(!admitted);
}

#[test]
fn a_full_catalog_of_small_tools_fits_one_decode_reservation() {
    let tools: Vec<_> = (0..256)
        .map(|index| {
            json!({"name":format!("tool_{index}"),
        "description":"bounded tool","inputSchema":{"type":"object","properties":{}}})
        })
        .collect();
    let wire = json!({"jsonrpc":"2.0","id":1,"result":{"tools":tools}});
    assert!(
        matches!(accepted(&wire, Some("tools/list")), JsonRpcMessage::Response(response)
        if matches!(&response.result, ServerResult::ListToolsResult(result) if result.tools.len() == 256))
    );
}

#[test]
fn bootstrap_results_and_nested_elicitation_use_their_concrete_graphs() {
    let initialize = rmcp::model::InitializeResult::new(rmcp::model::ServerCapabilities::default());
    let discover =
        rmcp::model::DiscoverResult::new(vec![], rmcp::model::ServerCapabilities::default());
    for (method, result) in [
        (
            "initialize",
            serde_json::to_value(initialize).expect("initialize"),
        ),
        (
            "server/discover",
            serde_json::to_value(discover).expect("discover"),
        ),
    ] {
        let response = accepted(
            &json!({"jsonrpc":"2.0","id":0,"result":result}),
            Some(method),
        );
        assert!(matches!(response, JsonRpcMessage::Response(response)
            if matches!(&response.result, ServerResult::InitializeResult(_) | ServerResult::DiscoverResult(_))));
    }
    let result = json!({"resultType":"input_required","inputRequests":{"choice":{
        "method":"elicitation/create","params":{"mode":"form","message":"choose",
        "requestedSchema":{"type":"object","properties":{"choice":{"type":"string","enum":["a","b"]}},"required":["choice"]}}}}});
    let response = accepted(
        &json!({"jsonrpc":"2.0","id":8,"result":result}),
        Some("tools/call"),
    );
    assert!(matches!(response, JsonRpcMessage::Response(response)
        if matches!(&response.result, ServerResult::InputRequiredResult(result)
            if result.input_requests.as_ref().expect("requests").len() == 1)));
    let mut invalid = result;
    invalid["inputRequests"]["choice"]["params"]["requestedSchema"]["properties"]["choice"]["type"] =
        json!(false);
    let wire = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":8,"result":invalid})).expect("wire");
    let mut admitted = false;
    assert!(
        decode(&wire, Some("tools/call"), &mut |_| {
            admitted = true;
            Ok(())
        })
        .is_err()
    );
    assert!(
        admitted,
        "failed nested trial decoders also require prior admission"
    );
}

#[test]
fn borrowed_ids_preserve_exact_strings_and_reject_non_scalar_graphs() {
    let wire = br#"{"jsonrpc":"2.0","id":"\u0030\u0030\u0037","result":{}}"#;
    let route = classify(wire).expect("route");
    let id = route.id.expect("id");
    assert!(
        id.matches_request_id(&RequestId::Number(7))
            .expect("numeric equivalent")
    );
    assert!(
        id.matches_request_id(&RequestId::String("007".into()))
            .expect("exact")
    );
    assert!(
        !id.matches_request_id(&RequestId::String("7".into()))
            .expect("not equal")
    );
    let wire = br#"{"jsonrpc":"2.0","id":{"untrusted":[1,2,3]},"result":{}}"#;
    assert!(classify(wire).is_err());
    let id = "retained-wire-identity".repeat(8192);
    let wire = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"result":{}})).expect("wire");
    let route = classify(&wire).expect("no invented ID string cap");
    assert!(
        route
            .id
            .expect("id")
            .matches_request_id(&RequestId::String(id.into()))
            .expect("exact")
    );
}

#[test]
fn custom_result_type_fields_are_payload_data_not_protocol_selectors() {
    for custom in [
        json!({"resultType":null}),
        json!({"resultType":42,"data":[false,null]}),
        json!({"resultType":{"nested":"input_required"}}),
        json!({"resultType":"input_required","inputRequests":"arbitrary"}),
        json!({"resultType":"task","taskId":17}),
    ] {
        let decoded = accepted(
            &json!({"jsonrpc":"2.0","id":1,"result":custom}),
            Some("custom/method"),
        );
        assert!(matches!(decoded, JsonRpcMessage::Response(response)
            if matches!(&response.result, ServerResult::CustomResult(value) if value.0 == custom)));
    }
}
