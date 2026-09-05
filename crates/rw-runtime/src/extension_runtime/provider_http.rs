//! Each authenticated HTTP operation owns a bounded channel and private network runtime.
use super::{PluginRuntimeBudget, plugin_http_error, plugin_http_guard_error};
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::StreamExt as _;
use rw_ext::{
    PluginHttpStreamResponse, PluginProviderHttpHandler, PluginProviderHttpOperation,
    PluginRpcError,
};
use rw_store::credentials::{CredentialManager, CredentialReference};
use rw_tools::{CancellationToken, EgressPolicy, SupervisedEgressProxy};
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{OwnedSemaphorePermit, mpsc, oneshot};

type HttpResult = Result<PluginHttpStreamResponse, PluginRpcError>;
type BodyItem = Result<Vec<u8>, PluginRpcError>;

pub(super) struct RuntimePluginProviderHttp {
    credentials: Arc<CredentialManager>,
    registrar: Arc<dyn rw_providers::KnownSecretRegistrar>,
    domains: Arc<BTreeSet<String>>,
    budget: Arc<PluginRuntimeBudget>,
}
impl RuntimePluginProviderHttp {
    pub(super) fn new(
        credentials_path: &Path,
        domains: &[String],
        registrar: Arc<dyn rw_providers::KnownSecretRegistrar>,
        budget: Arc<PluginRuntimeBudget>,
    ) -> Self {
        Self {
            credentials: Arc::new(CredentialManager::system(credentials_path)),
            registrar,
            domains: Arc::new(domains.iter().cloned().collect()),
            budget,
        }
    }
}
impl PluginProviderHttpHandler for RuntimePluginProviderHttp {
    fn prepare(
        &self,
        params: serde_json::Value,
        cancellation: CancellationToken,
    ) -> Result<Arc<dyn PluginProviderHttpOperation>, PluginRpcError> {
        let params = serde_json::from_value(params).map_err(|_| {
            plugin_http_error("invalid_request", "provider HTTP request is invalid")
        })?;
        let permit = self.budget.http()?;
        let (head, response) = oneshot::channel();
        Ok(Arc::new(HttpOperation {
            input: Mutex::new(Some(HttpInput {
                params,
                credentials: self.credentials.clone(),
                registrar: self.registrar.clone(),
                domains: self.domains.clone(),
                cancellation: cancellation.clone(),
                head,
            })),
            response: tokio::sync::Mutex::new(Some(response)),
            worker: tokio::sync::Mutex::new(None),
            cancellation,
            started: AtomicBool::new(false),
            settled: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            permit: Some(permit),
        }))
    }
}
struct HttpInput {
    params: rw_plugin_protocol::ProviderHttpCapabilityParams,
    credentials: Arc<CredentialManager>,
    registrar: Arc<dyn rw_providers::KnownSecretRegistrar>,
    domains: Arc<BTreeSet<String>>,
    cancellation: CancellationToken,
    head: oneshot::Sender<HttpResult>,
}
struct HttpOperation {
    input: Mutex<Option<HttpInput>>,
    response: tokio::sync::Mutex<Option<oneshot::Receiver<HttpResult>>>,
    worker: tokio::sync::Mutex<Option<tokio::task::JoinHandle<Result<(), PluginRpcError>>>>,
    cancellation: CancellationToken,
    started: AtomicBool,
    settled: AtomicBool,
    failed: AtomicBool,
    permit: Option<OwnedSemaphorePermit>,
}
impl Drop for HttpOperation {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if self.started.load(Ordering::Acquire) && !self.settled.load(Ordering::Acquire) {
            if let Some(permit) = self.permit.take() {
                permit.forget();
            }
            if let Some(worker) = self.worker.get_mut().take() {
                std::mem::forget(worker);
            }
        }
    }
}
#[async_trait]
impl PluginProviderHttpOperation for HttpOperation {
    async fn response(&self) -> HttpResult {
        let mut worker = self.worker.lock().await;
        if self.cancellation.is_cancelled() || self.settled.load(Ordering::Acquire) {
            return Err(plugin_http_error("cancelled", "HTTP admission is closed"));
        }
        let input = self
            .input
            .lock()
            .map_err(|_| unsettled("HTTP ownership poisoned"))?
            .take()
            .ok_or_else(|| {
                plugin_http_error("invalid_request", "HTTP response already consumed")
            })?;
        self.started.store(true, Ordering::Release);
        *worker = Some(tokio::task::spawn_blocking(move || run_owned(input)));
        drop(worker);
        let receive = self.response.lock().await.take().ok_or_else(|| {
            plugin_http_error("invalid_request", "HTTP response already consumed")
        })?;
        receive
            .await
            .map_err(|_| unsettled("HTTP worker exited before response"))?
    }
    async fn settle_effects(&self) -> Result<(), PluginRpcError> {
        self.cancellation.cancel();
        let mut worker = self.worker.lock().await;
        if self.failed.load(Ordering::Acquire) {
            return Err(unsettled("HTTP worker proof failed"));
        }
        if let Some(worker) = worker.as_mut() {
            let result = worker
                .await
                .map_err(|_| unsettled("HTTP worker panicked"))
                .and_then(|result| result);
            if let Err(error) = result {
                self.failed.store(true, Ordering::Release);
                return Err(error);
            }
        }
        // Join completed after private runtime destruction and proxy worker join.
        self.settled.store(true, Ordering::Release);
        worker.take();
        Ok(())
    }
}
fn unsettled(message: &str) -> PluginRpcError {
    plugin_http_error("effects_unsettled", message)
}

