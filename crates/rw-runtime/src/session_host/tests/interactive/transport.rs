//! Bounded test HTTP framing; all command outcomes and event bytes come from EngineHost.
use http_body_util::{BodyExt as _, Full, Limited, StreamBody, combinators::UnsyncBoxBody};
use hyper::{
    Request, Response, StatusCode,
    body::{Bytes, Frame, Incoming},
};
use hyper_util::rt::TokioIo;
use rw_core::{BoundClient, ClientCommand, ClientId, EngineHost, SequenceId, SessionId};
use std::{
    convert::Infallible,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    net::UnixListener,
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
};

pub(super) const CLIENT: &str = "joined-client";
pub(super) const BOOTSTRAP: &str =
    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const MAX_CONNECTIONS: usize = 16;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
type Body = UnsyncBoxBody<Bytes, Infallible>;
#[derive(Default)]
pub(super) struct Counts {
    pub commands: AtomicU64,
    pub events: AtomicU64,
    pub event_bytes: AtomicU64,
}
#[derive(Clone)]
struct State {
    host: EngineHost,
    controls: mpsc::Sender<String>,
    counts: Arc<Counts>,
}
pub(super) struct Transport {
    pub controls: mpsc::Receiver<String>,
    pub counts: Arc<Counts>,
    stopped: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}
