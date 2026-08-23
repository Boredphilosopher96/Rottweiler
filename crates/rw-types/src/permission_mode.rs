use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Active session permission mode shared by configuration, engine, and clients.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize, TS)]
#[serde(rename_all = "kebab-case")]
#[ts(rename_all = "kebab-case")]
pub enum PermissionModeDescriptor {
    Strict,
    AutoSafe,
    Yolo,
}

impl PermissionModeDescriptor {
    /// Stable command-line and durable-event spelling for this mode.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::AutoSafe => "auto-safe",
            Self::Yolo => "yolo",
        }
    }
}

impl std::fmt::Display for PermissionModeDescriptor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for PermissionModeDescriptor {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "strict" => Ok(Self::Strict),
            "auto-safe" => Ok(Self::AutoSafe),
            "yolo" => Ok(Self::Yolo),
            _ => Err(format!("unknown permission mode `{value}`")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PermissionModeDescriptor;

    #[test]
    fn command_line_spelling_round_trips() {
        for mode in [
            PermissionModeDescriptor::Strict,
            PermissionModeDescriptor::AutoSafe,
            PermissionModeDescriptor::Yolo,
        ] {
            assert_eq!(mode.as_str().parse(), Ok(mode));
        }
        assert!("auto_safe".parse::<PermissionModeDescriptor>().is_err());
    }
}
