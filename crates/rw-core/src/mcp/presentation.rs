use rw_tools::presentation::{BuiltinToolPresentation, fields};
use rw_types::extension_ui::UiField;

pub(super) static SEARCH: BuiltinToolPresentation =
    BuiltinToolPresentation::new("tool_search", "Available tools", || {
        vec![
            fields::table(
                "matches",
                "Tools",
                &["matches"],
                &[
                    ("Server", &["server"]),
                    ("Tool", &["name"]),
                    ("Description", &["description"]),
                ],
            ),
            fields::badge("truncated", "More matches available", &["truncated"]),
        ]
    });
pub(super) static CALL: BuiltinToolPresentation =
    BuiltinToolPresentation::new("mcp_call", "MCP tool result", result_fields);
pub(super) static RESOURCE: BuiltinToolPresentation =
    BuiltinToolPresentation::new("mcp_read_resource", "MCP resource", result_fields);
pub(super) static PROMPT: BuiltinToolPresentation =
    BuiltinToolPresentation::new("mcp_get_prompt", "MCP prompt", result_fields);
pub(super) static OVERFLOW: BuiltinToolPresentation =
    BuiltinToolPresentation::new("mcp_overflow_read", "MCP result content", || {
        vec![
            fields::text("artifact", "Artifact", &["artifact_id"]),
            fields::text("offset", "Offset", &["offset"]),
            fields::text("bytes", "Returned bytes", &["returned_bytes"]),
            fields::text("total", "Total bytes", &["original_bytes"]),
        ]
    });
fn result_fields() -> Vec<UiField> {
    vec![
        fields::text("server", "Server", &["server"]),
        fields::text("operation", "Operation", &["operation"]),
        fields::badge("format", "Format", &["format"]),
        fields::badge("truncated", "Truncated", &["truncated"]),
        fields::text("overflow", "Full result artifact", &["overflow", "id"]),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_plans_cover_each_registered_mcp_result_owner() {
        for declaration in [&SEARCH, &CALL, &RESOURCE, &PROMPT, &OVERFLOW] {
            declaration.plan().unwrap_or_else(|error| panic!("{error}"));
        }
    }
}
