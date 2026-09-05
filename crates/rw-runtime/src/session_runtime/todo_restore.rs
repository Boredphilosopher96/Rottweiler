use miette::Result;
use miette::miette;
use rw_tools::TodoTool;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_types::Block;
use rw_types::SessionId;
use rw_types::Turn;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

pub(super) async fn restore_todo_state(
    conversation: &[Turn],
    workspace: &Path,
    session_id: &SessionId,
    todo: &Arc<TodoTool>,
) -> Result<()> {
    todo.clear_session(session_id).await;
    let context = ToolContext::new(workspace)
        .map_err(|error| miette!("todo restore context failed: {error}"))?
        .with_session_id(session_id.clone());
    let mut pending = HashMap::new();
    for turn in conversation {
        for block in &turn.blocks {
            match block {
                Block::ToolCall { id, name, args } if name == "todo" => {
                    pending.insert(id.0.clone(), args.clone());
                }
                Block::ToolResult {
                    id,
                    is_error: false,
                    ..
                } => {
                    if let Some(arguments) = pending.remove(&id.0) {
                        todo.execute(&context, arguments)
                            .await
                            .map_err(|error| miette!("persisted todo state is invalid: {error}"))?;
                    }
                }
                Block::ToolResult { id, .. } => {
                    pending.remove(&id.0);
                }
                _ => {}
            }
        }
    }
    Ok(())
}
