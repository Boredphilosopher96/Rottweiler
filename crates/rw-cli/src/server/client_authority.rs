//! Runtime-scoped authenticated client capabilities without retained registrations.
use super::{ClientCredentials, ClientId, Result, SecretToken};

#[derive(Debug)]
pub(super) struct ClientAuthority {
    key: SecretToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClientCapability {
    Interactive,
    PluginDevelopment,
    ShellBroker,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AuthenticatedClient {
    pub(super) client_id: ClientId,
    pub(super) capability: ClientCapability,
}

impl ClientAuthority {
    pub(super) fn new(bootstrap: &SecretToken) -> Self {
        Self {
            key: SecretToken(blake3::derive_key(
                "Rottweiler client capabilities",
                &bootstrap.0,
            )),
        }
    }

    pub(super) fn mint(&self, capability: ClientCapability) -> Result<ClientCredentials> {
        let kind = match capability {
            ClientCapability::Interactive => 'i',
            ClientCapability::PluginDevelopment => 'p',
            ClientCapability::ShellBroker => 's',
        };
        let nonce = SecretToken::generate()?;
        let client_id = ClientId(format!("client-{kind}-{}", nonce.encode()));
        let token = self.signature(&client_id.0).encode();
        Ok(ClientCredentials { client_id, token })
    }

    pub(super) fn authenticate(&self, client_id: &str, token: &str) -> Option<AuthenticatedClient> {
        // Validate bounded ASCII before allocation or signature work. Both identity
        // and capability are authenticated together under this runtime's key.
        let suffix = client_id.strip_prefix("client-")?;
        let (kind, nonce) = suffix.split_once('-')?;
        if nonce.len() != 64
            || !nonce
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return None;
        }
        let capability = match kind {
            "i" => ClientCapability::Interactive,
            "p" => ClientCapability::PluginDevelopment,
            "s" => ClientCapability::ShellBroker,
            _ => return None,
        };
        self.signature(client_id)
            .matches_encoded(token)
            .then(|| AuthenticatedClient {
                client_id: ClientId(client_id.to_owned()),
                capability,
            })
    }

    fn signature(&self, client_id: &str) -> SecretToken {
        SecretToken(*blake3::keyed_hash(&self.key.0, client_id.as_bytes()).as_bytes())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn capability_identity_and_runtime_are_authenticated() {
        let authority = ClientAuthority::new(&SecretToken([1; 32]));
        let other = ClientAuthority::new(&SecretToken([2; 32]));
        let credentials = authority.mint(ClientCapability::ShellBroker).expect("mint");
        assert!(
            authority
                .authenticate(&credentials.client_id.0, &credentials.token)
                .is_some()
        );
        assert!(
            other
                .authenticate(&credentials.client_id.0, &credentials.token)
                .is_none()
        );
        let altered = credentials
            .client_id
            .0
            .replacen("client-s-", "client-i-", 1);
        assert!(
            authority
                .authenticate(&altered, &credentials.token)
                .is_none()
        );
        let next = authority.mint(ClientCapability::ShellBroker).expect("mint");
        assert_ne!(credentials.client_id, next.client_id);
        assert!(
            authority
                .authenticate(&next.client_id.0, &credentials.token)
                .is_none()
        );
        assert!(
            authority
                .authenticate("client-i-invalid", &credentials.token)
                .is_none()
        );
    }
}
