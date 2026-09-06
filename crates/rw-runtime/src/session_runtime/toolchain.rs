use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_core::HostError;
use rw_core::HostRuntimeService;
use rw_core::RuntimeServiceDescriptor;
use rw_core::RuntimeServiceKind;
use rw_core::ToolOutputStream;
use rw_ext::HookDirective;
use rw_ext::HookError;
use rw_ext::HookHandler;
use rw_ext::HookInvocation;
use rw_tools::BashSandboxMode;
use rw_tools::CancellationToken;
use rw_tools::CodeIntelligenceProvider;
use rw_tools::CommandExecutor;
use rw_tools::CommandRequest;
use rw_tools::ToolBehavior;
use rw_tools::ToolError;
use rw_tools::ToolOutputChunk;
use rw_tools::ToolOutputSink;
use rw_tools::ToolRegistry;
use rw_types::ToolOutput;
use rw_types::ToolOutputPart;
use rw_types::config::ToolchainConfig;
use rw_types::hook_contract::{HookInput, HookTransform};
use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

pub(super) const MAX_TOOLCHAIN_DIAGNOSTIC_BYTES: usize = 64 * 1024;

pub(super) struct CompiledToolchainRule {
    pub(super) matcher: globset::GlobMatcher,
    pub(super) formatter: Option<String>,
    pub(super) linters: Vec<String>,
}

#[derive(Clone)]
pub(super) struct ToolchainExecutionBoundary {
    pub(super) executor: Arc<dyn CommandExecutor>,
    pub(super) toolchain_executor: Arc<dyn CommandExecutor>,
    pub(super) read_only_executor: Arc<dyn CommandExecutor>,
    pub(super) read_only_scratch: PathBuf,
    pub(super) workspace_roots: Vec<PathBuf>,
}

pub(super) struct ToolchainRuntime {
    pub(super) current: RwLock<ToolchainExecutionBoundary>,
    pub(super) pending: Mutex<BTreeMap<u64, ToolchainExecutionBoundary>>,
    pub(super) active: Mutex<BTreeMap<(RuntimeServiceKind, String), usize>>,
}

impl ToolchainRuntime {
    #[cfg(test)]
    pub(super) fn new(executor: Arc<dyn CommandExecutor>, workspace_roots: &[PathBuf]) -> Self {
        let scratch = workspace_roots.first().cloned().unwrap_or_default();
        Self::new_with_read_only(
            Arc::clone(&executor),
            Arc::clone(&executor),
            executor,
            scratch,
            workspace_roots,
        )
    }

    pub(super) fn new_with_read_only(
        executor: Arc<dyn CommandExecutor>,
        toolchain_executor: Arc<dyn CommandExecutor>,
        read_only_executor: Arc<dyn CommandExecutor>,
        read_only_scratch: PathBuf,
        workspace_roots: &[PathBuf],
    ) -> Self {
        Self {
            current: RwLock::new(ToolchainExecutionBoundary {
                executor,
                toolchain_executor,
                read_only_executor,
                read_only_scratch,
                workspace_roots: canonical_toolchain_roots(workspace_roots),
            }),
            pending: Mutex::new(BTreeMap::new()),
            active: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) fn enter(
        self: &Arc<Self>,
        kind: RuntimeServiceKind,
        name: String,
    ) -> ToolchainActivityGuard {
        let key = (kind, name);
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *active.entry(key.clone()).or_default() += 1;
        ToolchainActivityGuard {
            runtime: Arc::clone(self),
            key,
        }
    }

    pub(super) fn active_services(&self) -> Vec<RuntimeServiceDescriptor> {
        self.active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .keys()
            .map(|(kind, name)| RuntimeServiceDescriptor {
                kind: *kind,
                name: name.clone(),
            })
            .collect()
    }

    pub(super) fn current(&self) -> ToolchainExecutionBoundary {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(super) fn prepare(
        &self,
        generation: u64,
        executor: Arc<dyn CommandExecutor>,
        toolchain_executor: Arc<dyn CommandExecutor>,
        read_only_executor: Arc<dyn CommandExecutor>,
        read_only_scratch: PathBuf,
        workspace_roots: &[PathBuf],
    ) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                generation,
                ToolchainExecutionBoundary {
                    executor,
                    toolchain_executor,
                    read_only_executor,
                    read_only_scratch,
                    workspace_roots: canonical_toolchain_roots(workspace_roots),
                },
            );
    }

