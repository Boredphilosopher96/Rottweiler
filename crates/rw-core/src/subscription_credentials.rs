use serde::{Deserialize, Serialize};

use rw_providers::{OAuthTokenSet, extract_openai_subscription_account_id};

use crate::AdminError;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OpenAiSubscriptionCredentialBundle {
    version: u8,
    access_token: String,
    refresh_token: String,
    account_id: String,
}

impl OpenAiSubscriptionCredentialBundle {
    pub(crate) fn from_login(tokens: &OAuthTokenSet) -> Result<Self, AdminError> {
        let refresh_token = tokens.refresh_token().ok_or_else(|| {
            AdminError::new("ChatGPT subscription login did not return a refresh token")
        })?;
        let account_id = tokens
            .id_token()
            .and_then(|token| extract_openai_subscription_account_id(token.expose_secret()))
            .or_else(|| {
                extract_openai_subscription_account_id(tokens.access_token().expose_secret())
            })
            .ok_or_else(|| {
                AdminError::new("ChatGPT subscription login did not return an account identifier")
            })?;
        validate_account_id(&account_id)?;
        Ok(Self {
            version: 1,
            access_token: tokens.access_token().expose_secret().to_owned(),
            refresh_token: refresh_token.expose_secret().to_owned(),
            account_id,
        })
    }

    pub(crate) fn new(access_token: String, refresh_token: String, account_id: String) -> Self {
        Self {
            version: 1,
            access_token,
            refresh_token,
            account_id,
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, AdminError> {
        let bundle = serde_json::from_str::<Self>(value)
            .map_err(|_| AdminError::new("ChatGPT subscription credential bundle is malformed"))?;
        if bundle.version != 1
            || bundle.access_token.is_empty()
            || bundle.refresh_token.is_empty()
            || bundle.account_id.is_empty()
        {
            return Err(AdminError::new(
                "ChatGPT subscription credential bundle is malformed",
            ));
        }
        validate_account_id(&bundle.account_id)?;
        Ok(bundle)
    }

    pub(crate) fn encode(&self) -> Result<String, AdminError> {
        serde_json::to_string(self)
            .map_err(|_| AdminError::new("could not encode ChatGPT subscription credential bundle"))
    }

    pub(crate) fn access_token(&self) -> &str {
        &self.access_token
    }

    pub(crate) fn refresh_token(&self) -> &str {
        &self.refresh_token
    }

    pub(crate) fn account_id(&self) -> &str {
        &self.account_id
    }
}

fn validate_account_id(value: &str) -> Result<(), AdminError> {
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.'))
    {
        return Err(AdminError::new(
            "ChatGPT subscription credential bundle has an invalid account identifier",
        ));
    }
    Ok(())
}

pub(crate) fn openai_codex_credential_id(provider: &str) -> String {
    format!("providers.{provider}.openai_codex")
}
