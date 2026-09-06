//! System resolution retains blocking admission through the actual OS lookup.
use std::{error::Error, net::ToSocketAddrs};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use rw_resources::{ResourceClass, WorkError};

pub(super) struct OwnedResolver;

impl Resolve for OwnedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addresses = rw_resources::run_blocking(ResourceClass::Blocking, move || {
                (name.as_str(), 0).to_socket_addrs()
            })
            .await??;
            Ok(Box::new(addresses) as Addrs)
        })
    }
}

pub(super) fn admission_failed(error: &(dyn Error + 'static)) -> bool {
    let mut source = Some(error);
    // Bound traversal even if a custom transport error has a cyclic source.
    for _ in 0..32 {
        let Some(error) = source else {
            return false;
        };
        if matches!(
            error.downcast_ref::<WorkError>(),
            Some(WorkError::Admission(_))
        ) {
            return true;
        }
        source = error.source();
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    #[tokio::test]
    async fn system_resolution_returns_addresses_without_choosing_a_service_port() {
        let addresses = OwnedResolver
            .resolve("localhost".parse().expect("name"))
            .await
            .expect("local resolver")
            .collect::<Vec<_>>();
        assert!(!addresses.is_empty());
        assert!(
            addresses
                .iter()
                .all(|address| address.ip().is_loopback() && address.port() == 0)
        );
    }

    #[test]
    fn resolver_admission_errors_remain_distinct_from_network_failures() {
        let error = WorkError::Admission(rw_resources::AdmissionError::QueueFull);
        assert!(admission_failed(&error));
        assert!(!admission_failed(&std::io::Error::other("lookup failed")));
    }
}
