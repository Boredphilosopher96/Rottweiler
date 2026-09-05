use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static APPLY_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("apply_worktree_diff", "Apply isolated changes", || {
        vec![
            fields::text("artifact", "Artifact", &["artifact_id"]),
            fields::text("base", "Base commit", &["base_commit"]),
            fields::table(
                "files",
                "Changed files",
                &["touched_files"],
                &[("File", &["path"]), ("Status", &["status"])],
            ),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        {
            let declaration = &APPLY_PRESENTATION;
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
