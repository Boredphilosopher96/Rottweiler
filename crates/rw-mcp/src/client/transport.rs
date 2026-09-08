//! One client service graph, with each wire retaining its physical transport owner.
use super::ingress::{http::HttpTransport, stdio::StdioTransport};
use futures_util::future::Either;
use rmcp::{
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    service::RoleClient,
    transport::Transport,
};
use std::io;

pub(super) enum ClientTransport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

impl Transport<RoleClient> for ClientTransport {
    type Error = io::Error;

    fn send(
        &mut self,
        message: ClientJsonRpcMessage,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        // Admission happens synchronously in the selected transport before its
        // independently owned send future leaves this connection borrow.
        match self {
            Self::Stdio(transport) => Either::Left(transport.send(message)),
            Self::Http(transport) => Either::Right(transport.send(message)),
        }
    }

    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        match self {
            Self::Stdio(transport) => transport.receive().await,
            Self::Http(transport) => transport.receive().await,
        }
    }

    async fn close(&mut self) -> io::Result<()> {
        match self {
            Self::Stdio(transport) => transport.close().await,
            Self::Http(transport) => transport.close().await,
        }
    }
}
