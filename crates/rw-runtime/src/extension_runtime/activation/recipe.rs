//! Approved preparation and launch retain physical owners through retirement.
use super::{ActivationLease, Generation, cancelled, error, unsettled};
use crate::extension_runtime::{
    PrivateMcpScratch, PrivatePluginApprovalStore, RuntimePluginProviderHttp,
    SessionPluginPushHandler, SharedPluginRedactor,
};
use crate::{
    extension_config::{DiscoveredPlugin, DiscoveredPluginTarget},
    source_plugin::{SourcePluginResolver, SourcePreparations},
};
use futures_util::FutureExt as _;
use rw_ext::{
    ApprovalRequirement, ApprovalStore, PluginBoundaryRedactor, PluginEndpointMetadata, PluginHost,
    PluginHostError, PluginLauncher, PluginProviderHttpHandler, PluginRpcError,
    plugin_launch_approval_requirement,
};
use std::{path::PathBuf, sync::Arc};
use tokio::time::Instant;

pub(in crate::extension_runtime) enum ActivationApproval {
    Configured,
    SessionDevelopment,
}

pub(in crate::extension_runtime) struct ActivationRecipe {
    pub approval: ActivationApproval,
    pub metadata: PluginEndpointMetadata,
    pub config: DiscoveredPlugin,
    pub private_root: PathBuf,
    pub workspace_roots: Vec<PathBuf>,
    pub helper: PathBuf,
    pub redactor: Arc<SharedPluginRedactor>,
    pub push_handler: Arc<SessionPluginPushHandler>,
    pub budget: Arc<super::PluginRuntimeBudget>,
    #[cfg(test)]
    pub launcher: Option<Arc<dyn PluginLauncher>>,
}

#[derive(Default)]
pub(super) struct ActivationResources {
    pub lease: Option<ActivationLease>,
    pub(super) scratch: Option<Arc<PrivateMcpScratch>>,
    preparation: Option<Arc<SourcePreparations>>,
    host: Option<Arc<PluginHost>>,
    pub failure: Option<PluginRpcError>,
    pub effects_started: bool,
}
impl ActivationResources {
    pub fn publish(&mut self) {
        if let Some(lease) = &mut self.lease {
            lease.published();
        }
    }
    pub fn settled(&mut self) {
        self.effects_started = false;
        self.host.take();
        self.preparation.take();
        self.scratch.take();
        if let Some(mut lease) = self.lease.take() {
            lease.settled();
        }
    }
}

pub(super) async fn activate(
    generation: &Arc<Generation>,
    deadline: Instant,
) -> Result<Arc<PluginHost>, PluginRpcError> {
    let recipe = &generation.recipe;
    let mut lease = generation
        .resources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lease
        .take()
        .ok_or_else(|| unsettled("plugin activation admission is absent"))?;
    // This lease owns only semaphore reservations while waiting. No native work
    // starts before it is returned to the retained generation resource owner.
    let reservation = recipe
        .budget
        .reserve_process(&mut lease, &generation.cancellation, deadline)
        .await;
    generation
        .resources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .lease = Some(lease);
    reservation?;
    if generation.cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let scratch = Arc::new(PrivateMcpScratch::create().map_err(diagnostic)?);
    {
        let mut resources = generation
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        resources.scratch = Some(Arc::clone(&scratch));
        resources.effects_started = true;
        if let Some(lease) = &mut resources.lease {
            lease.begin_effects();
        }
    }
    let launcher = launcher(recipe, &scratch)?;
    let process = prepare_process(generation, Arc::clone(&launcher), scratch).await?;
    if generation.cancellation.is_cancelled() {
        return Err(cancelled());
    }
    let (store, origin) = approvals(recipe, &process)?;
    require_approval(
        store.as_ref(),
        recipe.metadata.manifest(),
        &process,
        &origin,
    )?;
    let provider_http: Arc<dyn PluginProviderHttpHandler> =
        if recipe.metadata.manifest().capabilities.providers.is_empty() {
            Arc::new(rw_ext::DenyPluginProviderHttpHandler)
        } else {
            let registrar: Arc<dyn rw_providers::KnownSecretRegistrar> = recipe.redactor.clone();
            Arc::new(
                RuntimePluginProviderHttp::new(
                    &recipe.private_root.join("credentials.toml"),
                    &process
                        .allowed_domains()
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    registrar,
                )
                .map_err(diagnostic)?,
            )
        };
    let redactor: Arc<dyn PluginBoundaryRedactor> = recipe.redactor.clone();
    // Accepted launch is never cancellation-dropped. A late initialized host is
    // retained here and retired by the operation owner before proof is reported.
    let result = PluginHost::launch_approved_with_http(
        launcher.as_ref(),
        store.as_ref(),
        &process,
        &origin,
        &recipe.workspace_roots,
        recipe.metadata.manifest().clone(),
        recipe.push_handler.clone(),
        provider_http,
        redactor,
    )
    .await;
    let host = match result {
        Ok(host) => Arc::new(host),
        Err(PluginHostError::EffectsUnsettled { message }) => {
            let failure = unsettled(&message);
            generation
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .failure = Some(failure.clone());
            return Err(failure);
        }
        Err(error) => return Err(diagnostic(error)),
    };
    generation
        .resources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .host = Some(Arc::clone(&host));
    Ok(host)
}

