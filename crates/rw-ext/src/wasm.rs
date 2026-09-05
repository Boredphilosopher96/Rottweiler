//! Capability-bounded WebAssembly component hooks.
//!
//! The component ABI intentionally has no host imports. A component receives a
//! hook name and bounded JSON payload through the exported `invoke` function and
//! returns a bounded JSON directive. Filesystem, network, process, environment,
//! clock, randomness, and credential access therefore do not exist at this tier.

#[cfg(feature = "wasm-runtime")]
use std::sync::Arc;
use std::{io::Write, time::Duration};

#[cfg(feature = "wasm-runtime")]
use async_trait::async_trait;
use rw_plugin_protocol::ManifestError;
#[cfg(feature = "wasm-runtime")]
use rw_plugin_protocol::PluginManifest;
use serde::{Deserialize, Serialize};

use thiserror::Error;
#[cfg(feature = "wasm-runtime")]
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Linker},
};

#[cfg(feature = "wasm-runtime")]
use crate::{
    HookDirective, HookDispatcher, HookError, HookHandler, HookInvocation, HookRegistrationError,
};

/// Stable component export used by every WASM hook extension.
pub const WASM_HOOK_EXPORT: &str = "invoke";

/// Runtime and wire limits applied independently to every invocation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WasmHookLimits {
    pub max_component_bytes: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub max_memory_bytes: usize,
    pub max_table_elements: usize,
    pub max_instances: usize,
    pub fuel: u64,
}

impl Default for WasmHookLimits {
    fn default() -> Self {
        Self {
            max_component_bytes: 8 * 1024 * 1024,
            max_input_bytes: 512 * 1024,
            max_output_bytes: 512 * 1024,
            // `StoreLimits::memory_size` is per linear memory, not aggregate.
            // Keep both the per-memory ceiling and memory count deliberately
            // small so one hook cannot reserve a gigabyte-scale store.
            max_memory_bytes: 16 * 1024 * 1024,
            max_table_elements: 10_000,
            max_instances: 4,
            fuel: 10_000_000,
        }
    }
}

#[derive(Debug, Error)]
pub enum WasmHookHostError {
    #[error("WASM component exceeds the {limit}-byte limit")]
    ComponentTooLarge { limit: usize },
    #[error("WASM extension manifest is invalid: {0}")]
    Manifest(#[from] ManifestError),
    #[error("WASM hook components may currently declare hooks only")]
    UnsupportedCapability,
    #[error("WASM component could not be compiled: {message}")]
    Compile { message: String },
    #[error("WASM component could not be instantiated: {message}")]
    Instantiate { message: String },
    #[error("WASM component does not export `{WASM_HOOK_EXPORT}` with the required signature")]
    MissingInvoke,
    #[error("WASM hook input exceeds the {limit}-byte limit")]
    InputTooLarge { limit: usize },
    #[error("WASM hook output exceeds the {limit}-byte limit")]
    OutputTooLarge { limit: usize },
    #[error("WASM hook execution failed: {message}")]
    Execution { message: String },
    #[error("WASM hook returned an invalid directive: {message}")]
    InvalidDirective { message: String },
}

#[cfg(feature = "wasm-runtime")]
struct WasmStoreState {
    limits: StoreLimits,
}

/// A compiled, immutable component that registers through the normal hook
/// dispatcher. Each invocation receives a fresh short-lived store and instance.
#[cfg(feature = "wasm-runtime")]
#[derive(Clone)]
pub struct WasmHookHost {
    manifest: PluginManifest,
    engine: Engine,
    component: Component,
    limits: WasmHookLimits,
}

#[cfg(feature = "wasm-runtime")]
impl WasmHookHost {
    /// Validates and compiles a component without executing it.
    ///
    /// # Errors
    /// Returns an error for invalid manifests, unsupported capabilities, or invalid components.
    pub fn from_bytes(
        manifest: PluginManifest,
        component_bytes: &[u8],
        limits: WasmHookLimits,
    ) -> Result<Self, WasmHookHostError> {
        manifest.validate()?;
        if component_bytes.len() > limits.max_component_bytes {
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

        let mut config = Config::new();
        // Apple-silicon hosted environments can enforce executable-memory
        // policy more strictly than an interactive shell. Pulley keeps the
        // private extension tier free of JIT/code-signing requirements while
        // preserving Wasmtime's component model, fuel, and store limits.
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        config
            .target("pulley64")
            .map_err(|error| WasmHookHostError::Compile {
                message: format!("{error:#}"),
            })?;
        config.wasm_component_model(true);
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|error| WasmHookHostError::Compile {
            message: format!("{error:#}"),
        })?;
        let component = Component::from_binary(&engine, component_bytes).map_err(|error| {
            WasmHookHostError::Compile {
                message: format!("{error:#}"),
            }
        })?;
        Ok(Self {
            manifest,
            engine,
            component,
            limits,
        })
    }

