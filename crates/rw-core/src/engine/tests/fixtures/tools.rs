#![cfg(test)]

use crate::engine::MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS;
use crate::engine::MAX_TOOL_EXECUTION_WINDOW;
use crate::engine::session;
use crate::engine::tests;
use crate::engine::tests::fixtures::support::canonical_json_bytes;
use crate::engine::tests::fixtures::support::descriptor;
use crate::engine::turn::emit;
use async_trait::async_trait;
use futures_util::stream;
use rw_tools::CapabilityManifest;
use rw_tools::MutationScope;
use rw_tools::SubagentLifecycleMode;
use rw_tools::Tool;
use rw_tools::ToolContext;
use rw_tools::ToolDescriptor;
use rw_tools::ToolError;
use rw_tools::ToolOutputChunk;
use rw_tools::ToolResult;
use rw_types::ToolCapability;
use rw_types::ToolOutputStream;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::Notify;

#[derive(Clone)]
pub(in crate::engine::tests) enum StubOutcome {
    Success(ToolResult),
    Failure(String),
}

pub(in crate::engine::tests) struct StubTool {
    pub(in crate::engine::tests) descriptor: ToolDescriptor,
    pub(in crate::engine::tests) behavior: rw_tools::ToolBehavior,
    pub(in crate::engine::tests) outcome: StubOutcome,
    pub(in crate::engine::tests) calls: AtomicUsize,
    pub(in crate::engine::tests) inputs: Mutex<Vec<Value>>,
}

impl StubTool {
    pub(in crate::engine::tests) fn new(
        name: &str,
        capabilities: Vec<ToolCapability>,
        outcome: StubOutcome,
    ) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: name.to_owned(),
                description: format!("fixture {name}"),
                input_schema: json!({"type": "object"}),
                capabilities: CapabilityManifest::new(capabilities),
            },
            behavior: rw_tools::ToolBehavior::Standard,
            outcome,
            calls: AtomicUsize::new(0),
            inputs: Mutex::new(Vec::new()),
        }
    }

    pub(in crate::engine::tests) fn with_behavior(
        mut self,
        behavior: rw_tools::ToolBehavior,
    ) -> Self {
        self.behavior = behavior;
        self
    }
}

#[async_trait]
impl Tool for StubTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn behavior(&self) -> rw_tools::ToolBehavior {
        self.behavior
    }

    async fn execute(&self, _context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inputs.lock().expect("input lock").push(input);
        match &self.outcome {
            StubOutcome::Success(result) => Ok(result.clone()),
            StubOutcome::Failure(message) => Err(ToolError::InvalidInput(message.clone())),
        }
    }
}

pub(in crate::engine::tests) struct PlanMutationTripwire {
    pub(in crate::engine::tests) descriptor: ToolDescriptor,
}

impl PlanMutationTripwire {
    pub(in crate::engine::tests) fn new(name: &str, capabilities: Vec<ToolCapability>) -> Self {
        Self {
            descriptor: ToolDescriptor {
                name: name.to_owned(),
                description: format!("plan-mode mutation tripwire {name}"),
                input_schema: json!({"type": "object"}),
                capabilities: CapabilityManifest::new(capabilities),
            },
        }
    }
}

#[async_trait]
impl Tool for PlanMutationTripwire {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let marker = format!(
            "tripwire-{}-{}",
            self.descriptor.name,
            blake3::hash(canonical_json_bytes(&input).as_slice()).to_hex()
        );
        std::fs::write(context.workspace_root().join(marker), b"MUTATED")
            .map_err(|error| ToolError::Command(error.to_string()))?;
        Ok(ToolResult::new("tripwire executed", Value::Null))
    }
}

pub(in crate::engine::tests) struct ReverseCompletionTool {
    pub(in crate::engine::tests) descriptor: ToolDescriptor,
    pub(in crate::engine::tests) first: bool,
    pub(in crate::engine::tests) release_first: Arc<Notify>,
    pub(in crate::engine::tests) completion_order: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for ReverseCompletionTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        if self.first {
            self.release_first.notified().await;
            self.completion_order
                .lock()
                .expect("completion lock")
                .push(self.descriptor.name.clone());
        } else {
            self.completion_order
                .lock()
                .expect("completion lock")
                .push(self.descriptor.name.clone());
            self.release_first.notify_one();
        }
        Ok(ToolResult::new(&self.descriptor.name, Value::Null))
    }
}