impl Transport {
    pub async fn start(path: &Path, host: EngineHost) -> Self {
        let listener = UnixListener::bind(path).expect("bind isolated HTTP adapter");
        let (controls, receive) = mpsc::channel(8);
        let counts = Arc::new(Counts::default());
        let state = State {
            host,
            controls,
            counts: Arc::clone(&counts),
        };
        let (stopped, mut stop) = watch::channel(false);
        let task = tokio::spawn(async move {
            let admission = Arc::new(Semaphore::new(MAX_CONNECTIONS));
            let mut sockets = JoinSet::new();
            loop {
                tokio::select! {
                    result = sockets.join_next(), if !sockets.is_empty() => { result.expect("connection").expect("HTTP worker"); }
                    result = stop.changed() => { result.expect("transport shutdown sender"); break; }
                    incoming = listener.accept() => {
                        let (socket, _) = incoming.expect("accept fixture socket");
                        let Ok(permit) = Arc::clone(&admission).try_acquire_owned() else { drop(socket); continue; };
                        let state = state.clone();
                        let mut stop = stop.clone();
                        sockets.spawn(async move {
                            let _permit = permit;
                            let connection = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(socket),
                                hyper::service::service_fn(move |request| dispatch(request, state.clone())));
                            tokio::pin!(connection);
                            tokio::select! {
                                _ = &mut connection => {}
                                _ = stop.changed() => { /* Drop this physical socket; EngineHost retains admitted effects. */ }
                            }
                        });
                    }
                }
            }
            drop(listener);
            while let Some(result) = sockets.join_next().await {
                result.expect("retired HTTP worker");
            }
            assert_eq!(admission.available_permits(), MAX_CONNECTIONS);
        });
        Self {
            controls: receive,
            counts,
            stopped,
            task,
        }
    }
    pub async fn close(self) {
        self.stopped.send_replace(true);
        self.task.await.expect("all fixture HTTP sockets retired");
    }
}
fn response(status: StatusCode, bytes: impl Into<Bytes>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(bytes.into()).boxed_unsync())
        .expect("response")
}
async fn dispatch(request: Request<Incoming>, state: State) -> Result<Response<Body>, Infallible> {
    let path = request.uri().path().to_owned();
    let bearer = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok());
    if path == "/v1/connect" {
        return Ok(
            if request.method() == hyper::Method::POST
                && bearer == Some(&format!("Bearer {BOOTSTRAP}"))
            {
                response(
                    StatusCode::OK,
                    format!("{{\"client_id\":\"{CLIENT}\",\"token\":\"{TOKEN}\"}}"),
                )
            } else {
                response(StatusCode::UNAUTHORIZED, "{}")
            },
        );
    }
    if request.method() == hyper::Method::POST
        && matches!(
            path.as_str(),
            "/fixture/stall" | "/fixture/release" | "/fixture/done"
        )
    {
        return Ok(if bearer != Some(&format!("Bearer {BOOTSTRAP}")) {
            response(StatusCode::UNAUTHORIZED, "{}")
        } else if state.controls.try_send(path).is_ok() {
            response(StatusCode::ACCEPTED, "{}")
        } else {
            response(StatusCode::SERVICE_UNAVAILABLE, "{}")
        });
    }
    if bearer != Some(&format!("Bearer {TOKEN}"))
        || request
            .headers()
            .get("x-rottweiler-client")
            .and_then(|h| h.to_str().ok())
            != Some(CLIENT)
    {
        return Ok(response(StatusCode::UNAUTHORIZED, "{}"));
    }
    if path == "/v1/commands" && request.method() == hyper::Method::POST {
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            Limited::new(request.into_body(), MAX_REQUEST_BYTES).collect(),
        )
        .await;
        let Ok(Ok(body)) = body else {
            return Ok(response(StatusCode::BAD_REQUEST, "{}"));
        };
        let Ok(command) = serde_json::from_slice::<ClientCommand>(&body.to_bytes()) else {
            return Ok(response(StatusCode::BAD_REQUEST, "{}"));
        };
        state.counts.commands.fetch_add(1, Ordering::Relaxed);
        // No fixture success, event, read projection or queue metric is synthesized here.
        let reply = state
            .host
            .dispatch(
                BoundClient {
                    client_id: ClientId(CLIENT.into()),
                },
                command,
            )
            .await;
        return Ok(response(StatusCode::ACCEPTED, reply.bytes));
    }
    if path == "/v1/events" && request.method() == hyper::Method::GET {
        let Ok((session, sequence)) = event_position(request.uri().query().unwrap_or_default())
        else {
            return Ok(response(StatusCode::BAD_REQUEST, "{}"));
        };
        let Ok(mut events) = state
            .host
            .subscribe(
                BoundClient {
                    client_id: ClientId(CLIENT.into()),
                },
                session,
                sequence,
            )
            .await
        else {
            return Ok(response(StatusCode::BAD_REQUEST, "{}"));
        };
        let stream = async_stream::stream! {
            while let Some(event) = events.recv().await {
                let event = event.expect("real host event");
                state.counts.events.fetch_add(1, Ordering::Relaxed);
                state.counts.event_bytes.fetch_add(event.json.len() as u64, Ordering::Relaxed);
                let prefix = event.sequence.map_or_else(|| "data: ".to_owned(), |sequence| format!("id: {}\ndata: ", sequence.0));
                yield Ok::<_, Infallible>(Frame::data(Bytes::from(prefix)));
                yield Ok(Frame::data(event.json));
                yield Ok(Frame::data(Bytes::from_static(b"\n\n")));
            }
        };
        return Ok(Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(StreamBody::new(stream).boxed_unsync())
            .expect("SSE body"));
    }
    Ok(response(StatusCode::NOT_FOUND, "{}"))
}

fn event_position(query: &str) -> Result<(Option<SessionId>, Option<SequenceId>), ()> {
    let mut session = None;
    let mut sequence = None;
    for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match name.as_ref() {
            "session_id" if session.is_none() => {
                session = Some(SessionId::parse(value.into_owned()).map_err(|_| ())?)
            }
            "last_seen_sequence" if sequence.is_none() => {
                sequence = Some(SequenceId(value.parse::<u64>().map_err(|_| ())?))
            }
            _ => return Err(()),
        }
    }
    Ok((session, sequence))
}

#[test]
fn event_position_preserves_exact_authority_and_rejects_ambiguous_inputs() {
    assert_eq!(
        event_position("session_id=joined-interactive&last_seen_sequence=42"),
        Ok((
            Some(SessionId("joined-interactive".into())),
            Some(SequenceId(42))
        ))
    );
    assert_eq!(event_position(""), Ok((None, None)));
    for query in [
        "session_id=../foreign",
        "last_seen_sequence=no",
        "last_seen_sequence=-1",
        "session_id=a&session_id=b",
        "last_seen_sequence=1&last_seen_sequence=2",
        "unknown=value",
    ] {
        assert!(
            event_position(query).is_err(),
            "ambiguous event authority: {query}"
        );
    }
}
