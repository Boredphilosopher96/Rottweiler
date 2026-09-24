use rw_tools::presentation::{BuiltinToolPresentation, fields};

pub(super) static STATUS: BuiltinToolPresentation =
    BuiltinToolPresentation::new("spawn_agent", "Child agent", || {
        vec![
            fields::text("child", "Child", &["id"]),
            fields::badge("status", "Status", &["status"]),
        ]
    });
pub(super) static WAIT: BuiltinToolPresentation =
    BuiltinToolPresentation::new("spawn_agent", "Child agents", || {
        vec![fields::list("children", "Children", &["children"])]
    });
pub(super) static CONTROL: BuiltinToolPresentation =
    BuiltinToolPresentation::new("spawn_agent", "Child agent control", || {
        vec![
            fields::text("child", "Child", &["id"]),
            fields::badge("action", "Action", &["action"]),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_plans_cover_child_status_and_control_outcomes() {
        for declaration in [&STATUS, &WAIT, &CONTROL] {
            declaration.plan().unwrap_or_else(|error| panic!("{error}"));
        }
    }
}
