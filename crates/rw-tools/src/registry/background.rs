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
