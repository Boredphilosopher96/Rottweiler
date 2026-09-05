use crate::OutputFormat;
use crate::journal_reads::JournalReads;
use miette::miette;
use rw_core::AgentLoopError;
use rw_core::CachedModelCatalog;
use rw_core::Config;
use rw_core::HostRuntimeService;
use rw_core::HostSubagentService;
use rw_providers::ProviderEvent;
use rw_types::PermissionModeDescriptor as PermissionMode;
use rw_types::SessionId;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 8_192;

pub(super) const DEFAULT_EVENT_CAPACITY: usize = 1_024;

pub(super) const DEFAULT_DOOM_LOOP_LIMIT: usize = 5;

pub(super) const MAX_GLOBAL_REVIEW_FILES: usize = 1_024;

pub(super) const MAX_GLOBAL_REVIEW_DIFF_BYTES: usize = 2 * 1024 * 1024;

pub(super) const MAX_WORKSPACE_ROOTS: usize = 32;

pub struct RunOptions {
    pub prompt: Option<String>,
    pub output_format: OutputFormat,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub resume: Option<String>,
    pub continue_latest: bool,
    pub replay_dir: Option<PathBuf>,
    pub record_replay_script: Option<PathBuf>,
    pub in_memory_replay_script: Option<PathBuf>,
    pub record_script_delay_ms: u64,
    pub perf_markers: bool,
    pub replay_provider: String,
    pub model: Option<String>,
    pub additional_workspaces: Vec<PathBuf>,
    pub dangerously_trust: bool,
    pub action: RunAction,
}

pub enum RunAction {
    Agent,
    PromptDump { turn: Option<u64> },
}

/// A startup task must not outlive an invocation that returns before joining
/// it. Aborting drops any in-flight Tokio child process, whose `kill_on_drop`
/// boundary then terminates the audited Git subprocess.
pub(super) struct AbortOnDropTask<T> {
    pub(super) handle: Option<tokio::task::JoinHandle<T>>,
}

impl<T> AbortOnDropTask<T> {
    pub(super) fn new(handle: tokio::task::JoinHandle<T>) -> Self {
        Self {
            handle: Some(handle),
        }
    }

    pub(super) async fn join(mut self) -> std::result::Result<T, tokio::task::JoinError> {
        let Some(handle) = self.handle.take() else {
            unreachable!("startup task can be joined only once");
        };
        handle.await
    }
}

impl<T> Drop for AbortOnDropTask<T> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

#[derive(Clone)]
#[allow(dead_code)]
pub enum HostedProviderMode {
    Live,
    DeterministicReplay {
        provider_name: String,
        scripts: Vec<Vec<ProviderEvent>>,
        event_delay_ms: u64,
    },
}

pub(crate) struct HostedSessionComposition {
    pub provider_admission: Arc<crate::provider_admission::DurableProviderAdmission>,
    pub plugin_activation: Arc<crate::extension_runtime::PluginActivationBudget>,
    pub wasm_workers: Arc<rw_ext::WasmWorkerPool>,
    pub index_pool: Arc<rw_tools::WorkspaceIndexPool>,
    pub journal_reads: Arc<JournalReads>,
    pub workspace: PathBuf,
    pub additional_workspaces: Vec<PathBuf>,
    pub allowed_workspace_roots: Vec<PathBuf>,
    pub storage_root: PathBuf,
    pub credentials_path: PathBuf,
    pub config: Config,
    pub session_id: SessionId,
    pub requested_model: Option<String>,
    pub resume: bool,
    pub permission_mode: Option<PermissionMode>,
    pub max_turns: usize,
    pub provider_mode: HostedProviderMode,
    pub dangerously_trust: bool,
    pub wait_for_execution_lease: bool,
}

pub(crate) struct HostedActorRuntime {
    pub handle: rw_core::SessionHandle,
    pub model_catalog: Option<Arc<CachedModelCatalog>>,
    pub mcp: Option<Arc<dyn rw_core::HostMcpService>>,
    pub runtime_services: Arc<dyn HostRuntimeService>,
    pub subagents: Arc<dyn HostSubagentService>,
    pub model_alias: String,
    pub driver_client_id: Option<rw_core::ClientId>,
    pub shell_active: bool,
}

#[allow(clippy::needless_pass_by_value)]
pub(super) fn display_agent_error(error: AgentLoopError) -> miette::Report {
    miette!(error.to_string())
}
