use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static STATUS_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("background_status", "Background processes", || {
        vec![fields::table(
            "processes",
            "Processes",
            &["processes"],
            &[
                ("Process", &["process_id"]),
                ("Status", &["status", "state"]),
                ("Retained bytes", &["retained_output_bytes"]),
                ("Dropped bytes", &["dropped_output_bytes"]),
            ],
        )]
    });
pub(super) static OUTPUT_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("background_output", "Background output", || {
        vec![
            fields::text("process", "Process", &["process", "process_id"]),
            fields::badge("status", "Status", &["process", "status", "state"]),
            fields::text(
                "bytes",
                "Retained bytes",
                &["process", "retained_output_bytes"],
            ),
        ]
    });
pub(super) static KILL_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("background_kill", "Stop background process", || {
        vec![
            fields::text("process", "Process", &["process", "process_id"]),
            fields::badge("status", "Status", &["process", "status", "state"]),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [
            &STATUS_PRESENTATION,
            &OUTPUT_PRESENTATION,
            &KILL_PRESENTATION,
        ] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
