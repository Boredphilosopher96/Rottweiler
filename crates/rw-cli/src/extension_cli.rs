//! Signed WebAssembly extension registry commands.

use std::{
    io::{self, IsTerminal as _, Write as _},
    path::Path,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use miette::{IntoDiagnostic as _, Result, miette};
use rw_core::{prepare_update_network, runtime_support};
use url::Url;

const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_COMPONENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) async fn list_registry(source: &str) -> Result<()> {
    let catalog = fetch_catalog(source).await?;
    let mut releases = catalog.releases.iter().collect::<Vec<_>>();
    releases.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.version.cmp(&right.version))
    });
    for release in releases {
        println!(
            "{} {}  hooks={}  publisher={}",
            terminal_text(&release.name),
            terminal_text(&release.version),
            release.manifest.capabilities.hooks.len(),
            terminal_text(short_key(&release.publisher_key))
        );
    }
    Ok(())
}

pub(crate) async fn install_registry_release(
    store: &Path,
    source: &str,
    name: &str,
    version: Option<&str>,
    publisher_key: &str,
) -> Result<runtime_support::InstalledWasmExtension> {
    let catalog = fetch_catalog(source).await?;
    let release = match version {
        Some(version) => catalog
            .releases
            .iter()
            .find(|release| release.name == name && release.version == version),
        None => catalog.latest(name),
    }
    .ok_or_else(|| miette!("extension release was not found in the fetched registry"))?;
    let trusted_key = decode_publisher_key(publisher_key)?;
    release
        .verify(&trusted_key)
        .map_err(|error| miette!(error.to_string()))?;
    let network = prepare_update_network().map_err(|error| miette!(error.to_string()))?;
    for warning in network.warnings() {
        eprintln!("warning: {warning}");
    }
    let component_url = Url::parse(&release.component.url).into_diagnostic()?;
    let component = network
        .fetch(&component_url, MAX_COMPONENT_BYTES, Duration::from_mins(2))
        .await
        .map_err(|error| miette!(error.to_string()))?;
    let installed =
        runtime_support::install_verified_component(store, release, &trusted_key, &component)
            .map_err(|error| miette!(error.to_string()))?;
    println!(
        "installed {} {} (inactive); run `rw extension enable {} {}` after reviewing its capabilities",
        release.name, release.version, release.name, release.version
    );
    Ok(installed)
}

pub(crate) fn status(store: &Path) -> Result<()> {
    let installed = runtime_support::list_installed_wasm_extensions(store)
        .map_err(|error| miette!(error.to_string()))?;
    if installed.is_empty() {
        println!("No WASM extensions installed.");
        return Ok(());
    }
    for extension in installed {
        if let Some(problem) = extension.problem {
            println!(
                "{} {}  {}  invalid: {}",
                terminal_text(&extension.name),
                terminal_text(&extension.version),
                if extension.enabled {
                    "enabled"
                } else {
                    "inactive"
                },
                terminal_text(&problem)
            );
            continue;
        }
        println!(
            "{} {}  {}  manifest={}  component={}",
            terminal_text(&extension.name),
            terminal_text(&extension.version),
            if extension.enabled {
                "enabled"
            } else {
                "inactive"
            },
            &extension.manifest_fingerprint[..12],
            &extension.component_blake3[..12]
        );
    }
    Ok(())
}

pub(crate) async fn enable(store: &Path, name: &str, version: &str, yes: bool) -> Result<()> {
    let manifest = runtime_support::inspect_installed_wasm_extension(store, name, version)
        .map_err(|error| miette!(error.to_string()))?;
    let summary = serde_json::to_string_pretty(&manifest).into_diagnostic()?;
    println!("Exact extension capabilities to enable:\n{summary}");
    if !yes {
        if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(miette!(
                "refusing non-interactive extension activation without --yes"
            ));
        }
        eprint!("Enable this exact installed extension? [y/N] ");
        io::stderr().flush().into_diagnostic()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).into_diagnostic()?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Err(miette!("extension activation was not granted"));
        }
    }
    let (verified_manifest, component) =
        runtime_support::load_installed_wasm_extension(store, name, version)
            .map_err(|error| miette!(error.to_string()))?;
    let helper = crate::runtime::locate_wasm_host_executable()?;
    runtime_support::WasmProcessHook::new(
        helper,
        verified_manifest,
        component,
        runtime_support::WasmHookLimits::default(),
    )
    .map_err(|error| miette!(error.to_string()))?
    .validate()
    .await
    .map_err(|error| miette!(error.to_string()))?;
    let activation = runtime_support::activate_installed_wasm_extension(store, name, version)
        .map_err(|error| miette!(error.to_string()))?;
    println!(
        "enabled {} {}; restart active sessions to load it",
        activation.name, activation.version
    );
    Ok(())
}

pub(crate) fn disable(store: &Path, name: &str) -> Result<()> {
    if runtime_support::deactivate_wasm_extension(store, name)
        .map_err(|error| miette!(error.to_string()))?
    {
        println!("disabled {name}; restart active sessions to unload it");
    } else {
        println!("extension {name} was not enabled");
    }
    Ok(())
}

async fn fetch_catalog(source: &str) -> Result<runtime_support::ExtensionRegistryCatalog> {
    let source = Url::parse(source).into_diagnostic()?;
    let network = prepare_update_network().map_err(|error| miette!(error.to_string()))?;
    for warning in network.warnings() {
        eprintln!("warning: {warning}");
    }
    let bytes = network
        .fetch(&source, MAX_CATALOG_BYTES, Duration::from_secs(30))
        .await
        .map_err(|error| miette!(error.to_string()))?;
    runtime_support::ExtensionRegistryCatalog::from_slice(&bytes)
        .map_err(|error| miette!(error.to_string()))
}

fn decode_publisher_key(value: &str) -> Result<[u8; 32]> {
    STANDARD_NO_PAD
        .decode(value)
        .map_err(|_| miette!("publisher key must be unpadded base64"))?
        .try_into()
        .map_err(|_| miette!("publisher key must encode exactly 32 bytes"))
}

fn short_key(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .flat_map(char::escape_default)
        .collect::<String>()
}
