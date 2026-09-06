use rw_tools::presentation::{BuiltinToolPresentation, fields};

pub(super) static RESULT: BuiltinToolPresentation =
    BuiltinToolPresentation::new("spawn_agent", "Child task result", || {
        vec![
            fields::text("child", "Child", &["subagent_id"]),
            fields::badge("status", "Status", &["status"]),
            fields::text("summary", "Summary", &["final_text"]),
            fields::list("files", "Changed files", &["touched_files"]),
        ]
    });
pub(super) static CONTROL: BuiltinToolPresentation =
    BuiltinToolPresentation::new("spawn_agent", "Child task control", || {
        vec![
            fields::text("child", "Child", &["subagent_id"]),
            fields::badge("action", "Action", &["action"]),
            fields::badge("completed", "Completed", &["completed"]),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_plans_cover_child_results_and_control_outcomes() {
        for declaration in [&RESULT, &CONTROL] {
            declaration.plan().unwrap_or_else(|error| panic!("{error}"));
        }
    }
}
