//! Bounded one-shot protocol for the private WASM helper process.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    HookDirective, HookDispatcher, HookError, HookHandler, HookInvocation, HookRegistrationError,
    PluginManifest, WasmHookHostError, WasmHookLimits, encode_input,
};

pub const MAX_WASM_HOST_HEADER_BYTES: usize = 1024 * 1024;
pub const MAX_WASM_HOST_RESPONSE_BYTES: usize = 1024 * 1024;
pub const WASM_HOST_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const WASM_HOST_REAP_TIMEOUT: Duration = Duration::from_secs(1);
static WASM_HELPER_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WasmHostRequest {
    Validate {
        manifest: PluginManifest,
        limits: WasmHookLimits,
    },
    Invoke {
        manifest: PluginManifest,
        limits: WasmHookLimits,
        event: String,
        input: String,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum WasmHostResponse {
    Valid,
    Continue,
    Replace { payload: Value },
    Block { message: String },
    Error { message: String },
}

/// A hook adapter that starts the private runtime only when an enabled hook is
/// actually invoked. `rw` itself therefore has no Wasmtime dependency.
#[derive(Clone)]
pub struct WasmProcessHook {
    helper: PathBuf,
    manifest: PluginManifest,
    component: Arc<[u8]>,
    limits: WasmHookLimits,
}

impl WasmProcessHook {
    /// Checks manifest/capability and wire limits without starting the helper.
    /// Compilation happens inside the helper on validation or first invocation.
    ///
    /// # Errors
    /// Returns an error for invalid manifests, capabilities, or component size.
    pub fn new(
        helper: PathBuf,
        manifest: PluginManifest,
        component: Vec<u8>,
        limits: WasmHookLimits,
    ) -> Result<Self, WasmHookHostError> {
        manifest.validate()?;
        if component.len() > limits.max_component_bytes {
            return Err(WasmHookHostError::ComponentTooLarge {
                limit: limits.max_component_bytes,
            });
        }
        let capabilities = &manifest.capabilities;
        if !capabilities.tools.is_empty()
            || !capabilities.commands.is_empty()
            || !capabilities.providers.is_empty()
            || !capabilities.event_subscriptions.is_empty()
            || !capabilities.push.is_empty()
        {
            return Err(WasmHookHostError::UnsupportedCapability);
        }
        Ok(Self {
            helper,
            manifest,
            component: component.into(),
            limits,
        })
    }

    /// Registers the proxy through the same hook dispatcher as native hooks.
    ///
    /// # Errors
    /// Returns an error when a hook registration conflicts or is invalid.
    pub fn register_hooks(
        &self,
        dispatcher: &mut HookDispatcher,
    ) -> Result<(), HookRegistrationError> {
        let shared: Arc<dyn HookHandler> = Arc::new(self.clone());
        for declaration in &self.manifest.capabilities.hooks {
            let hook = declaration.name();
            let id = format!("wasm:{}:{}", self.manifest.name, hook.as_str());
            dispatcher.register_shared(declaration.registration(id), Arc::clone(&shared))?;
        }
        Ok(())
    }

    /// Compiles the component in the helper without executing it.
    ///
    /// # Errors
    /// Returns an error when the helper cannot start or rejects the component.
    pub async fn validate(&self) -> Result<(), WasmHookHostError> {
        let response = invoke_helper(
            &self.helper,
            &WasmHostRequest::Validate {
                manifest: self.manifest.clone(),
                limits: self.limits,
            },
            &self.component,
        )
        .await?;
        match response {
            WasmHostResponse::Valid => Ok(()),
            WasmHostResponse::Error { message } => Err(WasmHookHostError::Compile { message }),
            _ => Err(WasmHookHostError::Execution {
                message: "WASM helper returned an unexpected validation response".to_owned(),
            }),
        }
    }
}

#[async_trait]
impl HookHandler for WasmProcessHook {
    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        if invocation.cancellation().is_cancelled() {
            return Err(HookError::new("cancelled", "WASM hook was cancelled"));
        }
        let input = encode_input(invocation.payload(), self.limits.max_input_bytes)
            .map_err(|error| HookError::new("wasm_input", error.to_string()))?;
        let request = WasmHostRequest::Invoke {
            manifest: self.manifest.clone(),
            limits: self.limits,
            event: crate::PluginHook::from(invocation.event())
                .as_str()
                .to_owned(),
            input,
        };
        tokio::select! {
            result = invoke_helper(&self.helper, &request, &self.component) => {
                match result.map_err(|error| HookError::new("wasm_hook", error.to_string()))? {
                    WasmHostResponse::Continue => Ok(HookDirective::Continue),
                    WasmHostResponse::Replace { payload } => Ok(HookDirective::Replace(payload)),
                    WasmHostResponse::Block { message } => Ok(HookDirective::Block { message }),
                    WasmHostResponse::Error { message } => Err(HookError::new("wasm_hook", message)),
                    WasmHostResponse::Valid => Err(HookError::new("wasm_hook", "WASM helper returned an unexpected invocation response")),
                }
            }
            () = invocation.cancellation().cancelled() => {
                Err(HookError::new("cancelled", "WASM hook was cancelled"))
            }
        }
    }
}

/// Sends one bounded request and exact component byte sequence to the helper.
///
/// # Errors
/// Returns an error for wire-limit, process, I/O, or malformed-response failures.
pub async fn invoke_helper(
    helper: &std::path::Path,
    request: &WasmHostRequest,
    component: &[u8],
) -> Result<WasmHostResponse, WasmHookHostError> {
    invoke_helper_with_timeout(helper, request, component, WASM_HOST_REQUEST_TIMEOUT).await
}

async fn invoke_helper_with_timeout(
    helper: &std::path::Path,
    request: &WasmHostRequest,
    component: &[u8],
    request_timeout: Duration,
) -> Result<WasmHostResponse, WasmHookHostError> {
    let started = Instant::now();
    let _permit = tokio::time::timeout(request_timeout, WASM_HELPER_SLOT.acquire())
        .await
        .map_err(|_| helper_deadline_error())?
        .map_err(|_| WasmHookHostError::Execution {
            message: "private WASM helper is unavailable".to_owned(),
        })?;
    let remaining = request_timeout.saturating_sub(started.elapsed());
    if remaining.is_zero() {
        return Err(helper_deadline_error());
    }
    let header = serde_json::to_vec(request).map_err(|error| WasmHookHostError::Execution {
        message: format!("WASM helper request could not encode: {error}"),
    })?;
    if header.len() > MAX_WASM_HOST_HEADER_BYTES {
        return Err(WasmHookHostError::InputTooLarge {
            limit: MAX_WASM_HOST_HEADER_BYTES,
        });
    }
    let component_len =
        u32::try_from(component.len()).map_err(|_| WasmHookHostError::ComponentTooLarge {
            limit: u32::MAX as usize,
        })?;
    let header_len = u32::try_from(header.len()).map_err(|_| WasmHookHostError::InputTooLarge {
        limit: MAX_WASM_HOST_HEADER_BYTES,
    })?;
    let mut child = tokio::process::Command::new(helper)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| WasmHookHostError::Execution {
            message: format!("private WASM helper could not start: {error}"),
        })?;
    let exchange = async {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| WasmHookHostError::Execution {
                message: "private WASM helper stdin is unavailable".to_owned(),
            })?;
        stdin
            .write_all(&header_len.to_be_bytes())
            .await
            .map_err(|error| io_error(&error))?;
        stdin
            .write_all(&component_len.to_be_bytes())
            .await
            .map_err(|error| io_error(&error))?;
        stdin
            .write_all(&header)
            .await
            .map_err(|error| io_error(&error))?;
        stdin
            .write_all(component)
            .await
            .map_err(|error| io_error(&error))?;
        stdin.shutdown().await.map_err(|error| io_error(&error))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| WasmHookHostError::Execution {
                message: "private WASM helper stdout is unavailable".to_owned(),
            })?;
        let response_len = stdout.read_u32().await.map_err(|error| io_error(&error))? as usize;
        if response_len > MAX_WASM_HOST_RESPONSE_BYTES {
            return Err(WasmHookHostError::OutputTooLarge {
                limit: MAX_WASM_HOST_RESPONSE_BYTES,
            });
        }
        let mut response = vec![0; response_len];
        stdout
            .read_exact(&mut response)
            .await
            .map_err(|error| io_error(&error))?;
        let status = child.wait().await.map_err(|error| io_error(&error))?;
        if !status.success() {
            return Err(WasmHookHostError::Execution {
                message: "private WASM helper exited unsuccessfully".to_owned(),
            });
        }
        serde_json::from_slice(&response).map_err(|error| WasmHookHostError::InvalidDirective {
            message: format!("private WASM helper returned malformed output: {error}"),
        })
    };
    match tokio::time::timeout(remaining, exchange).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => {
            terminate_and_reap(&mut child).await;
            Err(error)
        }
        Err(_) => {
            terminate_and_reap(&mut child).await;
            Err(helper_deadline_error())
        }
    }
}

