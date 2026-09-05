//! Bounded protocol and adapters for the private WASM worker pool.
mod pool;
pub use pool::{WasmWorkerPool, WasmWorkerStats};

use std::{path::PathBuf, process::Stdio, sync::Arc, time::Duration};

use async_trait::async_trait;
use rw_plugin_protocol::PluginManifest;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    HookDirective, HookDispatcher, HookError, HookHandler, HookInvocation, HookRegistrationError,
    WasmHookHostError, WasmHookLimits, encode_input,
};

pub const MAX_WASM_HOST_HEADER_BYTES: usize = 1024 * 1024;
pub const MAX_WASM_HOST_RESPONSE_BYTES: usize = 1024 * 1024;
pub const WASM_HOST_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WasmHostRequest {
    Load {
        manifest: Box<PluginManifest>,
        limits: WasmHookLimits,
    },
    Invoke {
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
    pool: Arc<WasmWorkerPool>,
    generation: Arc<pool::Generation>,
}

impl WasmProcessHook {
    /// Checks manifest/capability and wire limits without starting the helper.
    /// Compilation happens inside the helper on validation or first invocation.
    ///
    /// # Errors
    /// Returns an error for invalid manifests, capabilities, or component size.
    pub fn new(
        pool: Arc<WasmWorkerPool>,
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
        let mut hash = blake3::Hasher::new();
        hash.update(blake3::hash(&component).as_bytes());
        let identity = serde_json::to_vec(&(manifest.clone(), limits)).map_err(|error| {
            WasmHookHostError::Compile {
                message: error.to_string(),
            }
        })?;
        hash.update(&identity);
        Ok(Self {
            pool,
            generation: Arc::new(pool::Generation {
                helper,
                manifest,
                component: component.into(),
                limits,
                digest: hash.finalize(),
                jobs: std::sync::Mutex::default(),
            }),
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
        for declaration in &self.generation.manifest.capabilities.hooks {
            let hook = declaration.name;
            let id = format!("wasm:{}:{}", self.generation.manifest.name, hook.as_str());
            dispatcher.register_shared(
                crate::plugin_hook_registration(*declaration, id),
                Arc::clone(&shared),
            )?;
        }
        Ok(())
    }

    /// Compiles the component in the helper without executing it.
    ///
    /// # Errors
    /// Returns an error when the helper cannot start or rejects the component.
    pub async fn validate(&self) -> Result<(), WasmHookHostError> {
        let response = self
            .pool
            .request(
                Arc::clone(&self.generation),
                None,
                WASM_HOST_REQUEST_TIMEOUT,
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
    async fn settle_effects(&self) -> Result<(), HookError> {
        self.generation
            .settle()
            .await
            .map_err(|error| HookError::new("effects_unsettled", error.to_string()))
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        if invocation.cancellation().is_cancelled() {
            return Err(HookError::new("cancelled", "WASM hook was cancelled"));
        }
        let input = encode_input(invocation.payload(), self.generation.limits.max_input_bytes)
            .map_err(|error| HookError::new("wasm_input", error.to_string()))?;
        let event = rw_plugin_protocol::PluginHook::from(invocation.event())
            .as_str()
            .to_owned();
        tokio::select! {
            result = self.pool.request(Arc::clone(&self.generation), Some((event, input)), WASM_HOST_REQUEST_TIMEOUT) => {
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

fn helper_deadline_error() -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: "private WASM helper exceeded its request deadline".to_owned(),
    }
}
fn io_error(error: &std::io::Error) -> WasmHookHostError {
    WasmHookHostError::Execution {
        message: format!("private WASM helper communication failed: {error}"),
    }
}