    #[must_use]
    pub const fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    /// Registers every manifest hook through [`HookDispatcher`].
    ///
    /// # Errors
    /// Returns an error when a registration is invalid or duplicates an existing hook.
    pub fn register_hooks(
        &self,
        dispatcher: &mut HookDispatcher,
    ) -> Result<(), HookRegistrationError> {
        let shared: Arc<dyn HookHandler> = Arc::new(self.clone());
        for declaration in &self.manifest.capabilities.hooks {
            let hook = declaration.name;
            let id = format!("wasm:{}:{}", self.manifest.name, hook.as_str());
            dispatcher.register_shared(
                crate::plugin_hook_registration(*declaration, id),
                Arc::clone(&shared),
            )?;
        }
        Ok(())
    }

    /// Invokes the compiled component through its bounded typed export.
    ///
    /// # Errors
    /// Returns an error when instantiation, execution, or directive validation fails.
    pub async fn invoke_json(
        &self,
        event: &str,
        input: &str,
    ) -> Result<HookDirective, WasmHookHostError> {
        if input.len() > self.limits.max_input_bytes {
            return Err(WasmHookHostError::InputTooLarge {
                limit: self.limits.max_input_bytes,
            });
        }
        let store_limits = StoreLimitsBuilder::new()
            .memory_size(self.limits.max_memory_bytes)
            .table_elements(self.limits.max_table_elements)
            .instances(self.limits.max_instances)
            .memories(2)
            .tables(4)
            .build();
        let mut store = Store::new(
            &self.engine,
            WasmStoreState {
                limits: store_limits,
            },
        );
        store.limiter(|state| &mut state.limits);
        store
            .set_fuel(self.limits.fuel)
            .map_err(|error| WasmHookHostError::Execution {
                message: error.to_string(),
            })?;
        store
            .fuel_async_yield_interval(Some(50_000))
            .map_err(|error| WasmHookHostError::Execution {
                message: error.to_string(),
            })?;
        let linker = Linker::new(&self.engine);
        let instance = linker
            .instantiate_async(&mut store, &self.component)
            .await
            .map_err(|error| WasmHookHostError::Instantiate {
                message: error.to_string(),
            })?;
        let invoke = instance
            .get_typed_func::<(String, String), (String,)>(&mut store, WASM_HOOK_EXPORT)
            .map_err(|_| WasmHookHostError::MissingInvoke)?;
        let (output,) = invoke
            .call_async(&mut store, (event.to_owned(), input.to_owned()))
            .await
            .map_err(|error| WasmHookHostError::Execution {
                message: error.to_string(),
            })?;
        parse_directive(&output, self.limits.max_output_bytes)
    }
}

#[cfg(feature = "wasm-runtime")]
#[async_trait]
impl HookHandler for WasmHookHost {
    async fn settle_effects(&self) -> std::result::Result<(), crate::HookError> {
        Ok(())
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        if invocation.cancellation().is_cancelled() {
            return Err(HookError::new("cancelled", "WASM hook was cancelled"));
        }
        let event = invocation.event().as_str().to_owned();
        let input = encode_input(invocation.input(), self.limits.max_input_bytes)
            .map_err(|error| HookError::new("wasm_input", error.to_string()))?;
        tokio::select! {
            result = self.invoke_json(&event, &input) => {
                result.map_err(|error| HookError::new("wasm_hook", error.to_string()))
            }
            () = invocation.cancellation().cancelled() => {
                Err(HookError::new("cancelled", "WASM hook was cancelled"))
            }
        }
    }
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) > self.limit {
            return Err(std::io::Error::other(
                "hook payload exceeds the configured limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn encode_input(
    payload: &impl Serialize,
    limit: usize,
) -> Result<String, WasmHookHostError> {
    let mut writer = BoundedJsonWriter {
        bytes: Vec::with_capacity(limit.min(16 * 1024)),
        limit,
    };
    serde_json::to_writer(&mut writer, payload).map_err(|error| {
        if writer.bytes.len() >= limit || error.is_io() {
            WasmHookHostError::InputTooLarge { limit }
        } else {
            WasmHookHostError::Execution {
                message: format!("hook payload could not encode: {error}"),
            }
        }
    })?;
    String::from_utf8(writer.bytes).map_err(|error| WasmHookHostError::Execution {
        message: format!("hook payload was not UTF-8: {error}"),
    })
}

#[cfg(feature = "wasm-runtime")]
pub(crate) fn parse_directive(
    output: &str,
    max_output_bytes: usize,
) -> Result<HookDirective, WasmHookHostError> {
    if output.len() > max_output_bytes {
        return Err(WasmHookHostError::OutputTooLarge {
            limit: max_output_bytes,
        });
    }
    serde_json::from_str(output).map_err(|error| WasmHookHostError::InvalidDirective {
        message: error.to_string(),
    })
}

/// WIT contract authors can use to compile a compatible component.
pub const WASM_HOOK_WIT: &str = r"package rottweiler:extension@1.0.0;

world hook-extension {
  export invoke: func(event: string, payload-json: string) -> string;
}
";

/// The dispatcher owns the user-visible deadline. Fuel bounds CPU work even if
/// the handler future is dropped after that deadline.
pub const DEFAULT_WASM_HOOK_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(all(test, feature = "wasm-runtime"))]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn manifest() -> PluginManifest {
        PluginManifest {
            name: "wasm-test".to_owned(),
            version: "1.0.0".to_owned(),
            protocol: rw_plugin_protocol::PROTOCOL_VERSION,
            capabilities: rw_plugin_protocol::PluginCapabilities {
                hooks: vec![rw_plugin_protocol::PluginHookCapability {
                    name: rw_plugin_protocol::HookEvent::PreTool,
                    class: rw_types::hook_contract::HookClass::Transform,
                    failure_policy: rw_plugin_protocol::HookFailurePolicy::FailOpen,
                }],
                ..rw_plugin_protocol::PluginCapabilities::default()
            },
        }
    }

    #[test]
    fn wire_directives_are_typed_and_bounded() {
        assert_eq!(
            parse_directive(r#"{"decision":"continue"}"#, 1_024).expect("continue"),
            HookDirective::Continue {}
        );
        assert_eq!(
            parse_directive(r#"{"decision":"transform","change":{"hook":"pre_tool","name":"read","arguments":{"ok":true}}}"#, 1_024)
                .expect("replace"),
            HookDirective::Transform { change: crate::HookTransform::PreTool { name: "read".to_owned(), arguments: serde_json::json!({"ok": true}) } }
        );
        assert!(parse_directive(r#"{"decision":"continue","message":"foreign"}"#, 1_024).is_err());
        assert!(parse_directive(r#"{"decision":"continue"}"#, 4).is_err());
    }

    #[test]
    fn malformed_component_never_executes() {
        assert!(
            WasmHookHost::from_bytes(manifest(), b"not wasm", WasmHookLimits::default()).is_err()
        );
    }

    #[tokio::test]
    async fn real_component_invokes_through_the_typed_abi() {
        let output = r#"{"decision":"transform","change":{"hook":"pre_tool","name":"read","arguments":{"reviewed":true}}}"#;
        let component = wat::parse_str(format!(
            r#"(component
              (type $hook (func (param "event" string) (param "payload-json" string) (result string)))
              (core module $module
                (memory (export "memory") 1)
                (global $heap (mut i32) (i32.const 4096))
                (func (export "realloc") (param i32 i32 i32 i32) (result i32)
                  (local $result i32)
                  global.get $heap
                  local.tee $result
                  local.get 3
                  i32.add
                  global.set $heap
                  local.get $result)
                (data (i32.const 256) "{}")
                (func (export "invoke") (param i32 i32 i32 i32) (result i32)
                  i32.const 128
                  i32.const 256
                  i32.store
                  i32.const 132
                  i32.const {}
                  i32.store
                  i32.const 128))
              (core instance $instance (instantiate $module))
              (func $invoke (type $hook)
                (canon lift (core func $instance "invoke")
                  (memory $instance "memory")
                  (realloc (func $instance "realloc"))))
              (export "invoke" (func $invoke)))"#,
            output.replace('"', "\\22"),
            output.len()
        ))
        .expect("component WAT");
        let host = WasmHookHost::from_bytes(manifest(), &component, WasmHookLimits::default())
            .expect("compiled component");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert!(host.engine.is_pulley());
        assert_eq!(
            host.invoke_json("pre_tool", r#"{"tool":"read"}"#)
                .await
                .expect("directive"),
            HookDirective::Transform {
                change: crate::HookTransform::PreTool {
                    name: "read".to_owned(),
                    arguments: serde_json::json!({"reviewed": true})
                }
            }
        );
    }
}
