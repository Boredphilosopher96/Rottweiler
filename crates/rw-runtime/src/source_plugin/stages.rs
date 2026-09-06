//! Filesystem stages run only inside the retained preparation IO owner.
use super::{
    GraphReport, JS_HOST_SOURCE_PLUGIN_ROLE, MAX_BUNDLE_BYTES, MAX_GRAPH_BYTES, MAX_REPORT_BYTES,
    PluginProcessConfig, SourcePluginIdentity, SourcePluginResolver, copy_graph, graph_digest,
    publish_bundle, random_suffix, read_bounded_nofollow, require_regular_file, validate_graph,
    validate_report,
};
use crate::extension_config::{DiscoveredPlugin, DiscoveredPluginTarget};
use miette::{IntoDiagnostic as _, Result, miette};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(super) struct StagedSource {
    pub root: PathBuf,
    pub entry: PathBuf,
    pub output: PathBuf,
    lock_bytes: Vec<u8>,
    discovered: GraphReport,
}

pub(super) fn check_inputs(plugin: &DiscoveredPlugin) -> Result<()> {
    let DiscoveredPluginTarget::TypeScript {
        package_root,
        entry,
    } = &plugin.target
    else {
        return Err(miette!("source staging requires a TypeScript package"));
    };
    let package = package_root.join("package.json");
    let lockfile = package_root.join("bun.lock");
    for required in [&package, &lockfile, &plugin.manifest_path, entry] {
        require_regular_file(required)?;
    }
    Ok(())
}

pub(super) fn stage(
    owner: &SourcePluginResolver,
    package_root: &Path,
    entry: &Path,
    discovered: GraphReport,
) -> Result<StagedSource> {
    validate_report(&discovered)?;
    let staging = owner
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
    let output = owner
        .scratch
        .path()
        .join(format!("bundle-{}", random_suffix()?));
    fs::create_dir(&output).into_diagnostic()?;
    Ok(StagedSource {
        root: staging,
        entry: staged_entry,
        output,
        lock_bytes,
        discovered,
    })
}

pub(super) fn publish(
    owner: &SourcePluginResolver,
    plugin: &DiscoveredPlugin,
    staged: StagedSource,
    rebuilt: GraphReport,
) -> Result<PluginProcessConfig> {
    let StagedSource {
        root: staging,
        output,
        lock_bytes,
        discovered,
        ..
    } = staged;
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
    let prepared = publish_bundle(&owner.private_root, &identity, &bundle_bytes, &discovered)?;
    PluginProcessConfig::new(&owner.host)
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
            config.with_attested_files([prepared.join("plugin.mjs"), prepared.join("graph.json")])
        })
        .and_then(|config| config.with_environment_allowlist(plugin.inherit_env.iter().cloned()))
        .and_then(|config| config.with_allowed_domains(plugin.allowed_domains.iter().cloned()))
        .map(|config| config.with_source_identity(identity))
        .map_err(|error| miette!(error.to_string()))
}
