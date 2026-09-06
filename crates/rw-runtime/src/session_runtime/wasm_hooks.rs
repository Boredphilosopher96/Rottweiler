use super::declarative_hooks::register_declarative_hooks;
use super::runtime_options::display_agent_error;
use super::toolchain::LspDiagnosticsHook;
use super::toolchain::ToolchainHook;
use super::toolchain::ToolchainRuntime;
use super::toolchain::ToolchainTestHook;
use miette::IntoDiagnostic;
use miette::Result;
use miette::miette;
use rw_core::StartupNotification;
use rw_core::builtin_hook_dispatcher;
use rw_ext::ExtensionCatalog;
use rw_ext::HookDispatcher;
use rw_ext::HookEffect;
use rw_ext::HookEvent;
use rw_ext::HookFailurePolicy;
use rw_ext::HookRegistration;
use rw_ext::WasmHookLimits;
use rw_ext::WasmProcessHook;
use rw_ext::load_active_wasm_extensions_report;
use rw_tools::CodeIntelligenceProvider;
use rw_tools::ToolBehavior;
use rw_tools::ToolRegistry;
use rw_types::ToolCapability;
use rw_types::config::ToolchainConfig;
use rw_types::hook_contract::HookClass;
use std::io::Read as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) fn compose_runtime_hooks_with_extensions(
    config: &ToolchainConfig,
    runtime: &Arc<ToolchainRuntime>,
    tools: Arc<ToolRegistry>,
    catalog: &ExtensionCatalog,
    intelligence: Arc<dyn CodeIntelligenceProvider>,
    validated_wasm_hooks: &[NamedWasmHook],
) -> Result<HookDispatcher> {
    let mut hooks = compose_runtime_hooks(config, Arc::clone(runtime), tools, Some(intelligence))?;
    register_declarative_hooks(&mut hooks, catalog, runtime)?;
    register_retained_wasm_hooks(&mut hooks, validated_wasm_hooks)?;
    Ok(hooks)
}

pub(super) async fn compose_runtime_hooks_with_extensions_validated(
    wasm_workers: Arc<rw_ext::WasmWorkerPool>,
    config: &ToolchainConfig,
    runtime: &Arc<ToolchainRuntime>,
    tools: Arc<ToolRegistry>,
    catalog: &ExtensionCatalog,
    intelligence: Arc<dyn CodeIntelligenceProvider>,
) -> Result<(
    HookDispatcher,
    Vec<StartupNotification>,
    Arc<[NamedWasmHook]>,
)> {
    let mut hooks = compose_runtime_hooks(config, Arc::clone(runtime), tools, Some(intelligence))?;
    register_declarative_hooks(&mut hooks, catalog, runtime)?;
    let (validated_wasm_hooks, notices) =
        register_validated_wasm_hooks(wasm_workers, &mut hooks).await?;
    Ok((hooks, notices, validated_wasm_hooks.into()))
}

pub(super) async fn register_validated_wasm_hooks(
    wasm_workers: Arc<rw_ext::WasmWorkerPool>,
    dispatcher: &mut HookDispatcher,
) -> Result<(Vec<NamedWasmHook>, Vec<StartupNotification>)> {
    let (hosts, mut notices) = load_active_wasm_hook_proxies(&wasm_workers)?;
    let mut validated = Vec::new();
    for (name, host) in hosts {
        if host.validate().await.is_err() {
            notices.push(wasm_startup_notice(
                &format!("wasm:{name}"),
                &format!("Extension {name} was skipped because its component failed validation."),
            ));
            continue;
        }
        if host.register_hooks(dispatcher).is_err() {
            notices.push(wasm_startup_notice(
                &format!("wasm:{name}"),
                &format!(
                    "Extension {name} was skipped because its hooks conflict with another extension."
                ),
            ));
            continue;
        }
        validated.push((name, host));
    }
    Ok((validated, notices))
}

pub(super) fn register_retained_wasm_hooks(
    dispatcher: &mut HookDispatcher,
    validated_wasm_hooks: &[NamedWasmHook],
) -> Result<()> {
    for (name, host) in validated_wasm_hooks {
        host.register_hooks(dispatcher).map_err(|error| {
            miette!("validated WASM extension `{name}` could not re-register: {error}")
        })?;
    }
    Ok(())
}

pub(super) type NamedWasmHook = (String, WasmProcessHook);

pub(super) type WasmHookProxyLoad = (Vec<NamedWasmHook>, Vec<StartupNotification>);

pub(super) fn load_active_wasm_hook_proxies(
    wasm_workers: &Arc<rw_ext::WasmWorkerPool>,
) -> Result<WasmHookProxyLoad> {
    let mut notices = Vec::new();
    let mut hosts = Vec::new();
    let loader = rw_store::config::ConfigLoader::from_environment()
        .map_err(|error| miette!("extension configuration root is invalid: {error}"))?;
    let Some(configuration_root) = loader.credentials_path().parent().map(Path::to_path_buf) else {
        return Ok((hosts, notices));
    };
    let root = configuration_root.join("extensions");
    if !root.exists() {
        return Ok((hosts, notices));
    }
    let Ok(report) = load_active_wasm_extensions_report(&root) else {
        notices.push(wasm_startup_notice(
            "wasm-runtime",
            "WASM extensions are disabled because the activation ledger is invalid.",
        ));
        return Ok((hosts, notices));
    };
    for warning in report.warnings {
        notices.push(wasm_startup_notice("wasm-runtime", &warning));
    }
    if report.extensions.is_empty() {
        return Ok((hosts, notices));
    }
    let Ok(helper) = locate_wasm_host_executable() else {
        notices.push(wasm_startup_notice(
            "wasm-runtime",
            "Enabled WASM extensions are unavailable because the bundled runtime helper could not start.",
        ));
        return Ok((hosts, notices));
    };
    for (manifest, component) in report.extensions {
        let name = manifest.name.clone();
        let Ok(host) = WasmProcessHook::new(
            Arc::clone(wasm_workers),
            helper.clone(),
            manifest,
            component,
            WasmHookLimits::default(),
        ) else {
            notices.push(wasm_startup_notice(
                &format!("wasm:{name}"),
                &format!("Extension {name} was skipped because its manifest is invalid."),
            ));
            continue;
        };
        hosts.push((name, host));
    }
    Ok((hosts, notices))
}

