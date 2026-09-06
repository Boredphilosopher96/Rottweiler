use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static PLAN_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("submit_plan", "Submitted plan", || {
        vec![
            fields::text("title", "Title", &["title"]),
            fields::text("summary", "Summary", &["summary_md"]),
            fields::table(
                "steps",
                "Steps",
                &["steps"],
                &[
                    ("Description", &["description"]),
                    ("Verification", &["verification"]),
                ],
            ),
            fields::list("questions", "Open questions", &["open_questions"]),
        ]
    });
pub(super) static ANSWER_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("ask_user", "Answer", || {
        vec![fields::text("answer", "Answer", &["answer"])]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [&PLAN_PRESENTATION, &ANSWER_PRESENTATION] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
