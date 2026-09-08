#![allow(clippy::expect_used)]
use super::*;

#[tokio::test]
async fn an_unpolled_request_owns_its_body_and_retires_admission_without_invoking_the_bridge() {
    let (server, bridge) = super::super::ownership_tests::server(Duration::from_secs(30));
    let body = serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":42,"method":"tools/call","params":{"name":"rottweiler_tools_call","arguments":{"name":"read","arguments":{"text":"x".repeat(16*1024)}}}})).expect("JSON");
    let mut frame = crate::ingress::frame::RawFrame::new(crate::ingress::frame::STDIO_FRAME_BYTES)
        .expect("frame");
    frame.append(&body).expect("append");
    let decoded = wire::decode(frame).await.expect("decode");
    let Body::Request { id, request } = decoded.body else {
        panic!("request");
    };
    let jobs = Arc::new(Jobs::default());
    let control = Arc::new(Control {
        id,
        cancelled: AtomicBool::new(false),
        claimed: AtomicBool::new(false),
        task_done: AtomicBool::new(false),
        reply_done: AtomicBool::new(false),
        deadline: Instant::now() + WRITE_DEADLINE,
        credits: Credits {
            decoded: Arc::new(decoded.retained),
            _job: jobs.retain().expect("job"),
        },
    });
    let retired = Arc::downgrade(&control.credits.decoded);
    let work = RequestWork {
        request: *request,
        negotiated: Ok(Negotiated {
            legacy: true,
            initialize: None,
        }),
        server: Arc::new(server),
        control,
    };
    let future = work.run();
    assert!(retired.upgrade().is_some());
    drop(future);
    assert!(retired.upgrade().is_none());
    tokio::time::timeout(Duration::from_secs(1), jobs.settle())
        .await
        .expect("job retired");
    assert_eq!(bridge.entered.available_permits(), 0);
}