pub(super) fn wasm_startup_notice(plugin_id: &str, message: &str) -> StartupNotification {
    StartupNotification {
        plugin_id: sanitized_wasm_notice_text(plugin_id, 160),
        status: "unavailable".to_owned(),
        title: "WASM extension unavailable".to_owned(),
        message: sanitized_wasm_notice_text(message, 512),
    }
}

pub(super) fn sanitized_wasm_notice_text(value: &str, limit: usize) -> String {
    let mut text = String::with_capacity(value.len().min(limit));
    for character in value.chars().filter(|character| !character.is_control()) {
        if text.len().saturating_add(character.len_utf8()) > limit {
            break;
        }
        text.push(character);
    }
    text
}

/// Resolves the bundled private WASM host executable.
///
/// # Errors
/// Returns an error when no safe executable candidate can be located.
pub fn locate_wasm_host_executable() -> Result<rw_tools::ApprovedExecutable> {
    let receipt = if let Some(path) = std::env::var_os("ROTTWEILER_WASM_HOST_RECEIPT") {
        PathBuf::from(path)
    } else {
        let current = std::env::current_exe().into_diagnostic()?;
        let installed = current.canonicalize().into_diagnostic()?;
        installed
            .parent()
            .ok_or_else(|| miette!("WASM bundle directory is missing"))?
            .join("rottweiler-wasm-host.identity.json")
    };
    let receipt = receipt.canonicalize().into_diagnostic()?;
    let directory = receipt
        .parent()
        .ok_or_else(|| miette!("WASM receipt directory is missing"))?;
    let mut bytes = Vec::new();
    std::fs::File::open(&receipt)
        .into_diagnostic()?
        .take(4097)
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    if bytes.len() > 4096 {
        return Err(miette!(
            "WASM helper identity receipt exceeds its byte limit"
        ));
    }
    let identity: rw_tools::ExecutableDigest = serde_json::from_slice(&bytes).into_diagnostic()?;
    rw_tools::ApprovedExecutable::from_installed(&directory.join("rottweiler-wasm-host"), &identity)
        .into_diagnostic()
}

pub(super) fn compose_runtime_hooks(
    config: &ToolchainConfig,
    runtime: Arc<ToolchainRuntime>,
    tools: Arc<ToolRegistry>,
    intelligence: Option<Arc<dyn CodeIntelligenceProvider>>,
) -> Result<HookDispatcher> {
    let mut hooks = builtin_hook_dispatcher().map_err(display_agent_error)?;
    let has_commands = config.formatter.is_some()
        || !config.linters.is_empty()
        || config
            .rules
            .iter()
            .any(|rule| rule.formatter.is_some() || !rule.linters.is_empty());
    if has_commands {
        let applicable_tools = tools
            .names_with_behavior(ToolBehavior::FileMutation)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        hooks
            .register(
                HookRegistration::new(
                    "builtin.toolchain",
                    HookEvent::PostTool,
                    HookClass::Transform,
                )
                .with_priority(100)
                .with_failure_policy(HookFailurePolicy::FailClosed)
                .with_effect(HookEffect::WorkspaceMutating)
                .with_applicable_tools(applicable_tools)
                .with_required_capabilities([ToolCapability::Execute])
                .with_timeout(std::time::Duration::from_mins(2)),
                ToolchainHook::compile(config, Arc::clone(&runtime), Arc::clone(&tools))?,
            )
            .map_err(|error| miette!("toolchain hook could not register: {error}"))?;
    }
    if let Some(command) = config.test.clone() {
        hooks
            .register(
                HookRegistration::new(
                    "builtin.toolchain_test",
                    HookEvent::TurnEnd,
                    HookClass::Policy,
                )
                .with_priority(100)
                .with_failure_policy(HookFailurePolicy::FailClosed)
                .with_effect(HookEffect::WorkspaceMutating)
                .with_required_capabilities([ToolCapability::Execute])
                .with_timeout(std::time::Duration::from_mins(10)),
                ToolchainTestHook {
                    command,
                    runtime: Arc::clone(&runtime),
                },
            )
            .map_err(|error| miette!("toolchain test hook could not register: {error}"))?;
    }
    if let Some(intelligence) = intelligence {
        let applicable_tools = tools
            .names_with_behavior(ToolBehavior::FileMutation)
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        hooks
            .register(
                HookRegistration::new(
                    "builtin.lsp_diagnostics",
                    HookEvent::PostTool,
                    HookClass::Transform,
                )
                .with_priority(200)
                .with_failure_policy(HookFailurePolicy::FailOpen)
                .with_applicable_tools(applicable_tools)
                .with_required_capabilities([ToolCapability::Execute])
                .with_timeout(std::time::Duration::from_secs(15)),
                LspDiagnosticsHook {
                    intelligence,
                    runtime,
                    tools,
                },
            )
            .map_err(|error| miette!("LSP diagnostics hook could not register: {error}"))?;
    }
    Ok(hooks)
}
