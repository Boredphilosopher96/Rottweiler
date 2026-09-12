use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static DIAGNOSTICS_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("diagnostics", "Diagnostics", || {
        vec![
            fields::badge("backend", "Backend", &["backend"]),
            fields::text("note", "Note", &["note"]),
            fields::table(
                "diagnostics",
                "Diagnostics",
                &["diagnostics"],
                &[
                    ("File", &["path"]),
                    ("Severity", &["severity"]),
                    ("Message", &["message"]),
                    ("Line (zero-based)", &["range", "start", "line"]),
                ],
            ),
        ]
    });
pub(super) static DEFINITION_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("definition", "Definitions", || locations("definitions"));
pub(super) static REFERENCES_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("references", "References", || locations("references"));
pub(super) static RENAME_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("rename", "Rename edit plan", || {
        vec![
            fields::badge("backend", "Backend", &["backend"]),
            fields::badge("applied", "Applied", &["applied"]),
            fields::text("note", "Note", &["note"]),
            fields::table(
                "edits",
                "Proposed edits",
                &["edits"],
                &[("File", &["path"]), ("New text", &["new_text"])],
            ),
        ]
    });
fn locations(field: &str) -> Vec<rw_types::extension_ui::UiField> {
    vec![
        fields::badge("backend", "Backend", &["backend"]),
        fields::text("note", "Note", &["note"]),
        fields::table(
            "locations",
            "Locations",
            &[field],
            &[
                ("File", &["path"]),
                ("Line (zero-based)", &["range", "start", "line"]),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [
            &DIAGNOSTICS_PRESENTATION,
            &DEFINITION_PRESENTATION,
            &REFERENCES_PRESENTATION,
            &RENAME_PRESENTATION,
        ] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