fn helper_deadline_error() -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: "private WASM helper exceeded its request deadline".to_owned(),
    }
}

async fn terminate_and_reap(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(WASM_HOST_REAP_TIMEOUT, child.wait()).await;
}

fn io_error(error: &std::io::Error) -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: format!("private WASM helper communication failed: {error}"),
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used)]
mod tests {
    use std::{os::unix::fs::PermissionsExt as _, time::Instant};

    use super::*;

    #[tokio::test]
    async fn hanging_helper_is_bounded_killed_and_reaped() {
        let fixture = tempfile::tempdir().expect("fixture");
        let helper = fixture.path().join("hanging-helper");
        let pid_file = fixture.path().join("helper.pid");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\nprintf '%s' \"$$\" > '{}'\ntrap '' TERM\nwhile :; do sleep 1; done\n",
                pid_file.display()
            ),
        )
        .expect("helper script");
        let mut permissions = std::fs::metadata(&helper)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&helper, permissions).expect("helper permissions");
        let request = WasmHostRequest::Validate {
            manifest: PluginManifest {
                name: "deadline-test".to_owned(),
                version: "1.0.0".to_owned(),
                protocol: crate::PROTOCOL_VERSION,
                capabilities: crate::PluginCapabilities::default(),
            },
            limits: WasmHookLimits::default(),
        };
        let started = Instant::now();
        let error =
            invoke_helper_with_timeout(&helper, &request, b"component", Duration::from_millis(200))
                .await
                .expect_err("hanging helper must time out");
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));

        let pid = std::fs::read_to_string(&pid_file)
            .expect("helper pid")
            .trim()
            .to_owned();
        let status = std::process::Command::new("kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("probe process");
        assert!(!status.success(), "timed-out helper must be reaped");
    }
}
