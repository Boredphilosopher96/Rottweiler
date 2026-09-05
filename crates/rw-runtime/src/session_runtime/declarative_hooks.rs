use super::toolchain::HookCommandCapture;
use super::toolchain::HookCommandResult;
use super::toolchain::ToolchainExecutionBoundary;
use super::toolchain::ToolchainRuntime;
use super::toolchain::append_post_tool_diagnostics;
use super::toolchain::resolve_toolchain_file;
use async_trait::async_trait;
use miette::Result;
use miette::miette;
use rw_ext::DiscoveredShellHook;
use rw_ext::ExtensionCatalog;
use rw_ext::HookDirective;
use rw_ext::HookDispatcher;
use rw_ext::HookEffect;
use rw_ext::HookError;
use rw_ext::HookEvent;
use rw_ext::HookHandler;
use rw_ext::HookInvocation;
use rw_tools::BashSandboxMode;
use rw_tools::CommandRequest;
use rw_types::hook_contract::{HookClass, HookInput, HookTransform};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub(super) enum DeclarativeHookMatcher {
    Any,
    Tool {
        name: String,
        arguments: globset::GlobMatcher,
    },
}

impl DeclarativeHookMatcher {
    pub(super) fn compile(value: &str) -> Result<Self> {
        if value == "*" {
            return Ok(Self::Any);
        }
        let (name, pattern) = value
            .split_once('(')
            .and_then(|(name, pattern)| pattern.strip_suffix(')').map(|pattern| (name, pattern)))
            .ok_or_else(|| miette!("hook matcher must use `*` or `tool(pattern)`"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(miette!("hook matcher tool name is invalid"));
        }
        let arguments = globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .backslash_escape(true)
            .build()
            .map_err(|error| miette!("hook matcher glob is invalid: {error}"))?
            .compile_matcher();
        Ok(Self::Tool {
            name: name.to_owned(),
            arguments,
        })
    }

    pub(super) fn matches(&self, payload: &HookInput) -> bool {
        match self {
            Self::Any => true,
            Self::Tool { name, arguments } => {
                payload.tool_name() == Some(name)
                    && hook_argument_text(payload)
                        .as_deref()
                        .is_some_and(|value| arguments.is_match(value))
            }
        }
    }
}

fn hook_arguments(input: &HookInput) -> Option<&serde_json::Value> {
    match input {
        HookInput::PreTool(input) => Some(&input.arguments),
        HookInput::PostTool(input) => Some(&input.arguments),
        HookInput::PermissionCheck(input) => Some(&input.arguments),
        _ => None,
    }
}

pub(super) fn hook_argument_text(payload: &HookInput) -> Option<String> {
    let arguments = hook_arguments(payload)?;
    arguments
        .get("path")
        .or_else(|| arguments.get("command"))
        .or_else(|| arguments.get("url"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| serde_json::to_string(arguments).ok())
}

pub(super) struct DeclarativeShellHookHandler {
    pub(super) hook: DiscoveredShellHook,
    pub(super) matcher: DeclarativeHookMatcher,
    pub(super) runtime: Arc<ToolchainRuntime>,
}

impl DeclarativeShellHookHandler {
    pub(super) fn command_request(
        &self,
        invocation: &HookInvocation<'_>,
        boundary: &ToolchainExecutionBoundary,
    ) -> std::result::Result<CommandRequest, HookError> {
        let mut command = self
            .hook
            .load_command()
            .map_err(|error| HookError::new("declarative_hook_changed", error.to_string()))?;
        if command.contains("{file}") {
            let virtual_path = hook_arguments(invocation.input())
                .and_then(|arguments| arguments.get("path"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    HookError::new(
                        "declarative_hook_file",
                        "hook command requested {file} without a tool path",
                    )
                })?;
            let (file, _) =
                resolve_toolchain_file(&boundary.workspace_roots, Path::new(virtual_path))
                    .ok_or_else(|| {
                        HookError::new(
                            "declarative_hook_file",
                            "hook file could not be resolved inside a workspace root",
                        )
                    })?;
            command = command.replace("{file}", &shell_words::quote(&file.to_string_lossy()));
        }
        let read_only = self.hook.registration().effect() == HookEffect::ReadOnly;
        let (executor_root, env) = if read_only {
            let scratch = boundary.read_only_scratch.clone();
            let env = BTreeMap::from([
                ("HOME".to_owned(), scratch.to_string_lossy().into_owned()),
                ("TMPDIR".to_owned(), scratch.to_string_lossy().into_owned()),
            ]);
            (scratch, env)
        } else {
            let root = boundary.workspace_roots.first().cloned().ok_or_else(|| {
                HookError::new("declarative_hook_root", "workspace root is unavailable")
            })?;
            (root, BTreeMap::new())
        };
        Ok(CommandRequest {
            command,
            cwd: executor_root,
            env,
            network_domains: Vec::new(),
            sandbox: BashSandboxMode::Sandboxed,
        })
    }
}

#[async_trait]
impl HookHandler for DeclarativeShellHookHandler {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        let boundary = self.runtime.current();
        let first = boundary.executor.settle_effects().await;
        let second = boundary.read_only_executor.settle_effects().await;
        first
            .and(second)
            .map_err(|error| HookError::new("effects_unsettled", error.to_string()))
    }

    async fn invoke(
        &self,
        invocation: HookInvocation<'_>,
    ) -> std::result::Result<HookDirective, HookError> {
        if !self.matcher.matches(invocation.input()) {
            return Ok(HookDirective::Continue {});
        }
        let boundary = self.runtime.current();
        let read_only = self.hook.registration().effect() == HookEffect::ReadOnly;
        let executor = if read_only {
            Arc::clone(&boundary.read_only_executor)
        } else {
            Arc::clone(&boundary.executor)
        };
        let request = self.command_request(&invocation, &boundary)?;
        let capture = Arc::new(HookCommandCapture::default());
        let outcome = executor
            .run(request, invocation.cancellation().clone(), capture.clone())
            .await
            .map_err(|error| HookError::new("declarative_hook_command", error.to_string()))?;
        let (stdout, stderr) = capture.finish();
        if outcome.exit_code != 0 && self.hook.registration().class() == HookClass::Policy {
            let message = if !stderr.trim().is_empty() {
                stderr
            } else if !stdout.trim().is_empty() {
                stdout
            } else {
                format!("hook {} exited with {}", self.hook.id(), outcome.exit_code)
            };
            return Ok(HookDirective::Block { message });
        }
        if let HookInput::PostTool(input) = invocation.input()
            && self.hook.registration().class() == HookClass::Transform
            && (outcome.exit_code != 0 || !stdout.is_empty() || !stderr.is_empty())
        {
            let result = HookCommandResult {
                exit_code: outcome.exit_code,
                stdout,
                stderr,
            };
            let mut output = input.output.clone();
            let diagnostics = result.render(&format!("hook {}", self.hook.id()));
            append_post_tool_diagnostics(&mut output, "Declarative hook diagnostics", &diagnostics);
            return Ok(HookDirective::Transform {
                change: HookTransform::PostTool {
                    output,
                    is_error: input.is_error || outcome.exit_code != 0,
                },
            });
        }
        if outcome.exit_code != 0 {
            return Err(HookError::new(
                "declarative_hook_exit",
                format!("hook {} exited with {}", self.hook.id(), outcome.exit_code),
            ));
        }
        Ok(HookDirective::Continue {})
    }
}

pub(super) fn register_declarative_hooks(
    dispatcher: &mut HookDispatcher,
    catalog: &ExtensionCatalog,
    runtime: &Arc<ToolchainRuntime>,
) -> Result<()> {
    for hook in catalog.shell_hooks() {
        if hook.registration().class() == HookClass::Transform
            && hook.registration().event() != HookEvent::PostTool
        {
            return Err(miette!(
                "declarative transform hooks require the post_tool event"
            ));
        }
        if hook.registration().effect() == HookEffect::WorkspaceMutating
            && !matches!(
                hook.registration().event(),
                HookEvent::PreTool | HookEvent::PostTool
            )
        {
            return Err(miette!(
                "declarative lifecycle hook {:?} cannot mutate the workspace without a tool checkpoint; declare `effect = \"read-only\"` or move it to pre_tool/post_tool",
                hook.id()
            ));
        }
        dispatcher
            .register(
                hook.registration().clone(),
                DeclarativeShellHookHandler {
                    hook: hook.clone(),
                    matcher: DeclarativeHookMatcher::compile(hook.matcher())?,
                    runtime: Arc::clone(runtime),
                },
            )
            .map_err(|error| miette!("declarative hook could not register: {error}"))?;
    }
    Ok(())
}
