//! Release-owned TypeScript source preparation and identity.

use rw_types::release_contract::JS_HOST_SOURCE_PLUGIN_ROLE;

mod preparation;
use preparation::{PreparationOutput, PreparationRequest};
pub(crate) use preparation::{SourcePreparationBudget, SourcePreparations};

use std::{
    collections::BTreeSet,
    fs,
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use miette::{IntoDiagnostic as _, Result, miette};
use rw_ext::{PluginLauncher, PluginProcessConfig, SourcePluginIdentity};
use serde::{Deserialize, Serialize};

use crate::extension_config::{DiscoveredPlugin, DiscoveredPluginTarget};

const MAX_GRAPH_INPUTS: usize = 4_096;
const MAX_GRAPH_BYTES: u64 = 64 * 1024 * 1024;
const MAX_REPORT_BYTES: u64 = 1024 * 1024;
const MAX_BUNDLE_BYTES: u64 = 16 * 1024 * 1024;
const HOST_DEADLINE: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct GraphInput {
    path: String,
    bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct GraphReport {
    abi: u32,
    format: String,
    inputs: Vec<GraphInput>,
}

pub(crate) struct SourcePluginResolver {
    host: PathBuf,
    private_root: PathBuf,
    scratch: Arc<crate::extension_runtime::PrivateMcpScratch>,
    launcher: Arc<dyn PluginLauncher>,
    preparation: Arc<SourcePreparations>,
}

/// Resolves one discovered plugin into the exact process identity shown at approval.
///
/// # Errors
///
/// Returns an error when the source package, release host, sandbox, dependency
/// lock, discovered graph, or sealed bundle fails validation or preparation.
pub async fn resolve_plugin_process(
    plugin: &DiscoveredPlugin,
    private_root: &Path,
    helper: &rw_tools::SandboxHelper,
) -> Result<PluginProcessConfig> {
    if matches!(plugin.target, DiscoveredPluginTarget::Executable { .. }) {
        return plugin.executable_process_config();
    }
    let scratch = Arc::new(crate::extension_runtime::PrivateMcpScratch::create()?);
    let launcher: Arc<dyn PluginLauncher> = Arc::new(
        crate::plugin_process::SandboxedPluginLauncher::new(scratch.path(), helper)
            .map_err(|error| miette!(error.to_string()))?,
    );
    let host = helper
        .installation_path()
        .parent()
        .ok_or_else(|| miette!("Rottweiler executable has no release directory"))?
        .join(rw_types::release_contract::JS_HOST_EXECUTABLE_NAME);
    SourcePluginResolver::new(
        &host,
        private_root,
        scratch,
        launcher,
        Arc::new(SourcePreparationBudget::default()),
    )?
    .resolve(plugin)
    .await
}

impl SourcePluginResolver {
    pub(crate) fn new(
        host: &Path,
        private_root: &Path,
        scratch: Arc<crate::extension_runtime::PrivateMcpScratch>,
        launcher: Arc<dyn PluginLauncher>,
        budget: Arc<SourcePreparationBudget>,
    ) -> Result<Self> {
        let host = fs::canonicalize(host).into_diagnostic()?;
        if !host.is_file() {
            return Err(miette!(
                "the release-owned TypeScript plugin host is unavailable"
            ));
        }
        Ok(Self {
            host,
            private_root: private_root.to_path_buf(),
            scratch,
            launcher,
            preparation: Arc::new(SourcePreparations::new(budget)),
        })
    }

    pub(crate) fn preparation(&self) -> Arc<SourcePreparations> {
        Arc::clone(&self.preparation)
    }

    pub(crate) async fn resolve(&self, plugin: &DiscoveredPlugin) -> Result<PluginProcessConfig> {
        let DiscoveredPluginTarget::TypeScript {
            package_root,
            entry,
        } = &plugin.target
        else {
            return plugin.executable_process_config();
        };
        let package = package_root.join("package.json");
        let lockfile = package_root.join("bun.lock");
        for required in [&package, &lockfile, &plugin.manifest_path, entry] {
            require_regular_file(required)?;
        }
        let discovered = self.graph(package_root, entry).await?;
        validate_report(&discovered)?;

        let staging = self
            .scratch
            .path()
            .join(format!("source-{}", random_suffix()?));
        fs::create_dir(&staging).into_diagnostic()?;
        copy_graph(package_root, &staging, &discovered)?;
        let staged_entry = staging.join(entry.strip_prefix(package_root).into_diagnostic()?);
        let staged_package = staging.join("package.json");
        let staged_lockfile = staging.join("bun.lock");
        let package_bytes = read_bounded_nofollow(&staged_package, MAX_REPORT_BYTES)?;
        let lock_bytes = read_bounded_nofollow(&staged_lockfile, MAX_GRAPH_BYTES)?;
        validate_graph(
            &staging,
            &staged_entry,
            &discovered,
            &package_bytes,
            &lock_bytes,
        )?;
        let output = self
            .scratch
            .path()
            .join(format!("bundle-{}", random_suffix()?));
        fs::create_dir(&output).into_diagnostic()?;
        let rebuilt = self.bundle(&staging, &staged_entry, &output).await?;
        if rebuilt != discovered {
            return Err(miette!(
                "TypeScript plugin source graph changed during preparation"
            ));
        }
        let bundle = output.join("plugin.mjs");
        let bundle_bytes = read_bounded_nofollow(&bundle, MAX_BUNDLE_BYTES)?;
        let graph_blake3 = graph_digest(&staging, &discovered)?;
        let lockfile_blake3 = blake3::hash(&lock_bytes).to_hex().to_string();
        let bundle_blake3 = blake3::hash(&bundle_bytes).to_hex().to_string();
        let identity = SourcePluginIdentity {
            graph_blake3,
            lockfile_blake3,
            bundle_blake3,
            host_abi: discovered.abi,
            bundle_format: discovered.format.clone(),
        };
        let prepared = publish_bundle(&self.private_root, &identity, &bundle_bytes, &discovered)?;
        PluginProcessConfig::new(&self.host)
            .and_then(|config| {
                config.with_argv([
                    JS_HOST_SOURCE_PLUGIN_ROLE.to_owned(),
                    "run".to_owned(),
                    prepared.join("plugin.mjs").to_string_lossy().into_owned(),
                ])
            })
            .and_then(|config| config.with_cwd(&prepared))
            .and_then(|config| config.with_code_root(&prepared))
            .and_then(|config| {
                config
                    .with_attested_files([prepared.join("plugin.mjs"), prepared.join("graph.json")])
            })
            .and_then(|config| {
                config.with_environment_allowlist(plugin.inherit_env.iter().cloned())
            })
            .and_then(|config| config.with_allowed_domains(plugin.allowed_domains.iter().cloned()))
            .map(|config| config.with_source_identity(identity))
            .map_err(|error| miette!(error.to_string()))
    }

    async fn graph(&self, root: &Path, entry: &Path) -> Result<GraphReport> {
        self.invoke(
            root,
            None,
            [
                "graph".to_owned(),
                root.to_string_lossy().into_owned(),
                entry.to_string_lossy().into_owned(),
            ],
        )
        .await
    }

    async fn bundle(&self, root: &Path, entry: &Path, output: &Path) -> Result<GraphReport> {
        self.invoke(
            root,
            Some(output),
            [
                "bundle".to_owned(),
                root.to_string_lossy().into_owned(),
                entry.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
            ],
        )
        .await
    }

    async fn invoke<const N: usize>(
        &self,
        root: &Path,
        output_root: Option<&Path>,
        argv: [String; N],
    ) -> Result<GraphReport> {
        let config = PluginProcessConfig::new(&self.host)
            .and_then(|config| {
                config.with_argv(std::iter::once(JS_HOST_SOURCE_PLUGIN_ROLE.to_owned()).chain(argv))
            })
            .and_then(|config| config.with_cwd(root))
            .and_then(|config| config.with_code_root(root))
            .map_err(|error| miette!(error.to_string()))?;
        let PreparationOutput {
            stdout: output,
            stderr: errors,
            status,
        } = self
            .preparation
            .execute(
                PreparationRequest {
                    config,
                    output_root: output_root.map(Path::to_path_buf),
                    launcher: Arc::clone(&self.launcher),
                    scratch: Arc::clone(&self.scratch),
                },
                tokio::time::Instant::now() + HOST_DEADLINE,
            )
            .await?;
        if status != Some(0) {
            let error = String::from_utf8_lossy(&errors);
            return Err(miette!(
                "TypeScript plugin preparation failed: {}",
                error.trim()
            ));
        }
        let report: GraphReport = serde_json::from_slice(&output)
            .map_err(|_| miette!("TypeScript plugin host returned an invalid graph report"))?;
        validate_report(&report)?;
        Ok(report)
    }
}

fn validate_report(report: &GraphReport) -> Result<()> {
    if report.abi == 0
        || report.format.is_empty()
        || report.format.len() > 64
        || report
            .format
            .bytes()
            .any(|byte| !byte.is_ascii_lowercase() && !byte.is_ascii_digit() && byte != b'-')
    {
        return Err(miette!(
            "TypeScript plugin host ABI or bundle format is invalid"
        ));
    }
    if report.inputs.is_empty() || report.inputs.len() > MAX_GRAPH_INPUTS {
        return Err(miette!("TypeScript plugin graph input count is invalid"));
    }
    let mut previous: Option<&str> = None;
    let mut folded = BTreeSet::new();
    let mut total = 0_u64;
    for input in &report.inputs {
        validate_logical_path(&input.path)?;
        if previous.is_some_and(|value| value >= input.path.as_str())
            || !folded.insert(input.path.to_ascii_lowercase())
        {
            return Err(miette!(
                "TypeScript plugin graph paths are not unique and sorted"
            ));
        }
        total = total
            .checked_add(input.bytes)
            .ok_or_else(|| miette!("TypeScript plugin graph byte count overflowed"))?;
        previous = Some(&input.path);
    }
    if total > MAX_GRAPH_BYTES {
        return Err(miette!("TypeScript plugin graph exceeds its byte limit"));
    }
    Ok(())
}

fn validate_graph(
    root: &Path,
    entry: &Path,
    report: &GraphReport,
    package_bytes: &[u8],
    lock_bytes: &[u8],
) -> Result<()> {
    validate_report(report)?;
    let entry = entry
        .strip_prefix(root)
        .into_diagnostic()?
        .to_string_lossy()
        .replace('\\', "/");
    if !report.inputs.iter().any(|input| input.path == entry)
        || !report
            .inputs
            .iter()
            .any(|input| input.path == "manifest.json")
    {
        return Err(miette!(
            "TypeScript plugin graph omits its entrypoint or manifest"
        ));
    }
    let package: serde_json::Value = serde_json::from_slice(package_bytes)
        .map_err(|_| miette!("TypeScript plugin package.json is invalid"))?;
    let lock = parse_bun_lock(lock_bytes)?;
    let lock_packages = lock
        .get("packages")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| miette!("TypeScript plugin bun.lock has no package identity map"))?;
    let dependencies = package
        .get("dependencies")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| miette!("TypeScript plugin package.json has no runtime dependencies"))?;
    if !dependencies.contains_key("@rottweiler/plugin") {
        return Err(miette!(
            "TypeScript plugins must depend on @rottweiler/plugin"
        ));
    }
    for input in &report.inputs {
        let Some(package_name) = node_module_package(&input.path) else {
            continue;
        };
        let Some(identity) = lock_packages
            .get(&package_name)
            .and_then(serde_json::Value::as_array)
        else {
            return Err(miette!("TypeScript plugin import is absent from bun.lock"));
        };
        if !identity.iter().any(|value| {
            value
                .as_str()
                .is_some_and(|value| value.starts_with("sha512-"))
        }) {
            return Err(miette!(
                "TypeScript plugin dependency has no locked integrity"
            ));
        }
    }
    Ok(())
}