fn run_owned(input: HttpInput) -> Result<(), PluginRpcError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| unsettled("HTTP runtime could not be constructed"))?;
    let proxy = SupervisedEgressProxy::start(EgressPolicy::new(input.domains.iter()))
        .map_err(|_| unsettled("HTTP proxy could not be constructed"))?;
    runtime.block_on(transfer(input, &proxy));
    retire(runtime, proxy)
}

fn retire(
    runtime: tokio::runtime::Runtime,
    proxy: SupervisedEgressProxy,
) -> Result<(), PluginRpcError> {
    let proof = proxy.lifecycle();
    // This blocking owner retains the executor through DNS/blocking-task shutdown.
    // Dropping the outer response future cannot detach these effects.
    drop(runtime);
    drop(proxy);
    if !proof.is_stopped() {
        return Err(unsettled("HTTP proxy shutdown remains unproven"));
    }
    Ok(())
}

async fn transfer(input: HttpInput, proxy: &SupervisedEgressProxy) {
    let HttpInput {
        params,
        credentials,
        registrar,
        domains,
        cancellation,
        head,
    } = input;
    let (body_send, mut body_receive) = mpsc::channel::<BodyItem>(8);
    let response = tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(plugin_http_error("cancelled", "provider HTTP was cancelled")),
        result = open(params, &credentials, registrar.as_ref(), &domains, proxy) => result,
    };
    let mut response = match response {
        Ok(response) => response,
        Err(error) => {
            let _ = head.send(Err(error));
            return;
        }
    };
    let body = async_stream::stream! {
        while let Some(item) = body_receive.recv().await { yield item; }
    };
    if head
        .send(Ok(PluginHttpStreamResponse {
            status: response.status,
            headers: response.headers,
            body: Box::pin(body),
        }))
        .is_err()
    {
        return;
    }
    loop {
        let next = tokio::select! { biased;
            () = cancellation.cancelled() => return,
            next = response.body.next() => next,
        };
        let Some(next) = next else {
            return;
        };
        let next = next.map_err(|error| plugin_http_guard_error(&error));
        let failed = next.is_err();
        tokio::select! { biased;
            () = cancellation.cancelled() => return,
            sent = body_send.send(next) => if sent.is_err() { return; },
        }
        if failed {
            return;
        }
    }
}

async fn open(
    params: rw_plugin_protocol::ProviderHttpCapabilityParams,
    credentials: &CredentialManager,
    registrar: &dyn rw_providers::KnownSecretRegistrar,
    domains: &BTreeSet<String>,
    proxy: &SupervisedEgressProxy,
) -> Result<rw_providers::GuardedHttpStreamResponse, PluginRpcError> {
    let url = url::Url::parse(&params.request.url)
        .map_err(|_| plugin_http_error("invalid_request", "provider HTTP URL is invalid"))?;
    if !plugin_http_domain_allowed(domains, &url) {
        return Err(plugin_http_error(
            "domain_denied",
            "provider HTTP URL is outside the plugin allowed_domains policy",
        ));
    }
    let body = BASE64_STANDARD
        .decode(
            params
                .request
                .body_base64
                .as_deref()
                .unwrap_or("")
                .as_bytes(),
        )
        .map_err(|_| plugin_http_error("invalid_request", "provider HTTP body is invalid"))?;
    let mut headers = params
        .request
        .headers
        .into_iter()
        .map(|header| (header.name, header.value))
        .collect::<Vec<_>>();
    if headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(&params.request.credential_header))
    {
        return Err(plugin_http_error(
            "invalid_request",
            "provider HTTP credential header cannot also be plugin-supplied",
        ));
    }
    let resolved = credentials
        .resolve(&CredentialReference::new(&params.credential_reference))
        .map_err(|_| {
            plugin_http_error(
                "authentication",
                "provider HTTP credential reference could not be resolved",
            )
        })?;
    let secret = rw_providers::Secret::new(resolved.secret().expose_secret().clone());
    registrar.register(&secret);
    headers.push((
        params.request.credential_header,
        format!(
            "{}{}",
            params.request.credential_prefix.unwrap_or_default(),
            secret.expose_secret()
        ),
    ));
    let method = match params.request.method.as_str() {
        "GET" => rw_providers::GuardedHttpMethod::Get,
        "POST" => rw_providers::GuardedHttpMethod::Post,
        "DELETE" => rw_providers::GuardedHttpMethod::Delete,
        _ => {
            return Err(plugin_http_error(
                "invalid_request",
                "provider HTTP method is invalid",
            ));
        }
    };
    let guarded = rw_providers::GuardedHttpRequest {
        method,
        url,
        headers,
        body,
        proxy: url::Url::parse(&proxy.url()).ok(),
        proxy_authentication: None,
        dns_pin: None,
        allow_private_destinations: false,
        response_deadline: Duration::from_mins(5),
        frame_deadline: Duration::from_secs(30),
        max_frame_bytes: 256 * 1024,
        max_body_bytes: 64 * 1024 * 1024,
    };
    rw_providers::guarded_http_request(guarded)
        .await
        .map_err(|error| plugin_http_guard_error(&error))
}
pub(super) fn plugin_http_domain_allowed(
    allowed_domains: &BTreeSet<String>,
    url: &url::Url,
) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        allowed_domains.iter().any(|allowed| {
            host == *allowed
                || host
                    .strip_suffix(allowed)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    })
}

#[cfg(test)]
#[path = "provider_http_tests.rs"]
mod tests;
