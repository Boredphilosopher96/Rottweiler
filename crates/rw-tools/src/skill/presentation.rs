use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static SKILL_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("skill", "Load skill", || {
        vec![
            fields::text("name", "Skill", &["name"]),
            fields::text("path", "Bundled file", &["path"]),
            fields::text("bytes", "Bytes", &["bytes"]),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        let first = SKILL_PRESENTATION
            .plan()
            .unwrap_or_else(|error| panic!("{error}"));
        let second = SKILL_PRESENTATION
            .plan()
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(first, second);
    }
}
