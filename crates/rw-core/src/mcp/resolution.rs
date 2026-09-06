//! DNS work retains shared blocking admission until the system resolver returns.
use super::ProductionMcpHttpError;
use std::net::{SocketAddr, ToSocketAddrs as _};

pub(super) async fn addresses(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, ProductionMcpHttpError> {
    let host = host.to_owned();
    rw_resources::run_blocking(rw_resources::ResourceClass::Blocking, move || {
        (host.as_str(), port)
            .to_socket_addrs()
            .map(Iterator::collect)
            .map_err(|_| ProductionMcpHttpError)
    })
    .await
    .map_err(|_| ProductionMcpHttpError)?
}
