use super::*;

async fn close_connection(state: &ReaderState, reason: PluginRpcError) {
    cancel_active_provider_http(&state.active_provider_http);
    state.termination.begin();
    let reason = state.termination.wait().await.err().unwrap_or(reason);
    fail_pending(&state.pending, reason.clone()).await;
    fail_provider_streams(&state.provider_streams, &reason);
}

pub(super) async fn reader_loop(mut stdout: PluginStdout, state: ReaderState) {
    let mut buffer = [0_u8; 8192];
    let mut decoder = FrameDecoder::default();
    loop {
        let count = match tokio::io::AsyncReadExt::read(&mut stdout, &mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let Ok(frames) = decoder.push(&buffer[..count]) else {
            close_connection(
                &state,
                rpc_error(
                    "invalid_frame",
                    "plugin emitted an invalid or oversized frame",
                ),
            )
            .await;
            return;
        };
        for frame in frames {
            if !process_incoming_frame(frame.frame, frame.wire_bytes, &state).await {
                close_connection(
                    &state,
                    rpc_error(
                        "protocol_violation",
                        "plugin RPC stream violated correlation or capabilities",
                    ),
                )
                .await;
                return;
            }
        }
    }
    close_connection(
        &state,
        rpc_error("connection_closed", "plugin RPC connection closed"),
    )
    .await;
}

pub(super) fn cancel_active_provider_http(active: &ActiveProviderHttp) {
    if let Ok(mut active) = active.lock() {
        for (_, cancellation) in std::mem::take(&mut *active) {
            cancellation.cancel();
        }
    }
}

pub(super) async fn terminate_and_reap(process: &dyn SupervisedPluginProcess) {
    let _ = process.kill_tree();
    let _ = tokio::time::timeout(DEFAULT_SHUTDOWN_TIMEOUT, process.reap()).await;
}

#[allow(clippy::too_many_lines)]
async fn process_incoming_frame(frame: RpcFrame, wire_bytes: usize, state: &ReaderState) -> bool {
    if state.termination.cancellation.is_cancelled() {
        return false;
    }
    match frame {
        RpcFrame::Success(success) => {
            let id = success.id;
            let provider = state
                .provider_streams
                .lock()
                .ok()
                .and_then(|mut streams| streams.remove(&id));
            if let Some(provider) = provider {
                provider.credit.closed.cancel();
                let Some(finished) = provider.finished.filter(|_| success.result.is_null()) else {
                    state.termination.begin();
                    let _ = provider.terminal.send(Some(Err(rpc_error(
                        "invalid_provider_stream",
                        "plugin provider stream ended without one terminal finished event",
                    ))));
                    return false;
                };
                let _ = provider.terminal.send(Some(Ok(finished)));
                return true;
            }
            if let Some(sender) = state.pending.lock().await.remove(&id) {
                sender.respond(Ok(success.result));
                true
            } else {
                let _ = state.process.kill_tree();
                false
            }
        }
        RpcFrame::Failure(failure) => {
            let Some(id) = failure.id else {
                let _ = state.process.kill_tree();
                return false;
            };
            let safe_code = failure
                .error
                .data
                .as_ref()
                .and_then(|data| data.get("code"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            let safe_code = if safe_code.is_empty() {
                failure.error.code.to_string()
            } else {
                safe_code
            };
            if matches!(failure.error.code, -32004 | -32800) {
                state.termination.begin();
                if let Err(error) = state.termination.wait().await {
                    fail_pending(&state.pending, error.clone()).await;
                    fail_provider_streams(&state.provider_streams, &error);
                    return false;
                }
            }
            let provider = state
                .provider_streams
                .lock()
                .ok()
                .and_then(|mut streams| streams.remove(&id));
            if let Some(provider) = provider {
                provider.credit.closed.cancel();
                let _ = provider.terminal.send(Some(Err(PluginRpcError {
                    code: safe_code.clone(),
                    message: failure.error.message,
                })));
                return true;
            }
            if let Some(sender) = state.pending.lock().await.remove(&id) {
                sender.respond(Err(PluginRpcError {
                    code: safe_code,
                    message: failure.error.message,
                }));
                true
            } else {
                let _ = state.process.kill_tree();
                false
            }
        }
        RpcFrame::Request(request) => {
            if request.method == METHOD_PROVIDER_HTTP {
                return start_provider_http_request(request, state);
            }
            start_host_command(request, state)
        }
        RpcFrame::Notification(notification) => {
            if notification.method == METHOD_TOOL_PROGRESS {
                if wire_bytes > rw_operation_contract::MAX_PROGRESS_FRAME_BYTES {
                    return false;
                }
                let Ok(params) = serde_json::from_value::<ToolProgressParams>(
                    state
                        .redactor
                        .redact(notification.params.unwrap_or(Value::Null)),
                ) else {
                    return false;
                };
                let request_id = params.request_id.clone();
                return state
                    .pending
                    .lock()
                    .await
                    .get_mut(&request_id)
                    .is_some_and(|request| request.progress(params));
            }
            if notification.method == METHOD_PROVIDER_HTTP_CANCEL {
                return cancel_provider_http_request(
                    &state.active_provider_http,
                    notification.params.unwrap_or(Value::Null),
                );
            }
            if notification.method == METHOD_PROVIDER_EVENT {
                return handle_provider_event(
                    &state.provider_streams,
                    notification.params.unwrap_or(Value::Null),
                    wire_bytes,
                );
            }
            // Mutating host capabilities require a correlated outcome.
            false
        }
    }
}

struct HostCommandLease {
    effect: Option<tokio::sync::OwnedSemaphorePermit>,
    termination: Arc<RequestTermination>,
    active: Arc<StdMutex<BTreeSet<RpcId>>>,
    id: RpcId,
}

impl HostCommandLease {
    fn complete(mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.id);
        }
        self.effect.take();
    }
}

impl Drop for HostCommandLease {
    fn drop(&mut self) {
        if let Some(effect) = self.effect.take() {
            // Destruction after a panic is not an actor outcome. Keep the barrier
            // charged permanently: queued host work may still commit later.
            std::mem::forget(effect);
            self.termination.begin();
            tracing::error!("host command owner disappeared without proving settlement");
        }
    }
}

fn start_host_command(request: RpcRequest, state: &ReaderState) -> bool {
    if state.enforcer.check_push_method(&request.method).is_err() {
        return false;
    }
    let Ok(mut active) = state.host_commands.lock() else {
        return false;
    };
    if active.len() >= usize::from(RPC_REQUEST_CAPACITY) || active.contains(&request.id) {
        return false;
    }
    let Ok(effect) = Arc::clone(&state.termination.host_effects).try_acquire_owned() else {
        return false;
    };
    active.insert(request.id.clone());
    drop(active);
    let lease = HostCommandLease {
        effect: Some(effect),
        termination: Arc::clone(&state.termination),
        active: Arc::clone(&state.host_commands),
        id: request.id.clone(),
    };
    let handler = Arc::clone(&state.push_handler);
    let enforcer = Arc::clone(&state.enforcer);
    let redactor = Arc::clone(&state.redactor);
    let writer = state.writer.clone();
    let termination = Arc::clone(&state.termination);
    let pending = Arc::clone(&state.pending);
    tokio::spawn(async move {
        // Keep this permit through the actual actor reply, even after teardown starts.
        let params = redactor.redact(request.params.unwrap_or(Value::Null));
        let response = match validate_control_origin(&pending, &request.method, &params).await {
            Ok(()) => {
                handle_push_request(&enforcer, handler.as_ref(), &request.method, params).await
            }
            Err(error) => Err(error),
        };
        if termination.cancellation.is_cancelled() {
            lease.complete();
            return;
        }
        let response = match response {
            Ok(result) => RpcFrame::Success(RpcSuccess {
                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                id: request.id,
                result: redactor.redact(result),
            }),
            Err(error) => RpcFrame::Failure(RpcFailure {
                jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
                id: Some(request.id),
                error: rw_plugin_protocol::RpcErrorObject {
                    code: -32000,
                    message: error.message,
                    data: Some(json!({"code":error.code})),
                },
            }),
        };
        if !tokio::time::timeout(DEFAULT_REQUEST_TIMEOUT, writer.send(response))
            .await
            .is_ok_and(|result| result.is_ok())
            || enforcer.violated()
        {
            termination.begin();
        }
        lease.complete();
    });
    true
}

pub(super) async fn validate_control_origin(
    pending: &Pending,
    method: &str,
    params: &Value,
) -> Result<(), PluginRpcError> {
    let origin = match method {
        rw_plugin_protocol::METHOD_SESSION_CONTROL => {
            let request: rw_types::extension_invocation::ExtensionControlRequest =
                serde_json::from_value(params.clone())
                    .map_err(|_| rpc_error("invalid_params", "invalid session control request"))?;
            let Some(origin) = request.origin else {
                return Ok(());
            };
            origin
        }
        rw_plugin_protocol::METHOD_SESSION_TOOL_CALL => {
            let request: rw_types::extension_tools::ExtensionToolCall =
                serde_json::from_value(params.clone())
                    .map_err(|_| rpc_error("invalid_params", "invalid session tool request"))?;
            request
                .validate()
                .map_err(|message| rpc_error("invalid_params", message))?;
            request.origin
        }
        _ => return Ok(()),
    };
    if pending
        .lock()
        .await
        .values()
        .any(|request| request.owns_origin(&origin))
    {
        return Ok(());
    }
    Err(rpc_error(
        "invalid_origin",
        "session control does not belong to an active command in this process",
    ))
}

fn start_provider_http_request(request: RpcRequest, state: &ReaderState) -> bool {
    let Ok(effect) = Arc::clone(&state.termination.host_effects).try_acquire_owned() else {
        return false;
    };
    let params = request.params.unwrap_or(Value::Null);
    let Ok(capability) = serde_json::from_value::<ProviderHttpCapabilityParams>(params.clone())
    else {
        return false;
    };
    if state
        .enforcer
        .check_provider_credential(&capability.alias, &capability.credential_reference)
        .is_err()
    {
        return false;
    }
    let _ = capability.request;
    let cancellation = CancellationToken::default();
    let inserted = state.active_provider_http.lock().is_ok_and(|mut active| {
        if state.termination.cancellation.is_cancelled()
            || active.len() >= WRITER_QUEUE_CAPACITY
            || active.contains_key(&request.id)
        {
            false
        } else {
            active.insert(request.id.clone(), cancellation.clone());
            true
        }
    });
    if !inserted {
        return false;
    }
    let handler = Arc::clone(&state.provider_http);
    let writer = state.writer.clone();
    let active = Arc::clone(&state.active_provider_http);
    let redactor = Arc::clone(&state.redactor);
    let termination = Arc::clone(&state.termination);
    tokio::spawn(async move {
        let _effect = effect;
        let id = request.id.clone();
        let cancel_writer = writer.clone();
        let cancelled = tokio::select! {
            biased;
            () = cancellation.cancelled() => true,
            () = stream_provider_http_response(
            request.id,
            params,
            cancellation.clone(),
            handler,
            writer,
            Arc::clone(&active),
            redactor,
        ) => false,
        };
        if let Ok(mut active) = active.lock() {
            active.remove(&id);
        }
        if cancelled && !termination.cancellation.is_cancelled() {
            let result = provider_http_result_frame(
                id,
                Err(rpc_error("cancelled", "provider HTTP was cancelled")),
            );
            if cancel_writer.try_send(result).is_err() {
                termination.begin();
            }
        }
    });
    true
}

fn cancel_provider_http_request(active: &ActiveProviderHttp, params: Value) -> bool {
    let Ok(cancel) = serde_json::from_value::<ProviderHttpCancelParams>(params) else {
        return false;
    };
    let Ok(active) = active.lock() else {
        return false;
    };
    if let Some(token) = active.get(&cancel.request_id) {
        token.cancel();
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn stream_provider_http_response(
    id: RpcId,
    params: Value,
    cancellation: CancellationToken,
    handler: Arc<dyn PluginProviderHttpHandler>,
    writer: RpcWriter,
    active: ActiveProviderHttp,
    redactor: Arc<dyn PluginBoundaryRedactor>,
) {
    let result = handler.request(params, &cancellation).await;
    let result = match result {
        Ok(mut response) => {
            let head = json!({
                "request_id": id,
                "event": {
                    "type": "head",
                    "status": response.status,
                    "headers": response.headers,
                }
            });
            if send_provider_http_event(&writer, redactor.as_ref(), head)
                .await
                .is_err()
            {
                cancellation.cancel();
                Err(rpc_error(
                    "connection_closed",
                    "plugin RPC connection closed",
                ))
            } else {
                stream_provider_http_body(
                    &id,
                    &mut response.body,
                    &cancellation,
                    &writer,
                    redactor.as_ref(),
                )
                .await
            }
        }
        Err(error) => Err(error),
    };
    if let Ok(mut active) = active.lock() {
        active.remove(&id);
    }
    let frame = provider_http_result_frame(id, result);
    let _ = writer.send(frame).await;
}

fn provider_http_result_frame(id: RpcId, result: Result<(), PluginRpcError>) -> RpcFrame {
    match result {
        Ok(()) => RpcFrame::Success(RpcSuccess {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id,
            result: Value::Null,
        }),
        Err(error) => RpcFrame::Failure(RpcFailure {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            id: Some(id),
            error: rw_plugin_protocol::RpcErrorObject {
                code: -32020,
                message: error.message,
                data: Some(json!({"code":error.code})),
            },
        }),
    }
}

pub(super) async fn stream_provider_http_body(
    id: &RpcId,
    body: &mut PluginHttpByteStream,
    cancellation: &CancellationToken,
    writer: &RpcWriter,
    redactor: &dyn PluginBoundaryRedactor,
) -> Result<(), PluginRpcError> {
    let overlap = redactor.maximum_secret_bytes().saturating_sub(1);
    let mut pending = Vec::new();
    loop {
        let next = tokio::select! {
            () = cancellation.cancelled() => {
                return Err(rpc_error("cancelled", "host-mediated provider HTTP was cancelled"));
            }
            next = body.next() => next,
        };
        let Some(chunk) = next else {
            break;
        };
        pending.extend_from_slice(&chunk?);
        if pending.len() <= overlap {
            continue;
        }
        let (bytes, tail) = redactor.redact_streaming_prefix(&pending, overlap);
        pending = tail;
        if bytes.is_empty() {
            continue;
        }
        send_provider_http_event(
            writer,
            redactor,
            json!({
                "request_id": id,
                "event": {"type":"body","data_base64":BASE64_STANDARD.encode(bytes)},
            }),
        )
        .await?;
    }
    if !pending.is_empty() {
        let bytes = redactor.redact_bytes(&pending);
        send_provider_http_event(
            writer,
            redactor,
            json!({
                "request_id": id,
                "event": {"type":"body","data_base64":BASE64_STANDARD.encode(bytes)},
            }),
        )
        .await?;
    }
    send_provider_http_event(
        writer,
        redactor,
        json!({"request_id":id,"event":{"type":"finished"}}),
    )
    .await
}

async fn send_provider_http_event(
    writer: &RpcWriter,
    redactor: &dyn PluginBoundaryRedactor,
    params: Value,
) -> Result<(), PluginRpcError> {
    writer
        .send_data(RpcFrame::Notification(RpcNotification {
            jsonrpc: rw_plugin_protocol::JSON_RPC_VERSION.to_owned(),
            method: METHOD_PROVIDER_HTTP_EVENT.to_owned(),
            params: Some(redactor.redact(params)),
        }))
        .await
        .map_err(|()| rpc_error("connection_closed", "plugin RPC connection closed"))
}

fn handle_provider_event(streams: &PendingProviderStreams, params: Value, bytes: usize) -> bool {
    let Ok(notification) = serde_json::from_value::<ProviderEventParams>(params) else {
        return false;
    };
    let Ok(event) = serde_json::from_value::<ProviderEvent>(notification.event.clone()) else {
        return false;
    };
    let finished = matches!(event, ProviderEvent::Finished { .. });
    let delivered = {
        let Ok(mut streams) = streams.lock() else {
            return false;
        };
        streams.get_mut(&notification.request_id).map(|stream| {
            if stream.finished.is_some() {
                return false;
            }
            if finished {
                // Terminal storage is reserved outside data credit. Canonicalize
                // this bounded enum rather than retaining arbitrary extra fields.
                stream.finished = serde_json::to_value(event).ok();
                return stream.finished.is_some();
            }
            if stream.remaining_credit.0 == 0 || stream.remaining_credit.1 < bytes {
                return false;
            }
            stream.remaining_credit.0 -= 1;
            stream.remaining_credit.1 -= bytes;
            let queued = stream.queued_bytes.fetch_add(bytes, Ordering::AcqRel);
            if queued.saturating_add(bytes) > PROVIDER_WINDOW_BYTES {
                stream.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                return false;
            }
            if stream.sender.try_send((notification.event, bytes)).is_err() {
                stream.queued_bytes.fetch_sub(bytes, Ordering::AcqRel);
                return false;
            }
            true
        })
    };
    delivered.unwrap_or_default()
}

async fn handle_push_request(
    enforcer: &CapabilityEnforcer,
    handler: &dyn PushHandler,
    method: &str,
    params: Value,
) -> Result<Value, PluginRpcError> {
    enforcer
        .check_push_method(method)
        .map_err(|error| rpc_error("capability_violation", &error.to_string()))?;
    validate_push_params(method, &params)?;
    handler.handle_push(method, params).await
}

pub(super) fn validate_push_params(method: &str, params: &Value) -> Result<(), PluginRpcError> {
    let object = params
        .as_object()
        .ok_or_else(|| rpc_error("invalid_push", "plugin push parameters must be an object"))?;
    if method == rw_plugin_protocol::METHOD_UI_PUBLISH_PANEL {
        let update: rw_types::extension_ui::UiPanelUpdate = serde_json::from_value(params.clone())
            .map_err(|_| rpc_error("invalid_push", "invalid panel update"))?;
        return update
            .validate()
            .map_err(|error| rpc_error("invalid_push", &error.to_string()));
    }
    if method == METHOD_SESSION_CONTROL {
        let request: rw_types::extension_invocation::ExtensionControlRequest =
            serde_json::from_value(params.clone())
                .map_err(|_| rpc_error("invalid_push", "invalid session control"))?;
        return request
            .control
            .validate()
            .map_err(|error| rpc_error("invalid_push", error));
    }
    if method == METHOD_SESSION_CONTEXT_READ {
        let request: rw_types::extension_control::ExtensionContextRead =
            serde_json::from_value(params.clone())
                .map_err(|_| rpc_error("invalid_push", "invalid context read"))?;
        return request
            .after_item_id
            .as_ref()
            .map(|id| rw_types::extension_control::validate_context_item_id(&id.0))
            .transpose()
            .map(|_| ())
            .map_err(|error| rpc_error("invalid_push", error));
    }
    if method == METHOD_EVENT_READ {
        let read: rw_plugin_protocol::ExtensionEventRead =
            serde_json::from_value(params.clone())
                .map_err(|_| rpc_error("invalid_event_read", "invalid event read parameters"))?;
        if read.max_bytes == 0
            || read.max_bytes > rw_types::extension_events::MAX_EXTENSION_EVENT_CHUNK_BYTES
        {
            return Err(rpc_error(
                "invalid_event_read",
                "event read size exceeds limit",
            ));
        }
        return Ok(());
    }
    if method == METHOD_EXTENSION_STATE_COMMIT {
        let transaction: rw_types::extension_contract::ExtensionStateTransaction =
            serde_json::from_value(params.clone())
                .map_err(|_| rpc_error("invalid_push", "invalid extension state transaction"))?;
        if transaction.acknowledged.is_some() {
            return Err(rpc_error(
                "invalid_push",
                "delivery acknowledgements belong to event delivery",
            ));
        }
        return rw_types::extension_contract::validate_state_transaction(&transaction)
            .map_err(|error| rpc_error("invalid_push", &error.to_string()));
    }
    validate_text_push(method, object)
}

fn validate_text_push(
    method: &str,
    object: &serde_json::Map<String, Value>,
) -> Result<(), PluginRpcError> {
    let (allowed, fields): (&[&str], &[(&str, usize)]) = match method {
        METHOD_SESSION_QUERY | METHOD_EXTENSION_STATE_READ => (&[], &[]),
        METHOD_SESSION_INJECT_MESSAGE => (
            &["session_id", "content"],
            &[
                ("session_id", MAX_NAME_BYTES),
                ("content", MAX_HOOK_PAYLOAD_BYTES),
            ],
        ),
        METHOD_SESSION_SET_STATUS => (
            &["session_id", "status"],
            &[
                ("session_id", MAX_NAME_BYTES),
                ("status", MAX_RPC_MESSAGE_BYTES),
            ],
        ),
        METHOD_UI_NOTIFY => (
            &["title", "message", "session_id"],
            &[
                ("title", MAX_NAME_BYTES),
                ("message", MAX_RPC_MESSAGE_BYTES),
            ],
        ),
        _ => return Err(rpc_error("invalid_push", "plugin push method is unknown")),
    };
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(rpc_error(
            "invalid_push",
            "plugin push contains unknown fields",
        ));
    }
    for (field, limit) in fields {
        let value = object
            .get(*field)
            .and_then(Value::as_str)
            .ok_or_else(|| rpc_error("invalid_push", "plugin push contains an invalid field"))?;
        if value.is_empty() || value.len() > *limit || value.chars().any(char::is_control) {
            return Err(rpc_error(
                "invalid_push",
                "plugin push field exceeds its bounds",
            ));
        }
    }
    if let Some(session_id) = object.get("session_id") {
        let session_id = session_id
            .as_str()
            .ok_or_else(|| rpc_error("invalid_push", "plugin push session id is invalid"))?;
        if rw_types::SessionId::validate(session_id).is_err() {
            return Err(rpc_error(
                "invalid_push",
                "plugin push session id is invalid",
            ));
        }
    }
    Ok(())
}

pub(super) async fn fail_pending(pending: &Pending, error: PluginRpcError) {
    for (_, sender) in std::mem::take(&mut *pending.lock().await) {
        sender.respond(Err(error.clone()));
    }
}

pub(super) fn fail_provider_streams(streams: &PendingProviderStreams, error: &PluginRpcError) {
    let Ok(mut streams) = streams.lock() else {
        return;
    };
    for (_, stream) in std::mem::take(&mut *streams) {
        stream.credit.closed.cancel();
        let _ = stream.terminal.send(Some(Err(error.clone())));
    }
}

pub(super) async fn drain_stderr(mut stderr: PluginStdout) {
    let mut buffer = [0u8; 4096];
    loop {
        match tokio::io::AsyncReadExt::read(&mut stderr, &mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}