    pub(super) fn commit(&self, generation: u64) {
        let prepared = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&generation);
        if let Some(prepared) = prepared {
            *self
                .current
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = prepared;
        }
    }

    pub(super) fn abort(&self, generation: u64) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&generation);
    }
}

pub(super) struct ToolchainActivityGuard {
    pub(super) runtime: Arc<ToolchainRuntime>,
    pub(super) key: (RuntimeServiceKind, String),
}

impl Drop for ToolchainActivityGuard {
    fn drop(&mut self) {
        let mut active = self
            .runtime
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let remove = active.get_mut(&self.key).is_some_and(|count| {
            *count = count.saturating_sub(1);
            *count == 0
        });
        if remove {
            active.remove(&self.key);
        }
    }
}

pub(super) struct RuntimeServiceView {
    pub(super) intelligence: Arc<dyn CodeIntelligenceProvider>,
    pub(super) toolchain: Arc<ToolchainRuntime>,
}

#[async_trait]
impl HostRuntimeService for RuntimeServiceView {
    async fn list(&self) -> std::result::Result<Vec<RuntimeServiceDescriptor>, HostError> {
        let mut services = self.toolchain.active_services();
        services.extend(
            self.intelligence
                .active_lsp_servers()
                .await
                .into_iter()
                .map(|name| RuntimeServiceDescriptor {
                    kind: RuntimeServiceKind::Lsp,
                    name,
                }),
        );
        services.sort_by(|left, right| {
            runtime_service_order(left.kind)
                .cmp(&runtime_service_order(right.kind))
                .then_with(|| left.name.cmp(&right.name))
        });
        services.dedup();
        Ok(services)
    }
}

pub(super) const fn runtime_service_order(kind: RuntimeServiceKind) -> u8 {
    match kind {
        RuntimeServiceKind::Lsp => 0,
        RuntimeServiceKind::Formatter => 1,
        RuntimeServiceKind::Linter => 2,
        RuntimeServiceKind::Test => 3,
    }
}

pub(super) fn toolchain_command_identity(kind: RuntimeServiceKind, command: &str) -> String {
    let fallback = || match kind {
        RuntimeServiceKind::Formatter => "formatter".to_owned(),
        RuntimeServiceKind::Linter => "linter".to_owned(),
        RuntimeServiceKind::Test => "test".to_owned(),
        RuntimeServiceKind::Lsp => "language server".to_owned(),
    };
    shell_words::split(command)
        .ok()
        .and_then(|parts| parts.into_iter().next())
        .filter(|program| !program.contains('='))
        .and_then(|program| {
            Path::new(&program)
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .filter(|name| {
            !name.is_empty()
                && name.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "._+-".contains(character)
                })
        })
        .unwrap_or_else(fallback)
}

pub(super) fn canonical_toolchain_roots(workspace_roots: &[PathBuf]) -> Vec<PathBuf> {
    workspace_roots
        .iter()
        .map(|root| std::fs::canonicalize(root).unwrap_or_else(|_| root.clone()))
        .collect()
}

pub(super) struct ToolchainHook {
    pub(super) formatter: Option<String>,
    pub(super) linters: Vec<String>,
    pub(super) rules: Vec<CompiledToolchainRule>,
    pub(super) runtime: Arc<ToolchainRuntime>,
    pub(super) tools: Arc<ToolRegistry>,
}

pub(super) struct ToolchainTestHook {
    pub(super) command: String,
    pub(super) runtime: Arc<ToolchainRuntime>,
}

