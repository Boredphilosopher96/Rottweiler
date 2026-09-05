use std::io::{self, Write};

use miette::{IntoDiagnostic, Result};
use rw_core::{
    GitHubCopilotLogin, OAuthLogin, ProviderApiKey, ProviderLogin, ProviderLoginCancellation,
    begin_provider_login, store_provider_api_key,
};

pub(super) fn auth_set_key(provider_name: &str) -> Result<()> {
    let input = rpassword::prompt_password("API key: ").into_diagnostic()?;
    let api_key = ProviderApiKey::from_terminal_input(input).into_diagnostic()?;
    let warnings = store_provider_api_key(provider_name, api_key).into_diagnostic()?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    println!("stored API key for provider {provider_name}");
    Ok(())
}

pub(super) async fn auth_login(provider_name: &str) -> Result<()> {
    match begin_provider_login(provider_name)
        .await
        .into_diagnostic()?
    {
        ProviderLogin::OAuth(login) => auth_oauth_login(*login).await,
        ProviderLogin::GitHubCopilot(login) => auth_github_copilot_login(*login).await,
    }
}

pub(super) async fn auth_oauth_login(login: OAuthLogin) -> Result<()> {
    for warning in login.warnings() {
        eprintln!("warning: {warning}");
    }
    println!("{}", login.authorization_url());
    io::stdout().flush().into_diagnostic()?;
    eprintln!("waiting for OAuth callback on {}", login.redirect_uri());
    let result = login.complete().await.into_diagnostic()?;
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    if result.refresh_token_stored {
        println!(
            "authenticated provider {}; access and refresh credentials were stored",
            result.provider
        );
    } else {
        eprintln!(
            "warning: provider {} did not issue a refresh token; only the access credential was stored",
            result.provider
        );
        println!("authenticated provider {}", result.provider);
    }
    Ok(())
}

pub(super) async fn auth_github_copilot_login(login: GitHubCopilotLogin) -> Result<()> {
    for warning in login.warnings() {
        eprintln!("warning: {warning}");
    }
    write_github_device_prompt(
        &mut io::stdout(),
        login.verification_uri(),
        login.user_code(),
    )
    .into_diagnostic()?;
    eprintln!("waiting for GitHub device authorization; press Ctrl-C to cancel");
    let cancellation = ProviderLoginCancellation::default();
    let poll_cancellation = cancellation.clone();
    let result = tokio::select! {
        result = login.complete(&poll_cancellation) => result.into_diagnostic()?,
        signal = tokio::signal::ctrl_c() => {
            signal.into_diagnostic()?;
            cancellation.cancel();
            return Err(miette::miette!("GitHub device authorization cancelled"));
        }
    };
    for warning in &result.warnings {
        eprintln!("warning: {warning}");
    }
    println!("authenticated provider {}", result.provider);
    Ok(())
}

pub(super) fn write_github_device_prompt(
    writer: &mut impl Write,
    verification_uri: &str,
    user_code: &str,
) -> io::Result<()> {
    writeln!(writer, "Open {verification_uri}")?;
    writeln!(writer, "Enter code: {user_code}")?;
    writer.flush()
}