pub(in crate::engine::tests) struct OrderedWindowProbe {
    pub(in crate::engine::tests) started: AtomicUsize,
    pub(in crate::engine::tests) window_filled: Notify,
    pub(in crate::engine::tests) exceeded_window: Notify,
}

#[async_trait]
impl Tool for OrderedWindowProbe {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor("window_probe")
    }

    async fn execute(&self, context: &ToolContext, input: Value) -> Result<ToolResult, ToolError> {
        let started = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        if started == MAX_TOOL_EXECUTION_WINDOW {
            self.window_filled.notify_one();
        } else if started > MAX_TOOL_EXECUTION_WINDOW {
            self.exceeded_window.notify_one();
        }
        if input["index"] == 0 {
            context.cancellation.cancelled().await;
            return Err(ToolError::Cancelled);
        }
        Ok(ToolResult::new(
            "completed behind the first call",
            Value::Null,
        ))
    }
}

pub(in crate::engine::tests) struct StreamingTool {
    pub(in crate::engine::tests) descriptor: ToolDescriptor,
    pub(in crate::engine::tests) release: Arc<Notify>,
    pub(in crate::engine::tests) completed: Arc<AtomicBool>,
}

pub(in crate::engine::tests) struct SaturatingOrderedTool {
    pub(in crate::engine::tests) first: bool,
    pub(in crate::engine::tests) background_full: Arc<Notify>,
}

#[async_trait]
impl Tool for SaturatingOrderedTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        descriptor(if self.first {
            "delayed_first"
        } else {
            "flood_later"
        })
    }

    async fn execute(&self, context: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        if self.first {
            self.background_full.notified().await;
        }
        let count = if self.first {
            1
        } else {
            MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS * 2
        };
        for index in 0..count {
            context
                .output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stdout,
                    content: format!("{}:{index}", self.descriptor().name),
                })
                .await?;
            if !self.first && index + 1 == MAX_IN_FLIGHT_TOOL_OUTPUT_CHUNKS - 1 {
                // On the single-thread executor the next immediately-ready
                // emit runs before the first tool wakes. The old shared
                // semaphore fills here and deadlocks both tools.
                self.background_full.notify_one();
            }
        }
        Ok(ToolResult::new("done", Value::Null))
    }
}

pub(in crate::engine::tests) struct EmptySequentialTool {
    pub(in crate::engine::tests) descriptor: ToolDescriptor,
    pub(in crate::engine::tests) first: bool,
    pub(in crate::engine::tests) first_started: Arc<Notify>,
    pub(in crate::engine::tests) release_first: Arc<Notify>,
    pub(in crate::engine::tests) second_started: Arc<AtomicBool>,
}

pub(in crate::engine::tests) struct SessionCaptureTool {
    pub(in crate::engine::tests) sessions: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Tool for SessionCaptureTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "session_capture".to_owned(),
            description: "captures the engine session id".to_owned(),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::default(),
        }
    }

    async fn execute(&self, context: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        let session = context
            .session_id()
            .ok_or_else(|| ToolError::InvalidInput("missing session".to_owned()))?;
        self.sessions
            .lock()
            .expect("session capture")
            .push(session.0.clone());
        Ok(ToolResult::new("captured", Value::Null))
    }
}

#[async_trait]
impl Tool for EmptySequentialTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        if self.first {
            self.first_started.notify_one();
            self.release_first.notified().await;
        } else {
            self.second_started.store(true, Ordering::SeqCst);
        }
        Ok(ToolResult::new("done", Value::Null))
    }
}

#[async_trait]
impl Tool for StreamingTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    async fn execute(&self, context: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        context
            .output
            .emit(ToolOutputChunk {
                stream: ToolOutputStream::Stdout,
                content: "live chunk".to_owned(),
            })
            .await?;
        tokio::select! {
            () = self.release.notified() => {
                self.completed.store(true, Ordering::SeqCst);
                Ok(ToolResult::new("done", Value::Null))
            }
            () = context.cancellation.cancelled() => Err(ToolError::Cancelled),
        }
    }
}

