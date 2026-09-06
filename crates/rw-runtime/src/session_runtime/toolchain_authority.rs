//! Read authority belongs to configured toolchain hooks, never the general shell.

use super::command_execution::{
    CommandFixtureMode, PrivateScratch, ScratchGuardedCommandExecutor,
    build_command_executor_for_policy, command_fixture_namespace,
};
use miette::{Result, miette};
use rw_tools::{
    CommandExecutor, CommandSafetyClassifier, ExecutionLease, NetworkPolicy, SandboxPolicy,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(super) fn build_toolchain_executor(
    workspace_roots: &[PathBuf],
    runtime_read_roots: &[PathBuf],
    workspace: &Path,
    mode: CommandFixtureMode,
    execution_lease: &Arc<ExecutionLease>,
    safety: &Arc<CommandSafetyClassifier>,
) -> Result<Arc<dyn CommandExecutor>> {
    if !rw_types::config::valid_toolchain_runtime_read_roots(runtime_read_roots) {
        return Err(miette!(
            "toolchain runtime read roots violate the configuration bounds"
        ));
    }
    let scratch = PrivateScratch::create("toolchain")?;
    let mut writes = workspace_roots.to_vec();
    writes.push(scratch.path().to_path_buf());
    let base = SandboxPolicy::new(&writes, NetworkPolicy::Deny)
        .map_err(|error| miette!("toolchain sandbox could not be built: {error}"))?;
    // Resolve every declared path before publishing this generation. The shared
    // sandbox owner deduplicates canonical roots and excludes sensitive paths.
    let declared = base
        .clone()
        .with_read_roots(runtime_read_roots)
        .map_err(|error| miette!("toolchain runtime read authority is invalid: {error}"))?;
    #[cfg(target_os = "linux")]
    let policy = declared;
    // macOS already permits general reads with credential exclusions. Applying
    // a narrow read policy here would remove its system-library read baseline.
    #[cfg(not(target_os = "linux"))]
    let policy = {
        drop(declared);
        base
    };
    let inner = build_command_executor_for_policy(
        &Arc::new(policy),
        workspace,
        command_fixture_namespace(mode, "toolchain"),
        execution_lease,
        safety,
        None,
        false,
    )?;
    Ok(Arc::new(ScratchGuardedCommandExecutor {
        inner,
        _scratch: scratch,
    }))
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
