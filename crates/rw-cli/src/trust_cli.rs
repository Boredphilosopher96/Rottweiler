use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use miette::{IntoDiagnostic, Result, miette};
use rw_runtime::{executable_config, session};

use crate::cli_args::TrustCommand;

pub(super) fn configuration_root() -> Result<PathBuf> {
    let root = configuration_root_path()?;
    ensure_configuration_root(&root)?;
    Ok(root)
}

pub(super) fn canonical_workspace_roots(primary: &Path, added: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut roots = vec![fs::canonicalize(primary).into_diagnostic()?];
    for supplied in added {
        let canonical = fs::canonicalize(supplied)
            .map_err(|error| miette!("--add-dir {} is unavailable: {error}", supplied.display()))?;
        if !canonical.is_dir() {
            return Err(miette!(
                "--add-dir {} is not a directory",
                supplied.display()
            ));
        }
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    Ok(roots)
}

pub(super) fn prompt_for_folder_trust(
    storage_root: &Path,
    roots: &[PathBuf],
    dangerously_trust: bool,
) -> Result<()> {
    use std::io::IsTerminal as _;

    if dangerously_trust {
        eprintln!(
            "warning: --dangerously-trust enables executable project configuration for this process without persisting a decision"
        );
        return Ok(());
    }
    let store = rw_store::trust::FolderTrustStore::new(storage_root.join("trust.json"));
    for root in roots {
        let assessment = store.assess(root).into_diagnostic()?;
        if !assessment.requires_confirmation() {
            continue;
        }
        eprintln!("{}", assessment.render_prompt());
        if std::io::stdin().is_terminal() {
            eprint!("Trust this exact project extension inventory? [y/N] ");
            std::io::stderr().flush().into_diagnostic()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).into_diagnostic()?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                store.grant(&assessment).into_diagnostic()?;
                eprintln!(
                    "trusted {}; project extension changes require a session restart",
                    assessment.workspace().display()
                );
            } else {
                eprintln!(
                    "project extension configuration remains inert for {}",
                    assessment.workspace().display()
                );
            }
        } else {
            eprintln!(
                "project extension configuration remains inert; use `rw trust grant` interactively or --dangerously-trust in a controlled CI image"
            );
        }
    }
    Ok(())
}

pub(super) fn run_trust_command(command: TrustCommand) -> Result<()> {
    use std::io::IsTerminal as _;

    let workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let store = rw_store::trust::FolderTrustStore::new(loader.trust_store_path().to_path_buf());
    match command {
        TrustCommand::Status => {
            let assessment = store.assess(&workspace).into_diagnostic()?;
            print!("{}", assessment.render_prompt());
        }
        TrustCommand::Grant => {
            let assessment = store.assess(&workspace).into_diagnostic()?;
            ensure_folder_trust_grantable(&assessment)?;
            if !assessment.requires_confirmation() {
                if assessment.project_execution_enabled() {
                    println!(
                        "{} is already trusted for its current project extension inventory",
                        assessment.workspace().display()
                    );
                } else {
                    println!(
                        "no project extension configuration found in {}; nothing to trust",
                        assessment.workspace().display()
                    );
                }
                return Ok(());
            }
            eprint!("{}", assessment.render_prompt());
            if !std::io::stdin().is_terminal() {
                return Err(miette!(
                    "refusing to grant folder trust without an interactive terminal; use --dangerously-trust only for controlled CI images"
                ));
            }
            eprint!("Trust this exact project extension inventory? [y/N] ");
            std::io::stderr().flush().into_diagnostic()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer).into_diagnostic()?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Err(miette!("folder trust was not granted"));
            }
            store.grant(&assessment).into_diagnostic()?;
            println!(
                "trusted {}; restart active sessions to load project extension configuration",
                assessment.workspace().display()
            );
        }
        TrustCommand::Revoke => {
            store.revoke(&workspace).into_diagnostic()?;
            println!(
                "revoked trust for {}; restart active sessions to unload project extension configuration",
                workspace.display()
            );
        }
    }
    Ok(())
}

pub(super) fn ensure_folder_trust_grantable(
    assessment: &rw_store::trust::FolderTrustAssessment,
) -> Result<()> {
    if let Some(failure) = assessment.inventory_failure() {
        return Err(miette!(
            "refusing to grant folder trust because the project extension inventory is incomplete at {}: {}",
            failure.path().display(),
            failure.message()
        ));
    }
    Ok(())
}

