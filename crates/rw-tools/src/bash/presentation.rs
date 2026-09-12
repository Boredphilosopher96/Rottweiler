use crate::presentation::{BuiltinToolPresentation, fields};

pub(super) static BACKGROUND_START_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("bash", "Start background command", || {
        vec![
            fields::text("process", "Process", &["background_process", "process_id"]),
            fields::badge(
                "status",
                "Status",
                &["background_process", "status", "state"],
            ),
        ]
    });
pub(super) static BASH_PRESENTATION: BuiltinToolPresentation =
    BuiltinToolPresentation::new("bash", "Run command", || {
        vec![
            fields::badge("exit", "Exit code", &["exit_code"]),
            fields::badge(
                "stdout-truncated",
                "Stdout truncated",
                &["stdout_truncated"],
            ),
            fields::badge(
                "stderr-truncated",
                "Stderr truncated",
                &["stderr_truncated"],
            ),
        ]
    });

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_declarations_are_valid_and_generation_stable() {
        for declaration in [&BACKGROUND_START_PRESENTATION, &BASH_PRESENTATION] {
            let first = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            let second = declaration.plan().unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(first, second);
        }
    }
}
