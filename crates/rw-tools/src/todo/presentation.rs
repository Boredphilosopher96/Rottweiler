use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static TODO_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("todo", "Tasks", || {
        vec![fields::table(
            "items",
            "Tasks",
            &["items"],
            &[("Task", &["content"]), ("Status", &["status"])],
        )]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [&TODO_PRESENTATION] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