fn parse_bun_lock(bytes: &[u8]) -> Result<serde_json::Value> {
    // Bun's text lockfile is JSONC and emits trailing commas. Normalize only
    // that syntax; strings and every semantic value remain byte-for-byte data.
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            normalized.push(byte);
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        if byte == b'"' {
            in_string = true;
            normalized.push(byte);
            index += 1;
            continue;
        }
        if byte == b',' {
            let mut next = index + 1;
            while next < bytes.len() && bytes[next].is_ascii_whitespace() {
                next += 1;
            }
            if next < bytes.len() && matches!(bytes[next], b'}' | b']') {
                index += 1;
                continue;
            }
        }
        normalized.push(byte);
        index += 1;
    }
    serde_json::from_slice(&normalized)
        .map_err(|_| miette!("TypeScript plugin bun.lock is invalid"))
}

fn node_module_package(path: &str) -> Option<String> {
    let path = path.strip_prefix("node_modules/")?;
    let mut parts = path.split('/');
    let first = parts.next()?;
    if first.starts_with('@') {
        Some(format!("{first}/{}", parts.next()?))
    } else {
        Some(first.to_owned())
    }
}

fn validate_logical_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 4096
        || path.contains('\\')
        || Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(miette!(
            "TypeScript plugin graph contains an invalid logical path"
        ));
    }
    Ok(())
}

