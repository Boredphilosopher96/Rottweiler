use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static SEARCH_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("websearch", "Search the web", || {
        vec![
            fields::text("count", "Results", &["count"]),
            fields::table(
                "results",
                "Results",
                &["results"],
                &[
                    ("Title", &["title"]),
                    ("URL", &["url"]),
                    ("Snippet", &["snippet"]),
                ],
            ),
        ]
    });
pub(super) static FETCH_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("webfetch", "Fetch page", || {
        vec![
            fields::text("url", "URL", &["final_url"]),
            fields::badge("status", "HTTP status", &["status"]),
            fields::text("type", "Content type", &["content_type"]),
            fields::text("bytes", "Retained bytes", &["bytes"]),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [&SEARCH_PRESENTATION, &FETCH_PRESENTATION] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
