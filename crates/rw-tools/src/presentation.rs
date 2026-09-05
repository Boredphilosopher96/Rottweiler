//! Host-owned display projection plans. Plugin responses cannot deserialize them.
mod builtin;
pub use builtin::BuiltinToolPresentation;
pub mod fields;
use rw_types::{
    ToolOutput, ToolOutputPart,
    extension_ui::{
        UiContractError, UiContribution, UiContributionOwner, UiField, UiPresentation,
        validate_contributions,
    },
};
use serde_json::Value;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolPresentationPlan {
    owner: UiContributionOwner,
    declaration: Arc<UiContribution>,
}
impl ToolPresentationPlan {
    /// # Errors
    /// Rejects invalid or non-tool declarations before the execution result owns a plan.
    pub fn new(
        owner: UiContributionOwner,
        declaration: Arc<UiContribution>,
    ) -> Result<Self, UiContractError> {
        validate_contributions(std::slice::from_ref(declaration.as_ref()))?;
        if !matches!(declaration.as_ref(), UiContribution::Tool { .. }) {
            return Err(UiContractError("tool presentation declaration"));
        }
        Ok(Self { owner, declaration })
    }
    /// Projects the authoritative, redacted post-hook output. The model payload
    /// remains independent from this bounded display value.
    ///
    /// # Errors
    /// Rejects a malformed display owner or an exhausted descriptor allowance.
    pub fn project(
        &self,
        output: &ToolOutput,
        redact: impl Fn(&str) -> String,
    ) -> Result<UiPresentation, UiContractError> {
        let source = match output {
            ToolOutput::Structured { value } => value,
            ToolOutput::Mixed { parts } => parts
                .iter()
                .find_map(|part| match part {
                    ToolOutputPart::Structured { value } => value.get("data"),
                    _ => None,
                })
                .unwrap_or(&Value::Null),
            ToolOutput::Text { .. } => &Value::Null,
        };
        let mut declaration = self.declaration.as_ref().clone();
        let (title, fields, actions) = match &mut declaration {
            UiContribution::Tool {
                title,
                fields,
                actions,
                ..
            }
            | UiContribution::Panel {
                title,
                fields,
                actions,
                ..
            } => (title, fields, actions),
        };
        *title = label(&redact, title);
        for field in fields {
            match field {
                UiField::Text { label: text, .. }
                | UiField::Badge { label: text, .. }
                | UiField::List { label: text, .. } => *text = label(&redact, text),
                UiField::Table {
                    label: text,
                    columns,
                    ..
                } => {
                    *text = label(&redact, text);
                    for column in columns {
                        column.label = label(&redact, &column.label);
                    }
                }
            }
        }
        for action in actions {
            action.label = label(&redact, &action.label);
        }
        UiPresentation::project(self.owner.clone(), &declaration, source)
    }
}
fn label(redact: &impl Fn(&str) -> String, value: &str) -> String {
    let mut value = redact(value);
    let mut end = value.len().min(rw_types::extension_ui::MAX_UI_LABEL_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

#[cfg(test)]
mod tests {
    use super::ToolPresentationPlan;
    use crate::ToolResult;
    use rw_types::{
        ToolOutput, ToolOutputPart,
        extension_ui::{
            UiContribution, UiContributionOwner, UiField, UiGenerationId, UiProjectedField,
            UiSelectorStep,
        },
    };
    use serde_json::json;
    use std::sync::Arc;
    #[test]
    fn plugin_wire_cannot_forge_host_presentation_metadata() {
        assert!(
            serde_json::from_value::<ToolResult>(
                json!({"content":"text","data":null,"presentation":{}})
            )
            .is_err()
        );
    }
    #[test]
    fn display_uses_the_authoritative_post_hook_output_and_redacts_labels() {
        let declaration = Arc::new(UiContribution::Tool {
            id: "view".into(),
            tool_name: "example".into(),
            title: "secret".into(),
            fields: vec![UiField::Text {
                id: "state".into(),
                label: "State".into(),
                path: vec![UiSelectorStep::Field {
                    name: "state".into(),
                }],
            }],
            actions: Vec::new(),
        });
        let plan = ToolPresentationPlan::new(
            UiContributionOwner {
                extension: "example".into(),
                generation: UiGenerationId::from_bytes([1; 16]),
            },
            declaration,
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let output = ToolOutput::Mixed {
            parts: vec![ToolOutputPart::Structured {
                value: json!({"data":{"state":"post-hook redacted state"},"truncated":false}),
            }],
        };
        let projected = plan
            .project(&output, |text| text.replace("secret", "[REDACTED]"))
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(projected.descriptor.title, "[REDACTED]");
        assert!(
            matches!(&projected.projected.fields[0],UiProjectedField::Text{value:Some(value),..} if value=="post-hook redacted state")
        );
    }
}
