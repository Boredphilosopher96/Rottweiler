use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static SYMBOLS_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("symbols", "Search symbols", || {
        vec![
            fields::text("count", "Symbols", &["count"]),
            fields::table(
                "matches",
                "Symbols",
                &["matches"],
                &[
                    ("Name", &["name"]),
                    ("Kind", &["kind"]),
                    ("File", &["location", "path"]),
                    ("Line", &["location", "line"]),
                ],
            ),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        {
            let declaration = &SYMBOLS_PRESENTATION;
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
