#![allow(clippy::expect_used)]
use super::*;
use crate::{
    McpInboundRouter,
    client::ingress::{
        Ingress,
        message::{self, Delivery},
    },
    ingress::frame::RawFrame,
};
use rmcp::model::{ClientJsonRpcMessage, ErrorCode, RequestId};
use serde_json::json;
use std::{sync::Arc, time::Duration};

fn denied(input: &[u8]) -> (RequestId, usize) {
    let mut charge = None;
    let value = decode(input, None, &mut |bytes| {
        assert!(
            charge.replace(bytes).is_none(),
            "one admission before ID ownership"
        );
        Ok(())
    })
    .expect("valid unsupported request");
    let DecodedMessage::DeniedRequest(id) = value else {
        panic!("denial")
    };
    (id, charge.expect("admitted"))
}

#[test]
fn unsupported_params_are_opaque_and_never_construct_method_graphs() {
    let mut charge = None;
    for method in [
        "sampling/createMessage",
        "elicitation/create",
        "roots/list",
        "tasks/get",
        "custom/work",
    ] {
        for params in [
            json!(null),
            json!(17),
            json!({"messages":"not a sampling schema"}),
            json!({"untrusted":"x".repeat(1024 * 1024)}),
        ] {
            let bytes = serde_json::to_vec(
                &json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}),
            )
            .expect("input");
            let (id, current) = denied(&bytes);
            assert_eq!(id, RequestId::Number(7));
            assert_eq!(
                *charge.get_or_insert(current),
                current,
                "body size/type cannot expand denial construction"
            );
            assert_eq!(current, 16 * 1024 + 2);
        }
    }
    let ping = br#"{"jsonrpc":"2.0","id":7,"method":"ping"}"#;
    assert!(matches!(
        decode(ping, None, &mut |_| Ok(())).expect("ping"),
        DecodedMessage::Protocol(ServerJsonRpcMessage::Request(_))
    ));
}

#[test]
fn denial_preserves_exact_escaped_string_identity_and_admits_before_owning_it() {
    let bytes = br#"{"jsonrpc":"2.0","id":"\u0030\u0030\u0037","method":"roots/list"}"#;
    let (id, charge) = denied(bytes);
    assert_eq!(id, RequestId::String("007".into()));
    assert_eq!(charge, 16 * 1024 + 2 * 20);
    let error = decode(bytes, None, &mut |_| {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "full"))
    })
    .expect_err("no result without admission");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    let id = "λ".repeat(128 * 1024);
    let bytes = serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":"roots/list"}))
        .expect("large ID");
    let (actual, charge) = denied(&bytes);
    assert_eq!(actual, RequestId::String(id.clone().into()));
    assert!(
        charge >= id.len() * 2,
        "retained ID and scalar scratch both admitted"
    );
}

#[test]
fn denial_does_not_bypass_envelope_or_structure_rejection() {
    let mut inputs: Vec<Vec<u8>> = [
        r#"{"jsonrpc":"2.0","id":1,"id":2,"method":"roots/list"}"#,
        r#"{"jsonrpc":"2.0","id":1,"\u0069d":2,"method":"roots/list"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"roots/list","method":"ping"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"roots/list","params":{},"params":{}}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"roots/list","result":{}}"#,
        r#"{"jsonrpc":"2.0","id":{},"method":"roots/list"}"#,
        r#"{"jsonrpc":"2.0","id":null,"method":"roots/list"}"#,
        r#"{"jsonrpc":"1.0","id":1,"method":"roots/list"}"#,
        r#"{"jsonrpc":"2.0","id":1,"method":"roots/list","params":{"x":1,}}"#,
    ]
    .into_iter()
    .map(|input| input.as_bytes().to_vec())
    .collect();
    inputs.push(
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\",\"params\":{}0{}}}",
            "[".repeat(65),
            "]".repeat(65)
        )
        .into_bytes(),
    );
    inputs.push(
        format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\",\"params\":[{}]}}",
            vec!["0"; 65_536].join(",")
        )
        .into_bytes(),
    );
    for input in inputs {
        let mut admitted = false;
        assert!(
            decode(&input, None, &mut |_| {
                admitted = true;
                Ok(())
            })
            .is_err()
        );
        assert!(!admitted, "invalid input cannot acquire a decoded owner");
    }
}

#[tokio::test]
async fn denial_credit_follows_reply_after_its_send_observer_disappears() {
    let ingress = Ingress::new(McpInboundRouter::default()).expect("ingress");
    let mut frame = RawFrame::new(4096).expect("raw credit");
    frame.append(br#"{"jsonrpc":"2.0","id":"007","method":"sampling/createMessage","params":{"secret":"never reflected"}}"#).expect("frame");
    let packet = ingress.decode_frame(frame).expect("owned decode");
    let weak = Arc::downgrade(&packet.retained);
    let Delivery::Reply(reply, retained) = packet.dispatch() else {
        panic!("local denial")
    };
    assert!(
        matches!(&reply, ClientJsonRpcMessage::Error(error) if error.id == Some(RequestId::String("007".into())) && error.error.code == ErrorCode::METHOD_NOT_FOUND && error.error.data.is_none())
    );
    let (release, waiting) = tokio::sync::oneshot::channel();
    let (started, observed) = tokio::sync::oneshot::channel();
    let task = message::control(
        async move {
            started.send(()).expect("observed");
            waiting.await.expect("release send");
            drop(reply);
            Ok(())
        },
        retained,
    );
    observed.await.expect("physical send started");
    drop(task);
    assert!(
        weak.upgrade().is_some(),
        "caller loss cannot refund pending reply"
    );
    release.send(()).expect("complete send");
    tokio::time::timeout(Duration::from_secs(2), async {
        while weak.upgrade().is_some() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("reply and credit retire together");
}