#[async_trait]
impl HookHandler for ToolchainTestHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        let boundary = self.runtime.current();
        boundary
            .toolchain_executor
            .settle_effects()
            .await
            .map_err(|error| HookError::new("effects_unsettled", error.to_string()))
    }

    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> std::result::Result<HookDirective, HookError> {
        if !matches!(invocation.input(), HookInput::TurnEnd(input) if input.status == rw_types::TurnStatus::Completed)
        {
            return Ok(HookDirective::Continue {});
        }
        let boundary = self.runtime.current();
        let cwd = boundary.workspace_roots.first().ok_or_else(|| {
            HookError::new("toolchain_test", "test command has no workspace root")
        })?;
        let _activity = self.runtime.enter(
            RuntimeServiceKind::Test,
            toolchain_command_identity(RuntimeServiceKind::Test, &self.command),
        );
        let capture = Arc::new(HookCommandCapture::default());
        let outcome = boundary
            .toolchain_executor
            .run(
                CommandRequest {
                    command: self.command.clone(),
                    cwd: cwd.clone(),
                    env: BTreeMap::new(),
                    network_domains: Vec::new(),
                    sandbox: BashSandboxMode::Sandboxed,
                },
                invocation.cancellation().clone(),
                capture.clone(),
            )
            .await
            .map_err(|error| HookError::new("toolchain_test", error.to_string()))?;
        if outcome.exit_code == 0 {
            return Ok(HookDirective::Continue {});
        }
        let (stdout, stderr) = capture.finish();
        Ok(HookDirective::Block {
            message: HookCommandResult {
                exit_code: outcome.exit_code,
                stdout,
                stderr,
            }
            .render("test"),
        })
    }
}

