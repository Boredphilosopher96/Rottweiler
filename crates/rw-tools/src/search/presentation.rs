use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static GREP_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("grep", "Search contents", || {
        vec![
            fields::text("count", "Matches", &["count"]),
            fields::table(
                "matches",
                "Matches",
                &["matches"],
                &[
                    ("File", &["path"]),
                    ("Line", &["line"]),
                    ("Text", &["text"]),
                ],
            ),
        ]
    });
pub(super) static GLOB_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("glob", "Find files", || {
        vec![
            fields::text("count", "Files", &["count"]),
            fields::list("paths", "Files", &["paths"]),
        ]
    });
pub(super) static LS_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("ls", "List directory", || {
        vec![
            fields::text("count", "Entries", &["count"]),
            fields::table(
                "entries",
                "Entries",
                &["entries"],
                &[
                    ("Kind", &["kind"]),
                    ("File", &["path"]),
                    ("Bytes", &["bytes"]),
                ],
            ),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [&GREP_PRESENTATION, &GLOB_PRESENTATION, &LS_PRESENTATION] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
