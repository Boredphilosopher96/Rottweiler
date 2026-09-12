use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rw_store::credentials::{CredentialError, CredentialStoreUnavailable, Secret};
use tempfile::tempdir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;

#[derive(Clone, Copy)]
struct EmptyEnvironment;

impl CredentialEnvironment for EmptyEnvironment {
    fn get(&self, _name: &str) -> std::result::Result<Option<String>, CredentialError> {
        Ok(None)
    }
}

#[derive(Clone)]
struct SeededEnvironment {
    name: String,
    value: String,
}

impl CredentialEnvironment for SeededEnvironment {
    fn get(&self, name: &str) -> std::result::Result<Option<String>, CredentialError> {
        Ok((name == self.name).then(|| self.value.clone()))
    }
}

#[derive(Clone)]
struct CountingVault {
    reads: Arc<AtomicUsize>,
}

impl CredentialStore for CountingVault {
    fn get(
        &self,
        identifier: &str,
    ) -> std::result::Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        assert_eq!(identifier, rw_store::credentials::CREDENTIAL_VAULT_ID);
        Ok(Some(Secret::new(
            "version = 1\n[credentials]\nfirst = 'one'\nsecond = 'two'\n".to_owned(),
        )))
    }

    fn set(
        &self,
        _identifier: &str,
        _secret: &Secret<String>,
    ) -> std::result::Result<(), CredentialStoreUnavailable> {
        Err(CredentialStoreUnavailable)
    }
}

#[derive(Clone)]
struct EmptyCountingVault {
    reads: Arc<AtomicUsize>,
    writes: Arc<AtomicUsize>,
    fresh_reads: Arc<AtomicUsize>,
}

impl CredentialStore for EmptyCountingVault {
    fn get(
        &self,
        identifier: &str,
    ) -> std::result::Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        assert_eq!(identifier, rw_store::credentials::CREDENTIAL_VAULT_ID);
        Ok(None)
    }

    fn set(
        &self,
        _identifier: &str,
        _secret: &Secret<String>,
    ) -> std::result::Result<(), CredentialStoreUnavailable> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn get_fresh(
        &self,
        _identifier: &str,
    ) -> std::result::Result<Option<Secret<String>>, CredentialStoreUnavailable> {
        self.fresh_reads.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

fn seeded_report(checks: Vec<DoctorCheck>) -> DoctorReport {
    finish_report(false, checks)
}

#[test]
fn fresh_configuration_fails_provider_and_default_model_readiness() {
    let mut checks = Vec::new();
    append_provider_readiness_checks(&mut checks, &Config::default());
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|item| item.status == CheckStatus::Fail));
    assert_eq!(checks[0].code, "provider_not_configured");
    assert_eq!(checks[1].code, "default_model_unresolved");
}

#[test]
fn configured_provider_and_default_route_pass_readiness() {
    let mut config = Config::default();
    config.providers.insert(
        "openai".to_owned(),
        ProviderConfig {
            kind: "openai".to_owned(),
            ..ProviderConfig::default()
        },
    );
    config.models.default = "fast".to_owned();
    config.models.aliases.insert(
        "fast".to_owned(),
        vec!["missing/gpt-5".to_owned(), "openai/gpt-5".to_owned()],
    );
    let mut checks = Vec::new();
    append_provider_readiness_checks(&mut checks, &config);
    assert_eq!(checks.len(), 2);
    assert!(checks.iter().all(|item| item.status == CheckStatus::Pass));
}

#[test]
fn seeded_bad_credential_is_diagnosed_deterministically() {
    let report = seeded_report(vec![reachability_check(
        "fixture",
        Reachability::CredentialRejected(401),
        500,
    )]);
    assert!(report.has_failures());
    assert_eq!(report.checks[0].code, "credential_rejected");
    assert_eq!(report.checks[0].details["http_status"], "401");
}