pub(super) async fn run_plugin_approval(name: Option<&str>, revoke: bool) -> Result<()> {
    use std::io::IsTerminal as _;

    let workspace =
        fs::canonicalize(std::env::current_dir().into_diagnostic()?).into_diagnostic()?;
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let effective_config = loader.load().into_diagnostic()?;
    let storage_root = loader
        .credentials_path()
        .parent()
        .ok_or_else(|| miette!("configuration root has no parent"))?
        .to_path_buf();
    session::initialize_private_storage_root(&storage_root).into_diagnostic()?;
    let (user_home, _) = session::extension_user_roots(&loader.credentials_path());
    let catalog = executable_config::discover_executable_configs(
        &user_home,
        &workspace,
        effective_config.project_trusted(),
    )?;
    let store = rw_runtime::PrivatePluginApprovalStore::open(&storage_root)?;
    let selected = catalog
        .plugins
        .iter()
        .filter(|plugin| name.is_none_or(|name| plugin.name == name))
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(miette!("configured plugin was not found"));
    }
    for plugin in selected {
        if revoke {
            println!(
                "{}",
                if store.revoke(&plugin.name)? {
                    format!("revoked plugin {}", plugin.name)
                } else {
                    format!("plugin {} was not approved", plugin.name)
                }
            );
            continue;
        }
        let manifest = plugin.load_manifest()?;
        let helper =
            rw_tools::SandboxHelper::from_running(&std::env::current_exe().into_diagnostic()?)
                .into_diagnostic()?;
        let process =
            rw_runtime::plugin::resolve_plugin_process(plugin, &storage_root, &helper).await?;
        let scope = match plugin.origin {
            executable_config::ExecutableConfigOrigin::User(_) => "user",
            executable_config::ExecutableConfigOrigin::TrustedProject(_) => "project",
        };
        let origin = format!("{scope}:{}", plugin.origin.path().display());
        let requirement =
            rw_ext::plugin_launch_approval_requirement(&store, &manifest, &process, &origin)
                .map_err(|error| miette!(error.to_string()))?;
        let summary = serde_json::json!({
            "name": plugin.name, "origin": origin, "executable": process.executable(),
            "argv": process.argv().iter().map(|value| value.to_string_lossy()).collect::<Vec<_>>(),
            "cwd": process.cwd(), "environment_names": process.environment_allowlist(),
            "allowed_domains": process.allowed_domains(), "capabilities": manifest.capabilities,
            "attested_files": process.attested_files(),
            "code_root": process.code_root(),
            "approval": format!("{requirement:?}"),
        });
        let rendered = serde_json::to_string_pretty(&summary).into_diagnostic()?;
        if rendered.len() > 128 * 1024 {
            return Err(miette!("plugin approval summary exceeded its size cap"));
        }
        println!("{rendered}");
        if name.is_none() {
            continue;
        }
        if matches!(requirement, rw_ext::ApprovalRequirement::Approved) {
            println!("plugin {} is already approved", plugin.name);
            continue;
        }
        if !std::io::stdin().is_terminal() {
            return Err(miette!(
                "refusing plugin approval without an interactive terminal"
            ));
        }
        eprint!("Approve this exact plugin identity? [y/N] ");
        std::io::stderr().flush().into_diagnostic()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).into_diagnostic()?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Err(miette!("plugin approval was not granted"));
        }
        rw_ext::approve_plugin_launch(&store, &manifest, &process, &origin)
            .map_err(|error| miette!(error.to_string()))?;
        println!(
            "approved plugin {}; restart active sessions to launch it",
            plugin.name
        );
    }
    Ok(())
}

pub(super) fn configuration_root_path() -> Result<PathBuf> {
    let loader = rw_store::config::ConfigLoader::from_environment().into_diagnostic()?;
    let root = loader
        .credentials_path()
        .parent()
        .ok_or_else(|| miette!("configuration root has no parent"))?
        .to_path_buf();
    Ok(root)
}

pub(super) fn ensure_configuration_root(root: &Path) -> Result<()> {
    fs::create_dir_all(root).into_diagnostic()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).into_diagnostic()?;
    }
    Ok(())
}
