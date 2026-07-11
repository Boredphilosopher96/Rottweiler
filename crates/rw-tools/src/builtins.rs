use std::sync::Arc;

use crate::bash::{BashTool, CommandExecutor};
use crate::files::{EditTool, MultiEditTool, ReadTool, WriteTool};
use crate::interaction::{AskUserTool, QuestionAsker, SubmitPlanTool, TodoTool};
use crate::registry::{Tool, ToolError, ToolLimits, ToolRegistry};
use crate::search::{GlobTool, GrepTool, LsTool};
use crate::symbols::{SymbolsTool, WorkspaceSymbolIndex};
use crate::web::{WebFetchTool, WebFetcher};

/// Host-provided boundaries required by the complete first-party tool set.
pub struct BuiltinDependencies {
    pub command_executor: Arc<dyn CommandExecutor>,
    pub web_fetcher: Arc<dyn WebFetcher>,
    pub question_asker: Arc<dyn QuestionAsker>,
    pub symbol_index: Arc<WorkspaceSymbolIndex>,
    pub limits: ToolLimits,
}

/// Session-lifecycle handles retained by core after registry construction.
pub struct BuiltinHandles {
    pub todo: Arc<TodoTool>,
    pub symbol_index: Arc<WorkspaceSymbolIndex>,
}

/// Register the first-party tool set through the same public API used by extensions.
///
/// # Errors
///
/// Returns [`ToolError::DuplicateTool`] without registering anything if a built-in name is
/// already present.
pub fn register_builtins(
    registry: &mut ToolRegistry,
    dependencies: BuiltinDependencies,
) -> Result<BuiltinHandles, ToolError> {
    let limits = dependencies.limits;
    let symbol_index = dependencies.symbol_index;
    let todo = Arc::new(TodoTool::new(limits));
    let todo_tool: Arc<dyn Tool> = todo.clone();
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(ReadTool::new(limits)),
        Arc::new(WriteTool::new(limits).with_symbol_index(Arc::clone(&symbol_index))),
        Arc::new(EditTool::new(limits).with_symbol_index(Arc::clone(&symbol_index))),
        Arc::new(MultiEditTool::new(limits).with_symbol_index(Arc::clone(&symbol_index))),
        Arc::new(GrepTool::new(limits)),
        Arc::new(GlobTool::new(limits)),
        Arc::new(LsTool::new(limits)),
        Arc::new(BashTool::new(dependencies.command_executor, limits)),
        Arc::new(WebFetchTool::new(dependencies.web_fetcher, limits)),
        todo_tool,
        Arc::new(AskUserTool::new(dependencies.question_asker, limits)),
        Arc::new(SubmitPlanTool),
        Arc::new(SymbolsTool::new(Arc::clone(&symbol_index), limits)),
    ];
    for tool in &tools {
        let name = tool.descriptor().name;
        if registry.resolve(&name).is_some() {
            return Err(ToolError::DuplicateTool(name));
        }
    }
    symbol_index
        .index_workspaces()
        .map_err(|error| ToolError::Intelligence(error.to_string()))?;
    for tool in tools {
        registry.register(tool)?;
    }
    Ok(BuiltinHandles { todo, symbol_index })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use async_trait::async_trait;
    use rw_intel::SymbolQuery;
    use tempfile::tempdir;

    use super::*;
    use crate::{
        CancellationToken, CommandOutcome, CommandRequest, FetchRequest, FetchResponse,
        ToolOutputSink,
    };

    struct NoCommand;

    #[async_trait]
    impl CommandExecutor for NoCommand {
        async fn run(
            &self,
            _request: CommandRequest,
            _cancellation: CancellationToken,
            _output: Arc<dyn ToolOutputSink>,
        ) -> Result<CommandOutcome, ToolError> {
            Err(ToolError::Command("disabled in test".to_owned()))
        }
    }

    struct NoFetch;

    #[async_trait]
    impl WebFetcher for NoFetch {
        async fn fetch(
            &self,
            _request: FetchRequest,
            _cancellation: CancellationToken,
        ) -> Result<FetchResponse, ToolError> {
            Err(ToolError::Network("disabled in test".to_owned()))
        }
    }

    struct NoQuestion;

    #[async_trait]
    impl QuestionAsker for NoQuestion {
        async fn ask(
            &self,
            _request: crate::AskUserInput,
            _cancellation: CancellationToken,
        ) -> Result<String, ToolError> {
            Err(ToolError::Interaction("disabled in test".to_owned()))
        }
    }

    #[test]
    fn registers_the_complete_builtin_set_atomically_on_name_conflicts() {
        let root = tempdir().expect("temp directory");
        fs::write(root.path().join("startup.rs"), "pub struct StartupSymbol;")
            .expect("startup source");
        let index = Arc::new(WorkspaceSymbolIndex::new([root.path()]).expect("index"));
        let dependencies = || BuiltinDependencies {
            command_executor: Arc::new(NoCommand),
            web_fetcher: Arc::new(NoFetch),
            question_asker: Arc::new(NoQuestion),
            symbol_index: Arc::clone(&index),
            limits: ToolLimits::default(),
        };
        let mut registry = ToolRegistry::new();
        register_builtins(&mut registry, dependencies()).expect("builtins");
        assert_eq!(registry.len(), 13);
        assert!(
            index
                .query(&SymbolQuery {
                    pattern: "StartupSymbol".to_owned(),
                    roles: Vec::new(),
                    languages: Vec::new(),
                    limit: 10,
                })
                .expect("startup symbols")
                .iter()
                .any(|symbol| symbol.name == "StartupSymbol")
        );
        for name in [
            "read",
            "write",
            "edit",
            "multi_edit",
            "grep",
            "glob",
            "ls",
            "bash",
            "webfetch",
            "todo",
            "ask_user",
            "submit_plan",
            "symbols",
        ] {
            assert!(registry.resolve(name).is_some(), "missing {name}");
        }

        let before = registry.len();
        assert!(matches!(
            register_builtins(&mut registry, dependencies()),
            Err(ToolError::DuplicateTool(_))
        ));
        assert_eq!(registry.len(), before);
    }
}
