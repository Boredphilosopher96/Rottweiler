#![cfg(test)]

use crate::engine::tests::fixtures::checkpoints::RecordingCheckpoints;
use async_trait::async_trait;
use rw_ext::HookDirective;
use rw_ext::HookError;
use rw_ext::HookHandler;
use rw_ext::HookInvocation;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

pub(in crate::engine::tests) struct MutatingPreHook {
    pub(in crate::engine::tests) checkpoints: Arc<RecordingCheckpoints>,
    pub(in crate::engine::tests) sibling: PathBuf,
}

#[async_trait]
impl HookHandler for MutatingPreHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, _invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        if !self
            .checkpoints
            .events
            .lock()
            .expect("checkpoint events")
            .iter()
            .any(|event| event.starts_with("begin:"))
        {
            return Err(HookError::new(
                "missing_checkpoint",
                "mutating pre hook ran before checkpoint begin",
            ));
        }
        std::fs::write(&self.sibling, "mutated by pre hook")
            .map_err(|error| HookError::new("fixture_write", error.to_string()))?;
        Ok(HookDirective::Continue)
    }
}

pub(in crate::engine::tests) struct FixedHook {
    pub(in crate::engine::tests) label: &'static str,
    pub(in crate::engine::tests) calls: Arc<Mutex<Vec<String>>>,
    pub(in crate::engine::tests) result: Result<HookDirective, HookError>,
}

pub(in crate::engine::tests) struct MarkPostToolFailed;

pub(in crate::engine::tests) struct SiblingFormatterPostHook {
    pub(in crate::engine::tests) sibling: PathBuf,
}

#[async_trait]
impl HookHandler for MarkPostToolFailed {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        let mut payload = invocation.payload().clone();
        payload["is_error"] = Value::Bool(true);
        Ok(HookDirective::Replace(payload))
    }
}

#[async_trait]
impl HookHandler for SiblingFormatterPostHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, _invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        std::fs::write(&self.sibling, "formatted sibling")
            .map_err(|error| HookError::new("formatter_write", error.to_string()))?;
        Ok(HookDirective::Continue)
    }
}

pub(in crate::engine::tests) struct PayloadCaptureHook {
    pub(in crate::engine::tests) label: &'static str,
    pub(in crate::engine::tests) payloads: Arc<Mutex<Vec<(&'static str, Value)>>>,
}

#[async_trait]
impl HookHandler for FixedHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, _invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        self.calls
            .lock()
            .expect("hook call lock")
            .push(self.label.to_owned());
        self.result.clone()
    }
}

#[async_trait]
impl HookHandler for PayloadCaptureHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        self.payloads
            .lock()
            .expect("captured hook payloads")
            .push((self.label, invocation.payload().clone()));
        Ok(HookDirective::Continue)
    }
}

pub(in crate::engine::tests) struct RewriteArgumentsHook(pub(in crate::engine::tests) Value);

pub(in crate::engine::tests) struct RewriteUserPromptHook(
    pub(in crate::engine::tests) &'static str,
);

pub(in crate::engine::tests) struct NeverHook;

pub(in crate::engine::tests) struct PermissionAllowHook;

#[async_trait]
impl HookHandler for PermissionAllowHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        let mut payload = invocation.payload().clone();
        payload["decision"] = Value::String("allow".to_owned());
        Ok(HookDirective::Replace(payload))
    }
}

#[async_trait]
impl HookHandler for NeverHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, _invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        std::future::pending().await
    }
}

#[async_trait]
impl HookHandler for RewriteArgumentsHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        let mut payload = invocation.payload().clone();
        payload["arguments"] = self.0.clone();
        Ok(HookDirective::Replace(payload))
    }
}

#[async_trait]
impl HookHandler for RewriteUserPromptHook {
    async fn settle_effects(&self) -> std::result::Result<(), rw_ext::HookError> {
        Ok(())
    }

    async fn invoke(&self, invocation: HookInvocation<'_>) -> Result<HookDirective, HookError> {
        let mut payload = invocation.payload().clone();
        payload["content"] = Value::String(self.0.to_owned());
        Ok(HookDirective::Replace(payload))
    }
}
