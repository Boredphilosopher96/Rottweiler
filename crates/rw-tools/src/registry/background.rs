use super::{Arc, SubagentEventSink, ToolContext};
impl ToolContext {
    /// Binds session-owned child delivery that survives the invoking tool and turn.
    #[must_use]
    pub fn with_background_subagent_event_sink(mut self, sink: Arc<dyn SubagentEventSink>) -> Self {
        self.background_subagent_events = Some(sink);
        self
    }
    #[must_use]
    pub fn background_subagent_event_sink(&self) -> Option<&Arc<dyn SubagentEventSink>> {
        self.background_subagent_events.as_ref()
    }
}

/// Session-owned work that holds the workspace lock after its invoking tool returned.
///
/// While any holder is active, workspace-mutating tool calls, completion hooks,
/// and idle-only session commands fail closed. Read-only work and children in
/// private worktrees never hold the lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionActivity {
    /// A `bash` process started with `run_in_background` is still running.
    BackgroundShell,
    /// A child agent with write access runs in the parent's shared workspace.
    SharedWorkspaceChild,
}

impl SessionActivity {
    /// What currently holds the workspace lock.
    #[must_use]
    pub const fn holder(self) -> &'static str {
        match self {
            Self::BackgroundShell => "a background shell process is running",
            Self::SharedWorkspaceChild => "a child agent is editing the shared workspace",
        }
    }

    /// The action that releases the lock.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::BackgroundShell => "wait for it to exit or stop it first",
            Self::SharedWorkspaceChild => {
                "wait for the child to finish or cancel it; use worktree isolation for children that edit in parallel"
            }
        }
    }

    /// Complete refusal for an operation that requires the workspace lock.
    #[must_use]
    pub fn blocked(self, operation: &str) -> String {
        format!(
            "{operation} is blocked because {}; {}",
            self.holder(),
            self.remedy()
        )
    }
}
