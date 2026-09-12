use rw_providers::FixtureRedactor;
use rw_tools::CommandFixtureRedactor;

pub(super) struct SharedCommandFixtureRedactor(pub(super) FixtureRedactor);

pub(super) fn credential_shaped_environment_name(name: &str) -> bool {
    let normalized = name.to_ascii_uppercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "API_KEY"
            | "TOKEN"
            | "ACCESS_TOKEN"
            | "REFRESH_TOKEN"
            | "ID_TOKEN"
            | "AUTH_TOKEN"
            | "BEARER_TOKEN"
            | "SESSION_TOKEN"
            | "OAUTH_TOKEN"
            | "PASSWORD"
            | "SECRET"
            | "CLIENT_SECRET"
            | "PRIVATE_KEY"
            | "CREDENTIAL"
            | "CREDENTIALS"
            | "AUTHORIZATION"
            | "COOKIE"
    ) || normalized.ends_with("_API_KEY")
        || normalized.ends_with("_TOKEN")
        || normalized.ends_with("_PASSWORD")
        || normalized.ends_with("_SECRET")
        || normalized.ends_with("_PRIVATE_KEY")
        || normalized.ends_with("_CREDENTIAL")
        || normalized.ends_with("_CREDENTIALS")
}

pub fn register_credential_environment(redactor: &FixtureRedactor) {
    for (name, value) in std::env::vars_os() {
        let (Some(name), Some(value)) = (name.to_str(), value.to_str()) else {
            continue;
        };
        register_credential_environment_value(redactor, name, value);
    }
}

pub(super) fn register_credential_environment_value(
    redactor: &FixtureRedactor,
    name: &str,
    value: &str,
) {
    if !value.is_empty() && credential_shaped_environment_name(name) {
        redactor.register_known_value(value);
    }
}

impl CommandFixtureRedactor for SharedCommandFixtureRedactor {
    fn redact(&self, value: &str) -> String {
        self.0.redact_text(value)
    }

    fn max_secret_bytes(&self) -> usize {
        self.0.maximum_registered_secret_bytes()
    }
}

pub(super) struct SharedEngineSecretRedactor(pub(super) FixtureRedactor);

impl rw_core::SecretRedactor for SharedEngineSecretRedactor {
    fn redact(&self, value: &str) -> String {
        self.0.redact_text(value)
    }

    fn max_secret_bytes(&self) -> usize {
        self.0.maximum_registered_secret_bytes().max(64)
    }

    fn has_incomplete_secret_envelope(&self, text: &str) -> bool {
        let Some(begin) = text.rfind("-----BEGIN ") else {
            return false;
        };
        let pending = &text[begin..];
        let Some(kind_end) = pending.find("PRIVATE KEY-----") else {
            return false;
        };
        !pending[kind_end..].lines().any(|line| {
            let Some(end) = line.find("-----END ") else {
                return false;
            };
            let marker = line[end + "-----END ".len()..].trim_end_matches('\r');
            marker
                .strip_suffix("PRIVATE KEY-----")
                .is_some_and(|label| !label.contains('-'))
        })
    }
}
