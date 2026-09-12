use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static READ_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("read", "Read file", || {
        vec![
            fields::text("path", "File", &["path"]),
            fields::text("start", "Start line", &["start_line"]),
            fields::text("lines", "Total lines", &["total_lines"]),
            fields::text("bytes", "Bytes", &["bytes"]),
        ]
    });
pub(super) static WRITE_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("write", "Write file", || {
        vec![
            fields::text("path", "File", &["path"]),
            fields::text("bytes", "Bytes written", &["bytes"]),
        ]
    });
pub(super) static EDIT_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("edit", "Edit file", || {
        vec![
            fields::text("path", "File", &["path"]),
            fields::text("match", "Match mode", &["match_mode"]),
        ]
    });
pub(super) static MULTI_EDIT_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("multi_edit", "Edit file", || {
        vec![
            fields::text("path", "File", &["path"]),
            fields::text("edits", "Edits applied", &["edits"]),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [
            &READ_PRESENTATION,
            &WRITE_PRESENTATION,
            &EDIT_PRESENTATION,
            &MULTI_EDIT_PRESENTATION,
        ] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