impl ToolchainHook {
    pub(super) fn compile(
        config: &ToolchainConfig,
        runtime: Arc<ToolchainRuntime>,
        tools: Arc<ToolRegistry>,
    ) -> Result<Self> {
        let rules = config
            .rules
            .iter()
            .map(|rule| {
                globset::GlobBuilder::new(&rule.pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                    .map(|glob| CompiledToolchainRule {
                        matcher: glob.compile_matcher(),
                        formatter: rule.formatter.clone(),
                        linters: rule.linters.clone(),
                    })
                    .map_err(|error| {
                        miette!("invalid toolchain file glob {:?}: {error}", rule.pattern)
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            formatter: config.formatter.clone(),
            linters: config.linters.clone(),
            rules,
            runtime,
            tools,
        })
    }

    pub(super) fn commands_for(&self, virtual_path: &str) -> (Option<&str>, &[String]) {
        self.rules
            .iter()
            .find(|rule| rule.matcher.is_match(virtual_path))
            .map_or(
                (self.formatter.as_deref(), self.linters.as_slice()),
                |rule| {
                    (
                        rule.formatter.as_deref().or(self.formatter.as_deref()),
                        if rule.linters.is_empty() {
                            self.linters.as_slice()
                        } else {
                            rule.linters.as_slice()
                        },
                    )
                },
            )
    }

    pub(super) async fn run_command(
        &self,
        kind: RuntimeServiceKind,
        command: &str,
        file: &Path,
        cwd: &Path,
        cancellation: CancellationToken,
    ) -> std::result::Result<HookCommandResult, HookError> {
        let file_text = file.to_string_lossy();
        let quoted_file = shell_words::quote(&file_text);
        let command = command.replace("{file}", &quoted_file);
        let _activity = self
            .runtime
            .enter(kind, toolchain_command_identity(kind, &command));
        let capture = Arc::new(HookCommandCapture::default());
        let boundary = self.runtime.current();
        let outcome = boundary
            .toolchain_executor
            .run(
                CommandRequest {
                    command,
                    cwd: cwd.to_path_buf(),
                    env: BTreeMap::new(),
                    network_domains: Vec::new(),
                    sandbox: BashSandboxMode::Sandboxed,
                },
                cancellation,
                capture.clone(),
            )
            .await
            .map_err(|error| HookError::new("toolchain_command", error.to_string()))?;
        let (stdout, stderr) = capture.finish();
        Ok(HookCommandResult {
            exit_code: outcome.exit_code,
            stdout,
            stderr,
        })
    }
}

pub(super) fn registered_file_mutation_path(
    tools: &ToolRegistry,
    name: &str,
    arguments: &serde_json::Value,
) -> std::result::Result<Option<PathBuf>, HookError> {
    let semantics = tools
        .invocation_semantics(name, arguments)
        .map_err(|error| HookError::new("tool_semantics", error.to_string()))?
        .ok_or_else(|| HookError::new("tool_semantics", "tool is not registered"))?;
    if semantics.behavior != ToolBehavior::FileMutation {
        return Ok(None);
    }
    match semantics.workspace_paths.as_slice() {
        [path] => Ok(Some(path.clone())),
        [] => Err(HookError::new(
            "tool_semantics",
            "registered file mutation did not declare a workspace path",
        )),
        _ => Err(HookError::new(
            "tool_semantics",
            "toolchain hooks require one registered workspace path",
        )),
    }
}

#[async_trait]
impl HookHandler for ToolchainHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        let boundary = self.runtime.current();
        boundary
            .toolchain_executor
            .settle_effects()
            .await
            .map_err(|error| HookError::new("effects_unsettled", error.to_string()))
    }

    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> std::result::Result<HookDirective, HookError> {
        let HookInput::PostTool(payload) = invocation.input() else {
            return Ok(HookDirective::Continue {});
        };
        let tool_name = &payload.name;
        let arguments = &payload.arguments;
        let Some(virtual_path) = registered_file_mutation_path(&self.tools, tool_name, arguments)?
        else {
            return Ok(HookDirective::Continue {});
        };
        let boundary = self.runtime.current();
        let Some((file, cwd)) = resolve_toolchain_file(&boundary.workspace_roots, &virtual_path)
        else {
            return Err(HookError::new(
                "toolchain_path",
                "post-tool path could not be resolved inside a workspace root",
            ));
        };
        let Some(virtual_path) = virtual_path.to_str() else {
            return Err(HookError::new(
                "toolchain_path",
                "registered tool path is not UTF-8",
            ));
        };
        let (formatter, linters) = self.commands_for(virtual_path);
        let mut diagnostics = Vec::new();
        let mut failed = false;
        if let Some(formatter) = formatter {
            let result = self
                .run_command(
                    RuntimeServiceKind::Formatter,
                    formatter,
                    &file,
                    &cwd,
                    invocation.cancellation().clone(),
                )
                .await?;
            failed |= result.exit_code != 0;
            if result.exit_code != 0 || !result.stdout.is_empty() || !result.stderr.is_empty() {
                diagnostics.push(result.render("formatter"));
            }
        }
        for linter in linters {
            let result = self
                .run_command(
                    RuntimeServiceKind::Linter,
                    linter,
                    &file,
                    &cwd,
                    invocation.cancellation().clone(),
                )
                .await?;
            failed |= result.exit_code != 0;
            if result.exit_code != 0 || !result.stdout.is_empty() || !result.stderr.is_empty() {
                diagnostics.push(result.render("linter"));
            }
        }
        if diagnostics.is_empty() {
            return Ok(HookDirective::Continue {});
        }
        let mut output = payload.output.clone();
        let diagnostics = diagnostics.join("\n\n");
        append_post_tool_diagnostics(&mut output, "Toolchain diagnostics", &diagnostics);
        Ok(HookDirective::Transform {
            change: HookTransform::PostTool {
                output,
                is_error: payload.is_error || failed,
            },
        })
    }
}

pub(super) struct LspDiagnosticsHook {
    pub(super) intelligence: Arc<dyn CodeIntelligenceProvider>,
    pub(super) runtime: Arc<ToolchainRuntime>,
    pub(super) tools: Arc<ToolRegistry>,
}

#[async_trait]
impl HookHandler for LspDiagnosticsHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> std::result::Result<HookDirective, HookError> {
        let HookInput::PostTool(payload) = invocation.input() else {
            return Ok(HookDirective::Continue {});
        };
        let tool_name = &payload.name;
        let arguments = &payload.arguments;
        let Some(virtual_path) = registered_file_mutation_path(&self.tools, tool_name, arguments)?
        else {
            return Ok(HookDirective::Continue {});
        };
        let boundary = self.runtime.current();
        let Some((file, _cwd)) = resolve_toolchain_file(&boundary.workspace_roots, &virtual_path)
        else {
            return Ok(HookDirective::Continue {});
        };
        let metadata = tokio::fs::metadata(&file)
            .await
            .map_err(|error| HookError::new("lsp_diagnostics_read", error.to_string()))?;
        if metadata.len() > 2 * 1024 * 1024 {
            return Ok(HookDirective::Continue {});
        }
        let source = tokio::fs::read_to_string(&file)
            .await
            .map_err(|error| HookError::new("lsp_diagnostics_read", error.to_string()))?;
        let diagnostics = self
            .intelligence
            .diagnostics(&virtual_path, &source)
            .await
            .items;
        if diagnostics.is_empty() {
            return Ok(HookDirective::Continue {});
        }
        let mut rendered = String::new();
        for diagnostic in diagnostics {
            let message = diagnostic
                .message
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            let line = format!(
                "{}:{}:{} {:?}: {}\n",
                diagnostic.path.display(),
                diagnostic.range.start.line.saturating_add(1),
                diagnostic.range.start.character.saturating_add(1),
                diagnostic.severity,
                message
            );
            if rendered.len().saturating_add(line.len()) > MAX_TOOLCHAIN_DIAGNOSTIC_BYTES {
                break;
            }
            rendered.push_str(&line);
        }
        if rendered.is_empty() {
            return Ok(HookDirective::Continue {});
        }
        let mut output = payload.output.clone();
        append_post_tool_diagnostics(&mut output, "LSP diagnostics (untrusted)", &rendered);
        Ok(HookDirective::Transform {
            change: HookTransform::PostTool {
                output,
                is_error: payload.is_error,
            },
        })
    }
}