#[test]
fn malformed_subscription_bundle_is_not_reported_as_present() {
    let reference = reference_key("subscription".to_owned(), None);
    let plan = ProviderPlan {
        name: "openai".to_owned(),
        kind: "openai_codex".to_owned(),
        endpoint: None,
        auth: Some(reference.clone()),
        auth_scheme: AuthScheme::OpaqueBundle,
        refresh: None,
        proxy: None,
        proxy_username: None,
        proxy_password: None,
    };
    let inventory = BTreeMap::from([(
        reference,
        InventoryValue::Present {
            source: "test",
            secret: DoctorSecret("malformed-canary".to_owned()),
        },
    )]);
    let item = provider_auth_check(&plan, &inventory);
    assert_eq!(item.status, CheckStatus::Fail);
    assert_eq!(item.code, "credential_invalid");
    assert!(
        !serde_json::to_string(&item)
            .expect("JSON")
            .contains("malformed-canary")
    );
}

#[test]
fn opaque_subscription_probe_never_claims_authentication_succeeded() {
    let reference = reference_key("subscription".to_owned(), None);
    let plan = ProviderPlan {
        name: "openai".to_owned(),
        kind: "openai_codex".to_owned(),
        endpoint: None,
        auth: Some(reference.clone()),
        auth_scheme: AuthScheme::OpaqueBundle,
        refresh: None,
        proxy: None,
        proxy_username: None,
        proxy_password: None,
    };
    let inventory = BTreeMap::from([(
        reference,
        InventoryValue::Present {
            source: "test",
            secret: DoctorSecret("opaque-canary".to_owned()),
        },
    )]);
    let value = classify_reachability(401, &plan, &inventory);
    assert_eq!(value, Reachability::ReachableAuthUnverified(401));
    let item = reachability_check("openai", value, 500);
    assert_eq!(item.status, CheckStatus::Warning);
    assert_eq!(item.code, "provider_reachable_auth_unverified");
}

#[test]
fn oauth_rejection_with_any_refresh_token_requires_refresh() {
    let access = reference_key("oauth-access".to_owned(), None);
    let refresh = reference_key("oauth-refresh".to_owned(), None);
    let plan = ProviderPlan {
        name: "oauth".to_owned(),
        kind: "generic".to_owned(),
        endpoint: None,
        auth: Some(access.clone()),
        auth_scheme: AuthScheme::Bearer,
        refresh: Some(refresh.clone()),
        proxy: None,
        proxy_username: None,
        proxy_password: None,
    };
    let inventory = BTreeMap::from([
        (
            access,
            InventoryValue::Present {
                source: "test",
                secret: DoctorSecret("expired-access".to_owned()),
            },
        ),
        (
            refresh,
            InventoryValue::Present {
                source: "test",
                secret: DoctorSecret("refresh-canary".to_owned()),
            },
        ),
    ]);
    assert_eq!(
        classify_reachability(401, &plan, &inventory),
        Reachability::RefreshRequired(401)
    );
}

#[test]
fn seeded_unreachable_provider_is_distinct_from_bad_auth() {
    let report = seeded_report(vec![reachability_check(
        "fixture",
        Reachability::Unreachable,
        500,
    )]);
    assert!(report.has_failures());
    assert_eq!(report.checks[0].code, "provider_unreachable");
    assert!(!report.checks[0].details.contains_key("http_status"));
}

#[test]
fn non_success_provider_statuses_are_never_reported_as_healthy() {
    let plan = ProviderPlan {
        name: "fixture".to_owned(),
        kind: "credential_free".to_owned(),
        endpoint: None,
        auth: None,
        auth_scheme: AuthScheme::None,
        refresh: None,
        proxy: None,
        proxy_username: None,
        proxy_password: None,
    };
    let inventory = BTreeMap::new();

    let not_found = reachability_check(
        "fixture",
        classify_reachability(404, &plan, &inventory),
        500,
    );
    assert_eq!(not_found.status, CheckStatus::Warning);
    assert_eq!(not_found.code, "provider_endpoint_response_unexpected");

    let limited = reachability_check(
        "fixture",
        classify_reachability(429, &plan, &inventory),
        500,
    );
    assert_eq!(limited.status, CheckStatus::Warning);
    assert_eq!(limited.code, "provider_rate_limited");

    let unavailable = reachability_check(
        "fixture",
        classify_reachability(500, &plan, &inventory),
        500,
    );
    assert_eq!(unavailable.status, CheckStatus::Fail);
    assert_eq!(unavailable.code, "provider_service_unavailable");
}

