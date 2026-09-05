use super::*;

#[test]
fn state_requests_are_host_bound_and_cannot_forge_delivery() {
    use super::super::incoming::validate_push_params;
    for method in [METHOD_SESSION_QUERY, METHOD_EXTENSION_STATE_READ] {
        assert!(validate_push_params(method, &json!({})).is_ok());
        for params in [
            json!({"session_id":"other"}),
            json!({"plugin_id":"other"}),
            Value::Null,
        ] {
            assert!(validate_push_params(method, &params).is_err());
        }
    }
    let write = json!({
        "expected_revision": null,
        "mutations": [{"action":"set","key":"a","value":1}],
        "acknowledged": null,
    });
    assert!(validate_push_params(METHOD_EXTENSION_STATE_COMMIT, &write).is_ok());
    let mut forged = write.clone();
    forged["acknowledged"] = json!({"session_id":"s","sequence":"4"});
    assert!(validate_push_params(METHOD_EXTENSION_STATE_COMMIT, &forged).is_err());
    let mut duplicate = write.clone();
    duplicate["mutations"] = json!([
        {"action":"set","key":"a","value":1},
        {"action":"delete","key":"a"}
    ]);
    assert!(validate_push_params(METHOD_EXTENSION_STATE_COMMIT, &duplicate).is_err());
    let mut oversized = write;
    oversized["mutations"][0]["value"] = json!("x".repeat(16 * 1024));
    assert!(validate_push_params(METHOD_EXTENSION_STATE_COMMIT, &oversized).is_err());
}

#[test]
fn state_pushes_require_individual_manifest_authority() {
    let mut declared = manifest();
    declared.capabilities.push = vec![rw_plugin_protocol::PluginPush::ExtensionStateRead];
    let process = Arc::new(FakeProcess::default());
    let enforcer = CapabilityEnforcer::new(&declared, process.clone());
    assert!(
        enforcer
            .check_push_method(METHOD_EXTENSION_STATE_READ)
            .is_ok()
    );
    assert!(
        enforcer
            .check_push_method(METHOD_EXTENSION_STATE_COMMIT)
            .is_err()
    );
}

#[test]
fn typed_session_controls_reject_implicit_actions_and_foreign_authority() {
    use super::super::incoming::validate_push_params;
    assert!(
        validate_push_params(
            METHOD_SESSION_CONTROL,
            &json!({"origin":null,"control":{"action":"select_model","model":"fast","provider":null}})
        )
        .is_ok()
    );
    for value in [
        json!({"model":"fast","provider":null}),
        json!({"action":"select_model","model":"fast"}),
        json!({"action":"select_mode","mode":"plan","session_id":"other"}),
        json!({"action":"pin_context","item_id":"x".repeat(rw_types::extension_control::MAX_CONTEXT_ITEM_ID_BYTES + 1)}),
    ] {
        assert!(validate_push_params(METHOD_SESSION_CONTROL, &value).is_err());
    }
    assert!(
        validate_push_params(
            METHOD_SESSION_CONTEXT_READ,
            &json!({"expected_sequence":null,"after_item_id":null})
        )
        .is_ok()
    );
    assert!(validate_push_params(METHOD_SESSION_CONTEXT_READ, &json!({})).is_err());
}

#[tokio::test]
async fn control_origin_is_bound_to_active_outbound_command_in_this_process() {
    use super::super::incoming::validate_control_origin;
    let origin = rw_types::extension_invocation::ExtensionInvocationId::from_bytes([1; 16]);
    let other = rw_types::extension_invocation::ExtensionInvocationId::from_bytes([2; 16]);
    let pending: Pending = Arc::new(Mutex::new(BTreeMap::new()));
    let (send, _receive) = oneshot::channel();
    let (mut request, _observer) = RequestPolicy::Ordinary {
        allow_closed: false,
    }
    .begin(send, DEFAULT_REQUEST_TIMEOUT);
    request
        .bind_command(
            METHOD_COMMAND_EXECUTE,
            &json!({"name":"command", "arguments":"", "invocation_id":origin,"lifetime":{"total_ms":300_000,"idle_ms":300_000}}),
        )
        .expect("command admission");
    let params = json!({"origin":origin,"control":{"action":"select_mode","mode":"plan"}});
    assert!(
        validate_control_origin(&pending, METHOD_SESSION_CONTROL, &params)
            .await
            .is_err()
    );
    pending.lock().await.insert(RpcId::Number(1), request);
    assert!(
        validate_control_origin(&pending, METHOD_SESSION_CONTROL, &params)
            .await
            .is_ok()
    );
    let mut foreign = params.clone();
    foreign["origin"] = json!(other);
    assert!(
        validate_control_origin(&pending, METHOD_SESSION_CONTROL, &foreign)
            .await
            .is_err()
    );
    pending.lock().await.remove(&RpcId::Number(1));
    assert!(
        validate_control_origin(&pending, METHOD_SESSION_CONTROL, &params)
            .await
            .is_err()
    );
}