#[derive(Default)]
pub(super) struct HookCommandCapture {
    pub(super) output: Mutex<(String, String)>,
}

#[async_trait]
impl ToolOutputSink for HookCommandCapture {
    async fn emit(&self, chunk: ToolOutputChunk) -> std::result::Result<(), ToolError> {
        let mut output = self
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let target = match chunk.stream {
            ToolOutputStream::Stdout => &mut output.0,
            ToolOutputStream::Stderr => &mut output.1,
        };
        let remaining = MAX_TOOLCHAIN_DIAGNOSTIC_BYTES.saturating_sub(target.len());
        let end = chunk
            .content
            .floor_char_boundary(remaining.min(chunk.content.len()));
        target.push_str(&chunk.content[..end]);
        Ok(())
    }
}

impl HookCommandCapture {
    pub(super) fn finish(&self) -> (String, String) {
        self.output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

pub(super) struct HookCommandResult {
    pub(super) exit_code: i32,
    pub(super) stdout: String,
    pub(super) stderr: String,
}

impl HookCommandResult {
    pub(super) fn render(&self, kind: &str) -> String {
        let mut rendered = format!("{kind} exit code: {}", self.exit_code);
        if !self.stdout.is_empty() {
            rendered.push_str("\nstdout:\n");
            rendered.push_str(&self.stdout);
        }
        if !self.stderr.is_empty() {
            rendered.push_str("\nstderr:\n");
            rendered.push_str(&self.stderr);
        }
        rendered
    }
}

pub(super) fn resolve_toolchain_file(
    roots: &[PathBuf],
    supplied: &Path,
) -> Option<(PathBuf, PathBuf)> {
    if supplied.is_absolute()
        || supplied.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return None;
    }
    let mut components = supplied.components();
    let (root_index, relative) = if components.next().is_some_and(
        |component| matches!(component, std::path::Component::Normal(name) if name == "@root"),
    ) {
        let std::path::Component::Normal(index) = components.next()? else {
            return None;
        };
        let index = index
            .to_str()?
            .parse::<usize>()
            .ok()
            .filter(|index| *index > 0)?;
        (index, components.as_path())
    } else {
        (0, supplied)
    };
    let root = roots.get(root_index)?;
    let candidate = std::fs::canonicalize(root.join(relative)).ok()?;
    candidate
        .starts_with(root)
        .then(|| (candidate, root.clone()))
}

pub(super) fn append_post_tool_diagnostics(
    output: &mut ToolOutput,
    heading: &str,
    diagnostics: &str,
) {
    let owned = std::mem::replace(
        output,
        ToolOutput::Text {
            text: String::new(),
        },
    );
    *output = match owned {
        ToolOutput::Text { mut text } => {
            text.push_str("\n\n");
            text.push_str(heading);
            text.push_str(":\n");
            text.push_str(diagnostics);
            ToolOutput::Text { text }
        }
        ToolOutput::Structured { value } => ToolOutput::Mixed {
            parts: vec![
                ToolOutputPart::Structured { value },
                ToolOutputPart::Text {
                    text: format!("{heading}:\n{diagnostics}"),
                },
            ],
        },
        ToolOutput::Mixed { mut parts } => {
            parts.push(ToolOutputPart::Text {
                text: format!("{heading}:\n{diagnostics}"),
            });
            ToolOutput::Mixed { parts }
        }
    };
}
