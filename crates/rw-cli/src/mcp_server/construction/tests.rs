#![allow(clippy::expect_used)]

use super::*;
use rw_core::{ClientId, CommandAckMeta, PROTOCOL_VERSION, SessionDescriptor, SessionId};
use serde_json::json;

fn listed(request: &str, sessions: Vec<SessionDescriptor>) -> HostEvent {
    HostEvent {
        json: serde_json::to_vec(&EngineEvent::SessionsListed {
            meta: CommandAckMeta {
                protocol_version: PROTOCOL_VERSION,
                client_id: ClientId("mcp-driver".into()),
                request_id: RequestId(request.into()),
                emitted_at: "2026-09-08T00:00:00Z".into(),
            },
            sessions,
        })
        .expect("event")
        .into(),
        sequence: None,
    }
}
fn descriptor(id: &str) -> SessionDescriptor {
    SessionDescriptor {
        session_id: SessionId(id.into()),
        title: "visible title".into(),
        workspace_name: "workspace".into(),
        model: rw_types::ModelAlias("test".into()),
        driver_client_id: Some(ClientId("mcp-driver".into())),
        shell_active: false,
    }
}

#[test]
fn create_requires_exact_correlated_singleton() {
    let request = RequestId("create".into());
    assert!(
        created_session(&listed("other", vec![descriptor("foreign")]), &request)
            .expect("fixture operation")
            .is_none()
    );
    assert_eq!(
        created_session(&listed("create", vec![descriptor("owned")]), &request)
            .expect("fixture operation"),
        Some(SessionSummary {
            id: "owned".into(),
            state: "driver".into()
        })
    );
    assert!(created_session(&listed("create", vec![]), &request).is_err());
    assert!(
        created_session(
            &listed("create", vec![descriptor("one"), descriptor("two")]),
            &request
        )
        .is_err()
    );
}

#[test]
fn control_decode_rejects_structural_growth_inside_wire_ceiling() {
    let mut large = descriptor("owned");
    large.title = "x".repeat(3 * 1024 * 1024);
    let event = listed("create", vec![large]);
    assert!(event.json.len() < MAX_CONTROL_BYTES);
    assert!(created_session(&event, &RequestId("create".into())).is_err());
}

#[test]
fn authorized_ids_have_no_duplicate_or_path_selectors() {
    let mut exact = vec!["z".to_owned(), "a".to_owned()];
    validate_authorized(&mut exact).expect("valid authorization");
    assert_eq!(exact, ["a", "z"]);
    assert!(validate_authorized(&mut ["same".into(), "same".into()]).is_err());
    assert!(validate_authorized(&mut ["../foreign".into()]).is_err());
    assert!(validate_authorized(&mut vec!["id".into(); rw_mcp::MAX_SERVER_SESSIONS + 1]).is_err());
}

#[test]
fn fixed_readonly_catalog_and_arguments_fit_declared_construction() {
    let descriptors = tool_descriptors(super::super::read_only_tools().expect("fixture operation"))
        .expect("fixture operation");
    assert_eq!(descriptors.len(), 4);
    let input = json!({"path": "file.txt", "offset": 2, "limit": 10});
    let (arguments, permission) = tool_arguments(input.clone()).expect("fixture operation");
    assert_eq!(arguments, input);
    assert_eq!(permission, input);
    assert!(tool_arguments(json!({"path": "x".repeat(MAX_ARGUMENT_BYTES)})).is_err());
}

#[tokio::test]
async fn lost_cpu_waiter_keeps_source_until_physical_work_retires() {
    struct Source(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for Source {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (entered, started) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let slot = McpResponseSlot::new(
        rw_mcp::McpResponseLimits::new(WORKING_BYTES).expect("fixture operation"),
    )
    .expect("fixture operation");
    let waiter = tokio::spawn(
        Construction::new(Source(Arc::clone(&dropped)), slot).map_cpu(move |source| {
            entered.send(()).expect("report worker entry");
            blocked
                .recv_timeout(Duration::from_secs(5))
                .expect("worker released");
            drop(source);
            Ok(())
        }),
    );
    started.await.expect("worker started");
    waiter.abort();
    assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
    assert!(!dropped.load(std::sync::atomic::Ordering::Acquire));
    release.send(()).expect("release worker");
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(std::sync::atomic::Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fixture operation");
}