fn copy_graph(source: &Path, destination: &Path, report: &GraphReport) -> Result<()> {
    let mut files = report
        .inputs
        .iter()
        .map(|input| input.path.clone())
        .collect::<Vec<_>>();
    files.extend(["package.json".to_owned(), "bun.lock".to_owned()]);
    files.sort();
    files.dedup();
    for logical in files {
        let bytes = read_bounded_beneath(source, Path::new(&logical), MAX_GRAPH_BYTES)?;
        let target = destination.join(&logical);
        let parent = target
            .parent()
            .ok_or_else(|| miette!("invalid source graph target"))?;
        fs::create_dir_all(parent).into_diagnostic()?;
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)
            .into_diagnostic()?;
        file.write_all(&bytes).into_diagnostic()?;
        file.sync_all().into_diagnostic()?;
    }
    Ok(())
}

fn graph_digest(root: &Path, report: &GraphReport) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rottweiler-typescript-graph-v1\0");
    for input in &report.inputs {
        hasher.update(input.path.as_bytes());
        hasher.update(b"\0");
        hasher.update(&read_bounded_nofollow(
            &root.join(&input.path),
            MAX_GRAPH_BYTES,
        )?);
        hasher.update(b"\0");
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn publish_bundle(
    private_root: &Path,
    identity: &SourcePluginIdentity,
    bundle: &[u8],
    report: &GraphReport,
) -> Result<PathBuf> {
    let root = private_root.join("plugin-bundles").join("v1");
    fs::create_dir_all(&root).into_diagnostic()?;
    let key = format!("{}-{}", identity.graph_blake3, identity.bundle_blake3);
    let final_path = root.join(key);
    if final_path.is_dir() {
        if blake3::hash(&read_bounded_nofollow(
            &final_path.join("plugin.mjs"),
            MAX_BUNDLE_BYTES,
        )?)
        .to_hex()
        .as_str()
            == identity.bundle_blake3
        {
            return Ok(final_path);
        }
        return Err(miette!(
            "cached TypeScript plugin bundle failed identity validation"
        ));
    }
    let staging = root.join(format!(".prepare-{}", random_suffix()?));
    fs::create_dir(&staging).into_diagnostic()?;
    write_new_synced(&staging.join("plugin.mjs"), bundle)?;
    write_new_synced(
        &staging.join("graph.json"),
        &serde_json::to_vec(report).into_diagnostic()?,
    )?;
    fs::rename(&staging, &final_path).into_diagnostic()?;
    fs::File::open(&root)
        .and_then(|file| file.sync_all())
        .into_diagnostic()?;
    Ok(final_path)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .into_diagnostic()?;
    file.write_all(bytes).into_diagnostic()?;
    file.sync_all().into_diagnostic()
}

fn require_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path).into_diagnostic()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(miette!(
            "TypeScript plugin input must be a real regular file"
        ));
    }
    Ok(())
}

