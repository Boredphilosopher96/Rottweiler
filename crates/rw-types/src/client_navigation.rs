//! Interactive command descriptors shared by the public registry and client discovery.

/// Name, description, and argument hint. Clients project the same catalog while
/// their connection is loading; presentation and terminal effects stay client-owned.
pub const INTERACTIVE_COMMANDS: &[(&str, &str, &str)] = &[
    ("new", "Start a new conversation", ""),
    ("models", "Switch the active model", ""),
    ("providers", "Connect a provider and discover models", ""),
    ("agents", "Inspect and manage child agents", ""),
    ("theme", "Preview and change the interface theme", ""),
    ("settings", "Change safe user settings", ""),
    (
        "context",
        "Inspect, pin, or evict context items",
        "[pin|evict <item-id>]",
    ),
    ("cost", "Show usage, cost, and budget accounting", ""),
    ("errors", "Review recent errors and recovery details", ""),
    ("exit", "Close Rottweiler", ""),
];