async fn prepare_process(
    generation: &Arc<Generation>,
    launcher: Arc<dyn PluginLauncher>,
    scratch: Arc<PrivateMcpScratch>,
) -> Result<rw_ext::PluginProcessConfig, PluginRpcError> {
    let recipe = &generation.recipe;
    match recipe.config.target {
        DiscoveredPluginTarget::Executable { .. } => recipe
            .config
            .executable_process_config()
            .map_err(diagnostic),
        DiscoveredPluginTarget::TypeScript { .. } => {
            let host = recipe
                .helper
                .parent()
                .ok_or_else(|| {
                    error(
                        "configuration",
                        "Rottweiler executable has no release directory",
                    )
                })?
                .join("rottweiler-plugin-host");
            let resolver = SourcePluginResolver::new(
                &host,
                &recipe.private_root,
                scratch,
                Arc::clone(&launcher),
                Arc::clone(&recipe.budget.preparation),
            )
            .map_err(diagnostic)?;
            generation
                .resources
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .preparation = Some(resolver.preparation());
            tokio::select! {
                biased;
                () = generation.cancellation.cancelled() => Err(cancelled()),
                result = resolver.resolve(&recipe.config) => result.map_err(diagnostic),
            }
        }
    }
}

fn approvals(
    recipe: &ActivationRecipe,
    process: &rw_ext::PluginProcessConfig,
) -> Result<(Box<dyn ApprovalStore>, String), PluginRpcError> {
    let (store, origin): (Box<dyn ApprovalStore>, String) = match recipe.approval {
        ActivationApproval::Configured => {
            let store =
                PrivatePluginApprovalStore::open(&recipe.private_root).map_err(diagnostic)?;
            let scope = match recipe.config.origin {
                crate::extension_config::ExecutableConfigOrigin::User(_) => "user",
                crate::extension_config::ExecutableConfigOrigin::TrustedProject(_) => "project",
            };
            (
                Box::new(store),
                format!("{scope}:{}", recipe.config.origin.path().display()),
            )
        }
        ActivationApproval::SessionDevelopment => {
            let store =
                crate::extension_runtime::development::SessionDevelopmentApprovalStore::default();
            let origin = format!("development:{}", recipe.config.manifest_path.display());
            rw_ext::approve_plugin_launch(&store, recipe.metadata.manifest(), process, &origin)
                .map_err(diagnostic)?;
            (Box::new(store), origin)
        }
    };
    Ok((store, origin))
}

fn launcher(
    recipe: &ActivationRecipe,
    scratch: &PrivateMcpScratch,
) -> Result<Arc<dyn PluginLauncher>, PluginRpcError> {
    #[cfg(test)]
    if let Some(launcher) = &recipe.launcher {
        return Ok(Arc::clone(launcher));
    }
    Ok(Arc::new(
        crate::plugin_process::SandboxedPluginLauncher::new(scratch.path(), &recipe.helper)
            .map_err(diagnostic)?,
    ))
}

fn require_approval(
    store: &dyn ApprovalStore,
    manifest: &rw_plugin_protocol::PluginManifest,
    process: &rw_ext::PluginProcessConfig,
    origin: &str,
) -> Result<(), PluginRpcError> {
    match plugin_launch_approval_requirement(store, manifest, process, origin)
        .map_err(diagnostic)?
    {
        ApprovalRequirement::Approved => Ok(()),
        ApprovalRequirement::FirstLoad { .. } => Err(error(
            "approval_required",
            "plugin first approval is required",
        )),
        ApprovalRequirement::ManifestChanged { .. } => {
            Err(error("approval_required", "plugin approval changed"))
        }
    }
}

pub(super) async fn retire(generation: &Generation) -> Result<(), PluginRpcError> {
    let (host, preparation) = {
        let resources = generation
            .resources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (resources.host.clone(), resources.preparation.clone())
    };
    let mut failure = None;
    if let Some(host) = host {
        match std::panic::AssertUnwindSafe(host.shutdown())
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => failure = Some(unsettled(&error.to_string())),
            Err(_) => failure = Some(unsettled("plugin host retirement panicked")),
        }
    }
    if let Some(preparation) = preparation {
        match std::panic::AssertUnwindSafe(preparation.settle_cancelled())
            .catch_unwind()
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                failure.get_or_insert_with(|| unsettled(&error.to_string()));
            }
            Err(_) => {
                failure.get_or_insert_with(|| unsettled("plugin source retirement panicked"));
            }
        }
    }
    let mut resources = generation
        .resources
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(error) = failure {
        resources.failure.get_or_insert(error);
    }
    if let Some(error) = &resources.failure {
        return Err(error.clone());
    }
    resources.settled();
    Ok(())
}
fn diagnostic(error: impl std::fmt::Display) -> PluginRpcError {
    super::error("activation_failed", &error.to_string())
}