#[test]
fn seeded_unavailable_sandbox_is_a_failure() {
    let report = seeded_report(vec![sandbox_check(false, "none")]);
    assert!(report.has_failures());
    assert_eq!(report.checks[0].code, "sandbox_unavailable");
}

#[test]
fn seeded_dumb_terminal_is_a_failure() {
    let report = seeded_report(vec![terminal_check_from(Some("dumb"), None, true)]);
    assert!(report.has_failures());
    assert_eq!(report.checks[0].code, "terminal_dumb");
}

#[test]
fn unsupported_native_windows_fails_and_unknown_architecture_warns() {
    let windows = os_check_from("windows", "x86_64", false);
    assert_eq!(windows.status, CheckStatus::Fail);
    assert_eq!(windows.code, "os_unsupported");

    let unknown_arch = os_check_from("linux", "mystery", false);
    assert_eq!(unknown_arch.status, CheckStatus::Warning);
    assert_eq!(unknown_arch.code, "architecture_unverified");
}

#[test]
fn missing_but_creatable_config_root_does_not_fail() {
    let root = tempdir().expect("runtime root");
    let executable = std::env::current_exe().expect("test executable");
    let missing = root.path().join("new").join("config");
    let item = runtime_path_check(Some(&missing), Some(&executable), Some(root.path()));
    assert_eq!(item.status, CheckStatus::Warning);
    assert_eq!(item.code, "config_root_not_created");
    assert_eq!(item.details["config_root_state"], "creatable");
}

#[test]
fn network_probes_are_skipped_unless_explicitly_requested() {
    let report = seeded_report(vec![check(
        "provider.fixture.reachability",
        CheckStatus::Skipped,
        "network_probe_not_requested",
        "skipped",
    )]);
    assert!(!report.has_failures());
    assert_eq!(report.checks[0].status, CheckStatus::Skipped);
}

#[test]
fn stable_json_and_text_never_have_a_secret_field() {
    let report = seeded_report(vec![check(
        "provider.fixture.auth",
        CheckStatus::Pass,
        "credential_present",
        "provider credential is present",
    )]);
    let first = serde_json::to_string(&report).expect("doctor JSON");
    assert_eq!(first, serde_json::to_string(&report).expect("doctor JSON"));
    assert!(!first.contains("secret"));
    assert!(!render_text(&report).contains("secret"));
}

#[test]
fn timeout_is_clamped_to_a_bounded_range() {
    assert_eq!(
        1_u64.clamp(MIN_NETWORK_TIMEOUT_MS, MAX_NETWORK_TIMEOUT_MS),
        250
    );
    assert_eq!(
        u64::MAX.clamp(MIN_NETWORK_TIMEOUT_MS, MAX_NETWORK_TIMEOUT_MS),
        10_000
    );
}

