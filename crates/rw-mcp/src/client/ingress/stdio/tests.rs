use super::*;
use rmcp::model::{ClientRequest, JsonRpcMessage, RequestId};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt as _, BufReader};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture() -> Result<(StdioTransport, tokio::io::DuplexStream, Arc<Ingress>), McpError> {
    let ingress = Ingress::new(crate::McpInboundRouter::default())?;
    let (client, server) = tokio::io::duplex(8192);
    let (read, write) = tokio::io::split(client);
    let transport = StdioTransport::new(Box::pin(read), Box::pin(write), Arc::clone(&ingress))?;
    Ok((transport, server, ingress))
}

fn ping(
    ingress: &Ingress,
    id: i64,
) -> Result<
    (
        ClientJsonRpcMessage,
        Arc<super::super::requests::RequestState>,
    ),
    Box<dyn std::error::Error>,
> {
    let mut request: ClientRequest = serde_json::from_str(r#"{"method":"ping"}"#)?;
    let retained = ingress.requests.prepare(&mut request)?;
    Ok((
        JsonRpcMessage::request(request, RequestId::Number(id)),
        retained,
    ))
}

#[tokio::test]
async fn cancelled_receive_preserves_partial_frame_and_the_exact_response_owner() -> TestResult {
    let (mut transport, server, ingress) = fixture()?;
    let mut server = BufReader::new(server);
    let (request, state) = ping(&ingress, 7)?;
    transport.send(request).await?;
    let mut sent = String::new();
    server.read_line(&mut sent).await?;
    assert!(sent.contains("\"id\":7"));
    server
        .get_mut()
        .write_all(br#"{"jsonrpc":"2.0","id":7,"res"#)
        .await?;
    assert!(
        tokio::time::timeout(Duration::from_millis(20), transport.receive())
            .await
            .is_err()
    );
    server.get_mut().write_all(b"ult\":{}}\n").await?;
    let response = tokio::time::timeout(Duration::from_secs(2), transport.receive())
        .await?
        .ok_or("response missing")?;
    assert!(
        matches!(&response, ServerJsonRpcMessage::Response(value) if value.id == RequestId::Number(7))
    );
    let retained = state.reply_retention()?;
    let weak = Arc::downgrade(&retained);
    drop(retained);
    drop(response);
    // Enter the next receive so the route-or-drop fence retires; the exact
    // request state must still retain its body independently of that fence.
    assert!(
        tokio::time::timeout(Duration::from_millis(20), transport.receive())
            .await
            .is_err()
    );
    assert!(weak.upgrade().is_some());
    drop(state);
    assert!(weak.upgrade().is_none());
    transport.close().await?;
    Ok(())
}

#[tokio::test]
async fn unknown_response_and_duplicate_response_close_the_wire() -> TestResult {
    for duplicate in [false, true] {
        let (mut transport, mut server, ingress) = fixture()?;
        let (_request, state) = ping(&ingress, 9)?;
        if duplicate {
            let (request, new_state) = ping(&ingress, 1)?;
            transport.send(request).await?;
            server
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                .await?;
            assert!(transport.receive().await.is_some());
            server
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"1\",\"result\":{}}\n")
                .await?;
            assert!(transport.receive().await.is_none());
            drop(new_state);
        } else {
            server
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":99,\"result\":{}}\n")
                .await?;
            assert!(transport.receive().await.is_none());
        }
        drop(state);
        transport.close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn remote_cancel_settles_only_its_exact_owned_request() -> TestResult {
    let (mut transport, mut server, ingress) = fixture()?;
    let (request, state) = ping(&ingress, 3)?;
    transport.send(request).await?;
    server.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":3,\"reason\":\"private cancellation text\"}}\n").await?;
    let response = transport
        .receive()
        .await
        .ok_or("cancellation response missing")?;
    assert!(
        matches!(&response, ServerJsonRpcMessage::Error(error) if error.id == Some(RequestId::Number(3)))
    );
    let encoded = serde_json::to_string(&response)?;
    assert!(!encoded.contains("private cancellation text"));
    assert!(state.reply_retention().is_ok());
    drop(response);
    drop(state);
    transport.close().await?;
    Ok(())
}