fn read_bounded_nofollow(path: &Path, limit: u64) -> Result<Vec<u8>> {
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        let descriptor = rustix::fs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| miette!("could not open TypeScript plugin input safely: {error}"))?;
        fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = fs::File::open(path).into_diagnostic()?;
    read_bounded_file(file, limit)
}

fn read_bounded_beneath(root: &Path, relative: &Path, limit: u64) -> Result<Vec<u8>> {
    validate_logical_path(&relative.to_string_lossy().replace('\\', "/"))?;
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        use std::os::fd::OwnedFd;

        let mut directory: OwnedFd = rustix::fs::open(
            root,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| miette!("could not open TypeScript plugin package safely: {error}"))?;
        if let Some(parent) = relative.parent() {
            for component in parent.components() {
                let Component::Normal(name) = component else {
                    return Err(miette!("TypeScript plugin graph path is unsafe"));
                };
                directory = rustix::fs::openat(
                    &directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|error| {
                    miette!("could not traverse TypeScript plugin package safely: {error}")
                })?;
            }
        }
        let name = relative
            .file_name()
            .ok_or_else(|| miette!("TypeScript plugin graph path is unsafe"))?;
        let descriptor = rustix::fs::openat(
            &directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| miette!("could not open TypeScript plugin input safely: {error}"))?;
        fs::File::from(descriptor)
    };
    #[cfg(not(unix))]
    let file = fs::File::open(root.join(relative)).into_diagnostic()?;
    read_bounded_file(file, limit)
}

