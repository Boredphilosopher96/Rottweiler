//! Transports may disconnect; admitted request effects remain owned until completion.
use super::*;
use tokio::{sync::OwnedSemaphorePermit, task::JoinSet};

struct Requests {
    tasks: Mutex<JoinSet<()>>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Default for Requests {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(JoinSet::new()),
            permits: Arc::new(tokio::sync::Semaphore::new(128)),
        }
    }
}

impl Requests {
    fn start(
        &self,
        request: Request<Incoming>,
        state: ServerState,
        shutdown: Arc<AtomicBool>,
        permit: Arc<OwnedSemaphorePermit>,
    ) -> tokio::sync::oneshot::Receiver<Response<HttpBody>> {
        let (send, receive) = tokio::sync::oneshot::channel();
        let Ok(request_permit) = Arc::clone(&self.permits).try_acquire_owned() else {
            let _ = send.send(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "engine request admission exhausted",
            ));
            return receive;
        };
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Both request execution and retained task bookkeeping are bounded.
        // A request keeps its connection slot even after that transport closes.
        while let Some(result) = tasks.try_join_next() {
            report_request(result);
        }
        tasks.spawn(async move {
            let _permit = (permit, request_permit);
            let Ok(response) = handle_request(request, state, shutdown).await;
            let _ = send.send(response);
        });
        receive
    }

    async fn settle(&self) {
        let mut tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        while let Some(result) = tasks.join_next().await {
            report_request(result);
        }
    }
}

fn report_request(result: std::result::Result<(), tokio::task::JoinError>) {
    if let Err(error) = result {
        tracing::error!(reason = %error, "engine request failed while owned");
    }
}

pub(super) async fn serve_owned(
    listener: std::os::unix::net::UnixListener,
    state: ServerState,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    mut stop: crate::tui_session::Stop,
) -> Result<()> {
    let listener = UnixListener::from_std(listener).into_diagnostic()?;
    let requests = Arc::new(Requests::default());
    let mut connections = JoinSet::new();
    let result = loop {
        if stop.requested() || *shutdown.borrow() {
            break Ok(());
        }
        tokio::select! {
            () = stop.cancelled() => break Ok(()),
            () = state.shutdown_notifier.notified() => break Ok(()),
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break Ok(());
                }
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(error).into_diagnostic(),
                };
                let Ok(permit) = state.connections.clone().try_acquire_owned() else { continue; };
                let permit = Arc::new(permit);
                let connection_state = state.clone();
                let requests = Arc::clone(&requests);
                connections.spawn(async move {
                    let shutdown_state = connection_state.clone();
                    let connection_shutdown = Arc::new(AtomicBool::new(false));
                    let request_shutdown = Arc::clone(&connection_shutdown);
                    let service = service_fn(move |request| {
                        let response = requests.start(request, connection_state.clone(),
                            Arc::clone(&request_shutdown), Arc::clone(&permit));
                        async move {
                            Ok::<_, Infallible>(response.await.unwrap_or_else(|_| {
                                error_response(StatusCode::INTERNAL_SERVER_ERROR, "engine request failed")
                            }))
                        }
                    });
                    if let Err(error) = http1::Builder::new()
                        .keep_alive(true)
                        .max_buf_size(16 * 1024)
                        .timer(hyper_util::rt::TokioTimer::new())
                        .header_read_timeout(REQUEST_BODY_TIMEOUT)
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        tracing::debug!(reason = %error, "engine client connection closed");
                    }
                    if connection_shutdown.load(Ordering::Acquire) {
                        shutdown_state.shutdown_notifier.notify_one();
                    }
                });
            }
        }
    };
    drop(listener);
    // Closing HTTP/SSE releases their buffers and request waiters, while the
    // separate request owners retain admitted dispatch and credential effects.
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    requests.settle().await;
    result
}

pub(super) fn forward_events(
    mut source: mpsc::Receiver<std::result::Result<rw_core::HostEvent, rw_core::HostError>>,
) -> mpsc::Receiver<std::result::Result<rw_core::HostEvent, String>> {
    let (send, receive) = mpsc::channel(HOST_EVENT_FORWARD_CAPACITY);
    tokio::spawn(async move {
        loop {
            let event = tokio::select! {
                () = send.closed() => return,
                event = source.recv() => match event {
                    Some(event) => event,
                    None => return,
                },
            };
            if send
                .send(event.map_err(|error| error.to_string()))
                .await
                .is_err()
            {
                return;
            }
        }
    });
    receive
}

#[cfg(test)]
mod tests;