#[test]
fn credential_inventory_reads_the_shared_vault_once() {
    let root = tempdir().expect("credential root");
    let reads = Arc::new(AtomicUsize::new(0));
    let manager = CredentialManager::with_backends(
        EmptyEnvironment,
        CountingVault {
            reads: Arc::clone(&reads),
        },
        root.path().join("credentials.toml"),
    );
    let references = [
        reference_key("first".to_owned(), None),
        reference_key("second".to_owned(), None),
    ]
    .into_iter()
    .collect();
    let inventory = inventory_credentials_with_manager(&manager, references);
    assert!(matches!(
        inventory.get(&reference_key("first".to_owned(), None)),
        Some(InventoryValue::Present { .. })
    ));
    assert!(matches!(
        inventory.get(&reference_key("second".to_owned(), None)),
        Some(InventoryValue::Present { .. })
    ));
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[test]
fn empty_vault_inventory_never_writes() {
    let root = tempdir().expect("credential root");
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let fresh_reads = Arc::new(AtomicUsize::new(0));
    let manager = CredentialManager::with_backends(
        EmptyEnvironment,
        EmptyCountingVault {
            reads: Arc::clone(&reads),
            writes: Arc::clone(&writes),
            fresh_reads: Arc::clone(&fresh_reads),
        },
        root.path().join("credentials.toml"),
    );
    let references = [
        reference_key("first".to_owned(), None),
        reference_key("second".to_owned(), None),
    ]
    .into_iter()
    .collect();
    let inventory = inventory_credentials_with_manager(&manager, references);
    assert!(
        inventory
            .values()
            .all(|value| matches!(value, InventoryValue::Missing))
    );
    assert_eq!(reads.load(Ordering::SeqCst), 1);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    assert_eq!(fresh_reads.load(Ordering::SeqCst), 0);
}

#[test]
fn empty_or_environment_only_inventory_does_not_touch_the_vault() {
    let root = tempdir().expect("credential root");
    let reads = Arc::new(AtomicUsize::new(0));
    let manager = CredentialManager::with_backends(
        EmptyEnvironment,
        CountingVault {
            reads: Arc::clone(&reads),
        },
        root.path().join("credentials.toml"),
    );
    assert!(inventory_credentials_with_manager(&manager, BTreeSet::new()).is_empty());
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    let env_reads = Arc::new(AtomicUsize::new(0));
    let manager = CredentialManager::with_backends(
        SeededEnvironment {
            name: "DOCTOR_TOKEN".to_owned(),
            value: "environment-canary".to_owned(),
        },
        CountingVault {
            reads: Arc::clone(&env_reads),
        },
        root.path().join("credentials.toml"),
    );
    let reference = reference_key(
        "environment-only".to_owned(),
        Some("DOCTOR_TOKEN".to_owned()),
    );
    let inventory =
        inventory_credentials_with_manager(&manager, [reference.clone()].into_iter().collect());
    assert!(matches!(
        inventory.get(&reference),
        Some(InventoryValue::Present {
            source: "environment",
            ..
        })
    ));
    assert_eq!(env_reads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn loopback_probe_distinguishes_a_rejected_credential() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut request = vec![0_u8; 8 * 1024];
        let read = socket.read(&mut request).await.expect("request");
        let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
        assert!(request.contains("authorization: bearer rejected-canary"));
        socket
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("response");
    });
    let reference = reference_key("fixture-key".to_owned(), None);
    let inventory = BTreeMap::from([(
        reference.clone(),
        InventoryValue::Present {
            source: "test",
            secret: DoctorSecret("rejected-canary".to_owned()),
        },
    )]);
    let plan = ProviderPlan {
        name: "fixture".to_owned(),
        kind: "openai_compatible".to_owned(),
        endpoint: Some(Url::parse(&format!("http://{address}/v1")).expect("endpoint")),
        auth: Some(reference),
        auth_scheme: AuthScheme::Bearer,
        refresh: None,
        proxy: None,
        proxy_username: None,
        proxy_password: None,
    };
    assert_eq!(
        probe_provider(&plan, &inventory, 1_000).await,
        Reachability::CredentialRejected(401)
    );
    server.await.expect("server");
}

#[tokio::test]
async fn explicit_proxy_path_and_proxy_auth_are_used() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("proxy listener");
    let address = listener.local_addr().expect("proxy address");
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("proxy accept");
        let mut request = vec![0_u8; 8 * 1024];
        let read = socket.read(&mut request).await.expect("proxy request");
        let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
        assert!(request.starts_with("head http://doctor.invalid/probe "));
        assert!(request.contains("proxy-authorization: basic "));
        socket
            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("proxy response");
    });
    let proxy_reference = reference_key("proxy-password".to_owned(), None);
    let inventory = BTreeMap::from([(
        proxy_reference.clone(),
        InventoryValue::Present {
            source: "test",
            secret: DoctorSecret("proxy-canary".to_owned()),
        },
    )]);
    let plan = ProviderPlan {
        name: "fixture".to_owned(),
        kind: "openai_compatible".to_owned(),
        endpoint: Some(Url::parse("http://doctor.invalid/probe").expect("endpoint")),
        auth: None,
        auth_scheme: AuthScheme::None,
        refresh: None,
        proxy: Some(Url::parse(&format!("http://{address}")).expect("proxy")),
        proxy_username: Some("doctor".to_owned()),
        proxy_password: Some(proxy_reference),
    };
    assert_eq!(
        probe_provider(&plan, &inventory, 1_000).await,
        Reachability::Reachable(204)
    );
    server.await.expect("proxy server");
}
