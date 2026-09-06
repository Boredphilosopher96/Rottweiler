//! HTTPS upstream proxies share the engine HTTP stack's cryptographic backend.
use std::{io, sync::Arc};

fn config() -> io::Result<rustls::ClientConfig> {
    let roots = webpki_roots::TLS_SERVER_ROOTS
        .iter()
        .cloned()
        .collect::<rustls::RootCertStore>();
    // Select locally: an embedding process may install a different global provider,
    // and feature unification must not change or invalidate this proxy's policy.
    rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
    .map_err(|error| io::Error::other(error.to_string()))
}

pub(super) fn connection(host: &str) -> io::Result<rustls::ClientConnection> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "proxy host is invalid"))?;
    rustls::ClientConnection::new(Arc::new(config()?), server_name)
        .map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_tls_starts_without_process_global_provider_selection() -> io::Result<()> {
        // Client construction exercises the same configuration as the HTTPS path;
        // it must not depend on another HTTP client first installing a default.
        let mut client = connection("proxy.example")?;
        assert!(client.is_handshaking());
        assert!(client.wants_write());
        let mut hello = Vec::new();
        assert!(client.write_tls(&mut hello)? > 0);
        assert!(!hello.is_empty());
        assert!(connection("invalid proxy host").is_err());
        Ok(())
    }
}
