use serde::{Deserialize, Serialize};

use rw_providers::Secret;

use crate::AdminError;

/// One versioned logical vault entry owned exclusively by Rottweiler.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GitHubCopilotCredential {
    version: u8,
    oauth_client_id: String,
    access_token: String,
}

impl GitHubCopilotCredential {
    pub(crate) fn from_secret(
        access_token: &Secret,
        oauth_client_id: &str,
    ) -> Result<Self, AdminError> {
        if access_token.expose_secret().is_empty() {
            return Err(AdminError::new(
                "GitHub Copilot device authorization returned an empty access token",
            ));
        }
        validate_client_id(oauth_client_id)?;
        Ok(Self {
            version: 1,
            oauth_client_id: oauth_client_id.to_owned(),
            access_token: access_token.expose_secret().to_owned(),
        })
    }

    pub(crate) fn parse(value: &str) -> Result<Self, AdminError> {
        let credential = serde_json::from_str::<Self>(value)
            .map_err(|_| AdminError::new("GitHub Copilot credential is malformed"))?;
        if credential.version != 1 || credential.access_token.is_empty() {
            return Err(AdminError::new("GitHub Copilot credential is malformed"));
        }
        validate_client_id(&credential.oauth_client_id)?;
        Ok(credential)
    }

    pub(crate) fn encode(&self) -> Result<String, AdminError> {
        serde_json::to_string(self)
            .map_err(|_| AdminError::new("could not encode GitHub Copilot credential"))
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn oauth_client_id(&self) -> &str {
        &self.oauth_client_id
    }
}

fn validate_client_id(value: &str) -> Result<(), AdminError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(AdminError::new(
            "GitHub Copilot credential has an invalid OAuth client identity",
        ));
    }
    Ok(())
}

pub(crate) fn github_copilot_credential_id(provider: &str) -> String {
    format!("providers.{provider}.github_copilot")
}

#[cfg(test)]
mod tests {
    use rw_providers::Secret;

    use super::GitHubCopilotCredential;

    #[test]
    fn credential_is_one_versioned_entry_and_never_debuggable() {
        let token = Secret::new("copilot-token-canary".to_owned());
        let credential = GitHubCopilotCredential::from_secret(&token, "rottweiler-test-client")
            .unwrap_or_else(|error| panic!("credential must build: {error}"));
        let encoded = credential
            .encode()
            .unwrap_or_else(|error| panic!("credential must encode: {error}"));
        let parsed = GitHubCopilotCredential::parse(&encoded)
            .unwrap_or_else(|error| panic!("credential must parse: {error}"));
        assert_eq!(parsed.access_token(), "copilot-token-canary");
        assert_eq!(parsed.oauth_client_id(), "rottweiler-test-client");
        assert!(encoded.contains("\"version\":1"));
    }

    #[test]
    fn malformed_or_empty_credentials_fail_closed() {
        for value in [
            r#"{"version":2,"oauth_client_id":"test","access_token":"token"}"#,
            r#"{"version":1,"oauth_client_id":"test","access_token":""}"#,
            r#"{"version":1,"oauth_client_id":"","access_token":"token"}"#,
            r#"{"version":1,"oauth_client_id":"test","access_token":"token","extra":true}"#,
            "not-json",
        ] {
            assert!(GitHubCopilotCredential::parse(value).is_err());
        }
    }
}