pub(in crate::engine::tests) struct CleanupTool {
    pub(in crate::engine::tests) cleanup_finished: Arc<AtomicBool>,
}

#[async_trait]
impl Tool for CleanupTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "cleanup_tool".to_owned(),
            description: "cooperative cancellation fixture".to_owned(),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
        }
    }

    async fn execute(&self, context: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        context.cancellation.cancelled().await;
        context
            .output
            .emit(ToolOutputChunk {
                stream: ToolOutputStream::Stderr,
                content: "cleanup complete".to_owned(),
            })
            .await?;
        tokio::task::yield_now().await;
        self.cleanup_finished.store(true, Ordering::SeqCst);
        Err(ToolError::Cancelled)
    }
}

pub(in crate::engine::tests) struct PanickingTool;

#[derive(Default)]
pub(in crate::engine::tests) struct ExternalCleanupTool {
    pub(in crate::engine::tests) started: Notify,
    pub(in crate::engine::tests) execution_dropped: Arc<AtomicBool>,
    pub(in crate::engine::tests) cleanup_started: Notify,
    pub(in crate::engine::tests) release_cleanup: Notify,
    pub(in crate::engine::tests) cleanup_finished: AtomicBool,
}

#[async_trait]
impl Tool for ExternalCleanupTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "external_cleanup".to_owned(),
            description: "externally owned effects fixture".to_owned(),
            input_schema: json!({"type":"object"}),
            capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
        }
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        struct MarkDropped(Arc<AtomicBool>);
        impl Drop for MarkDropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let _mark = MarkDropped(Arc::clone(&self.execution_dropped));
        self.started.notify_one();
        std::future::pending().await
    }

    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        assert!(self.execution_dropped.load(Ordering::SeqCst));
        self.cleanup_started.notify_one();
        self.release_cleanup.notified().await;
        self.cleanup_finished.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Tool for PanickingTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "panic_tool".to_owned(),
            description: "panic fixture".to_owned(),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
        }
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        panic!("fixture tool panic")
    }
}

pub(in crate::engine::tests) struct FileMutatingBash {
    pub(in crate::engine::tests) path: PathBuf,
}

#[async_trait]
impl Tool for FileMutatingBash {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "bash".to_owned(),
            description: "mutating command prelude fixture".to_owned(),
            input_schema: json!({"type":"object"}),
            capabilities: CapabilityManifest::new([
                ToolCapability::Execute,
                ToolCapability::WriteFilesystem,
            ]),
        }
    }

    fn mutation_scope(&self, _input: &Value) -> MutationScope {
        MutationScope::OpaqueWorkspace
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        std::fs::write(&self.path, "mutated by command prelude")
            .map_err(|error| ToolError::Command(error.to_string()))?;
        Ok(ToolResult::new("mutated", Value::Null))
    }
}

pub(in crate::engine::tests) struct FloodOutputTool;

#[async_trait]
impl Tool for FloodOutputTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "flood".to_owned(),
            description: "emit more live chunks than the bounded stream permits".to_owned(),
            input_schema: json!({"type": "object"}),
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    async fn execute(&self, context: &ToolContext, _input: Value) -> Result<ToolResult, ToolError> {
        for _ in 0..1_100 {
            context
                .output
                .emit(ToolOutputChunk {
                    stream: ToolOutputStream::Stdout,
                    content: "x".to_owned(),
                })
                .await?;
        }
        Ok(ToolResult::new("drained", Value::Null))
    }
}

pub(in crate::engine::tests) struct ThirdPartyLifecycleTool;

#[async_trait]
impl Tool for ThirdPartyLifecycleTool {
    async fn settle_effects(&self) -> std::result::Result<(), rw_tools::ToolError> {
        Ok(())
    }

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: "third_party_children".to_owned(),
            description: "fixture extension lifecycle producer".to_owned(),
            input_schema: Value::Null,
            capabilities: CapabilityManifest::new([ToolCapability::ReadFilesystem]),
        }
    }

    fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
        SubagentLifecycleMode::MultipleOrdered
    }

    async fn execute(
        &self,
        _context: &ToolContext,
        _input: Value,
    ) -> Result<ToolResult, ToolError> {
        Ok(ToolResult::new("done", Value::Null))
    }
}