fn read_bounded_file(file: fs::File, limit: u64) -> Result<Vec<u8>> {
    let metadata = file.metadata().into_diagnostic()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(miette!(
            "TypeScript plugin input is not a bounded regular file"
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .into_diagnostic()?;
    if bytes.len() as u64 > limit {
        return Err(miette!("TypeScript plugin input exceeds its byte limit"));
    }
    Ok(bytes)
}

fn random_suffix() -> Result<String> {
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| miette!("source preparation entropy failed: {error}"))?;
    Ok(u64::from_ne_bytes(bytes).to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    #[test]
    fn bun_text_lock_trailing_commas_are_parsed_without_changing_strings() {
        let lock = br#"{
          "packages": {
            "fixture": ["fixture@1.0.0", "comma,}literal", "sha512-proof"],
          },
        }"#;
        let parsed = parse_bun_lock(lock).expect("Bun text lock");
        assert_eq!(parsed["packages"]["fixture"][1], "comma,}literal");
        assert_eq!(parsed["packages"]["fixture"][2], "sha512-proof");
    }

    #[cfg(unix)]
    #[test]
    fn sealed_copy_rejects_a_swapped_symlink_directory_component() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("root");
        let outside = tempfile::tempdir().expect("outside");
        let destination = tempfile::tempdir().expect("destination");
        fs::write(root.path().join("package.json"), "{}").expect("package");
        fs::write(root.path().join("bun.lock"), "{}").expect("lock");
        fs::write(outside.path().join("index.ts"), "export {};").expect("source");
        symlink(outside.path(), root.path().join("src")).expect("symlink");
        let report = GraphReport {
            abi: 1,
            format: "fixture-v1".to_owned(),
            inputs: vec![GraphInput {
                path: "src/index.ts".to_owned(),
                bytes: 10,
            }],
        };

        assert!(copy_graph(root.path(), destination.path(), &report).is_err());
        assert!(!destination.path().join("src/index.ts").exists());
    }
}

#[cfg(all(test, target_os = "macos"))]
mod native_tests;
