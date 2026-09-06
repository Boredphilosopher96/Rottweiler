//! Invocation-bound HTTP effects remain owned through response and socket proof.
use super::*;
use futures_util::FutureExt as _;
use std::panic::AssertUnwindSafe;

pub(super) struct ActiveHttp {
    pub(super) invocation: RpcId,
    pub(super) cancellation: CancellationToken,
    pub(super) settled: Arc<AtomicBool>,
}

pub(super) fn has_unsettled(active: &ActiveProviderHttp, invocation: &RpcId) -> bool {
    active.lock().map_or(true, |active| {
        active
            .values()
            .any(|entry| entry.invocation == *invocation && !entry.settled.load(Ordering::Acquire))
    })
}

struct HttpLease {
    effect: Option<tokio::sync::OwnedSemaphorePermit>,
    operation: Option<Arc<dyn PluginProviderHttpOperation>>,
    termination: Arc<RequestTermination>,
    active: ActiveProviderHttp,
    id: RpcId,
    settled: Arc<AtomicBool>,
}
impl HttpLease {
    fn complete(mut self) {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.id);
        self.effect.take();
    }
}
impl Drop for HttpLease {
    fn drop(&mut self) {
        if let Some(permit) = self.effect.take() {
            // A panic, abandoned proof or failed retirement cannot release the
            // physical owner or advertise spare host effect capacity.
            std::mem::forget(permit);
            if let Some(operation) = self.operation.take() {
                std::mem::forget(operation);
            }
            self.termination.fail_host_proof();
        }
    }
}

pub(super) async fn start(request: RpcRequest, state: &ReaderState) -> bool {
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
    let stream_authorized = state.provider_streams.lock().is_ok_and(|streams| {
        streams
            .get(&capability.invocation_id)
            .is_some_and(|stream| {
                stream.alias == capability.alias
                    && stream.finished.is_none()
                    && tokio::time::Instant::now() < stream.deadline
            })
    });
    if !stream_authorized
        && !state
            .pending
            .lock()
            .await
            .get(&capability.invocation_id)
            .is_some_and(|request| request.owns_provider_http(&capability.alias))
    {
        return false;
    }
    let cancellation = CancellationToken::default();
    let settled = Arc::new(AtomicBool::new(false));
    let inserted = state.active_provider_http.lock().is_ok_and(|mut active| {
        if state.termination.cancellation.is_cancelled()
            || active.len() >= WRITER_QUEUE_CAPACITY
            || active.contains_key(&request.id)
        {
            return false;
        }
        active.insert(
            request.id.clone(),
            ActiveHttp {
                invocation: capability.invocation_id,
                cancellation: cancellation.clone(),
                settled: settled.clone(),
            },
        );
        true
    });
    if !inserted {
        return false;
    }
    let lease = HttpLease {
        effect: Some(effect),
        operation: None,
        termination: state.termination.clone(),
        active: state.active_provider_http.clone(),
        id: request.id,
        settled,
    };
    let handler = state.provider_http.clone();
    let writer = state.writer.clone();
    let redactor = state.redactor.clone();
    tokio::spawn(run(lease, handler, params, cancellation, writer, redactor));
    true
}

pub(super) fn cancel(active: &ActiveProviderHttp, params: Value) -> bool {
    let Ok(cancel) = serde_json::from_value::<ProviderHttpCancelParams>(params) else {
        return false;
    };
    let Ok(active) = active.lock() else {
        return false;
    };
    if let Some(entry) = active.get(&cancel.request_id) {
        entry.cancellation.cancel();
    }
    true
}

async fn run(
    mut lease: HttpLease,
    handler: Arc<dyn PluginProviderHttpHandler>,
    params: Value,
    cancellation: CancellationToken,
    writer: RpcWriter,
    redactor: Arc<dyn PluginBoundaryRedactor>,
) {
    let prepared = handler.prepare(params, cancellation.clone());
    let result = match prepared {
        Ok(operation) => {
            lease.operation = Some(operation.clone());
            let result = AssertUnwindSafe(async {
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => Err(rpc_error("cancelled", "provider HTTP was cancelled")),
                    result = stream(&lease.id, operation.as_ref(), &cancellation, &writer, redactor.as_ref()) => result,
                }
            }).catch_unwind().await;
            // The response and body futures have now ended, including on panic.
            cancellation.cancel();
            let proof = tokio::time::timeout(
                Duration::from_secs(5),
                AssertUnwindSafe(operation.settle_effects()).catch_unwind(),
            )
            .await;
            match proof {
                Ok(Ok(Ok(()))) => {}
                Err(_) => {
                    // The response deadline is final for this host instance, but
                    // slow DNS/runtime shutdown still has an owned observer.
                    lease.termination.fail_host_proof();
                    if matches!(
                        AssertUnwindSafe(operation.settle_effects())
                            .catch_unwind()
                            .await,
                        Ok(Ok(()))
                    ) {
                        lease.settled.store(true, Ordering::Release);
                        lease.complete();
                    }
                    return;
                }
                Ok(Err(_) | Ok(Err(_))) => return,
            }
            match result {
                Ok(result) => result,
                Err(_) => Err(rpc_error(
                    "provider_http_failed",
                    "provider HTTP response owner panicked",
                )),
            }
        }
        Err(error) if error.code == "effects_unsettled" => return,
        Err(error) => Err(error),
    };
    lease.settled.store(true, Ordering::Release);
    if result.is_ok()
        && incoming::send_provider_http_event(
            &writer,
            redactor.as_ref(),
            json!({"request_id":lease.id,"event":{"type":"finished"}}),
        )
        .await
        .is_err()
    {
        lease.termination.begin();
    }
    if writer
        .send(result_frame(lease.id.clone(), result))
        .await
        .is_err()
    {
        lease.termination.begin();
    }
    // Correlation remains charged until its terminal frame was admitted.
    lease.complete();
}

async fn stream(
    id: &RpcId,
    operation: &dyn PluginProviderHttpOperation,
    cancellation: &CancellationToken,
    writer: &RpcWriter,
    redactor: &dyn PluginBoundaryRedactor,
) -> Result<(), PluginRpcError> {
    let mut response = operation.response().await?;
    incoming::send_provider_http_event(
        writer,
        redactor,
        json!({"request_id":id,
        "event":{"type":"head","status":response.status,"headers":response.headers}}),
    )
    .await?;
    incoming::stream_provider_http_body(id, &mut response.body, cancellation, writer, redactor)
        .await
}

fn result_frame(id: RpcId, result: Result<(), PluginRpcError>) -> RpcFrame {
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
