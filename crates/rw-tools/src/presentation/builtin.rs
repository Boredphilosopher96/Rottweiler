//! Process-owned declarations beside first-party result producers.
use super::ToolPresentationPlan;
use crate::{ToolError, ToolResult};
use rw_types::extension_ui::{UiContribution, UiContributionOwner, UiField, UiGenerationId};
use std::sync::{Arc, OnceLock};

/// A validated immutable plan is shared across executions. Its generation is
/// the digest of its declaration, so persisted surfaces retain their identity.
pub struct BuiltinToolPresentation {
    name: &'static str,
    title: &'static str,
    fields: fn() -> Vec<UiField>,
    plan: OnceLock<Result<ToolPresentationPlan, String>>,
}
impl BuiltinToolPresentation {
    #[must_use]
    pub const fn new(
        name: &'static str,
        title: &'static str,
        fields: fn() -> Vec<UiField>,
    ) -> Self {
        Self {
            name,
            title,
            fields,
            plan: OnceLock::new(),
        }
    }

    /// # Errors
    /// Rejects an invalid source declaration before returning a display plan.
    pub fn plan(&self) -> Result<ToolPresentationPlan, ToolError> {
        self.plan
            .get_or_init(|| {
                let declaration = Arc::new(UiContribution::Tool {
                    id: self.name.into(),
                    tool_name: self.name.into(),
                    title: self.title.into(),
                    fields: (self.fields)(),
                    actions: Vec::new(),
                });
                let encoded =
                    serde_json::to_vec(declaration.as_ref()).map_err(|error| error.to_string())?;
                let hash = blake3::hash(&encoded);
                let mut generation = [0; 16];
                generation.copy_from_slice(&hash.as_bytes()[..16]);
                ToolPresentationPlan::new(
                    UiContributionOwner {
                        extension: "rottweiler".into(),
                        generation: UiGenerationId::from_bytes(generation),
                    },
                    declaration,
                )
                .map_err(|error| error.to_string())
            })
            .clone()
            .map_err(ToolError::Output)
    }

    /// # Errors
    /// Rejects a malformed source-owned declaration.
    pub fn attach(&self, result: ToolResult) -> Result<ToolResult, ToolError> {
        Ok(result.with_presentation(self.plan()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn immutable_declarations_are_prepared_once_and_generation_tracks_the_definition() {
        static BUILDS: AtomicUsize = AtomicUsize::new(0);
        let owner = BuiltinToolPresentation::new("fixture", "Result", || {
            BUILDS.fetch_add(1, Ordering::Relaxed);
            vec![crate::presentation::fields::text(
                "value",
                "Value",
                &["value"],
            )]
        });
        let first = owner.plan().unwrap_or_else(|error| panic!("{error}"));
        for _ in 0..100 {
            let current = owner.plan().unwrap_or_else(|error| panic!("{error}"));
            assert!(Arc::ptr_eq(&first.declaration, &current.declaration));
        }
        assert_eq!(BUILDS.load(Ordering::Relaxed), 1);
        let changed = BuiltinToolPresentation::new("fixture", "Changed result", || {
            vec![crate::presentation::fields::text(
                "value",
                "Value",
                &["value"],
            )]
        })
        .plan()
        .unwrap_or_else(|error| panic!("{error}"));
        assert_ne!(first.owner.generation, changed.owner.generation);
    }
}
