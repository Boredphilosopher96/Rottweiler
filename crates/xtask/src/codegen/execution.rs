//! Command execution and admission projections share source classifications.
use super::XtaskError;
use rw_types::ClientCommand;
use schemars::schema_for;
use std::collections::{BTreeMap, BTreeSet};
pub(super) fn generate() -> Result<String, XtaskError> {
    let schema = serde_json::to_value(schema_for!(ClientCommand))?;
    let variants = schema
        .get("oneOf")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| XtaskError::GeneratedContract("ClientCommand has no variants".into()))?;
    let read_tags = serde_json::to_value(ClientCommand::read_type_tags())?;
    let mut reads: BTreeSet<&str> = read_tags
        .as_array()
        .ok_or_else(|| XtaskError::GeneratedContract("read tags must be an array".into()))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let urgent_tags = serde_json::to_value(ClientCommand::urgent_type_tags())?;
    let mut urgent: BTreeSet<&str> = urgent_tags
        .as_array()
        .ok_or_else(|| XtaskError::GeneratedContract("urgent tags must be an array".into()))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let watch_tags = serde_json::to_value(ClientCommand::read_watch_type_tags())?;
    let mut watches: BTreeSet<&str> = watch_tags
        .as_array()
        .ok_or_else(|| XtaskError::GeneratedContract("watch tags must be an array".into()))?
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let mut watch_classes = BTreeMap::new();
    let mut lanes = BTreeMap::new();
    let mut classes = BTreeMap::new();
    for variant in variants {
        let tag = variant
            .pointer("/properties/type/const")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| XtaskError::GeneratedContract("command tag missing".into()))?;
        watch_classes.insert(tag, watches.remove(tag));
        classes.insert(tag, if reads.remove(tag) { "read" } else { "control" });
        lanes.insert(
            tag,
            if urgent.remove(tag) {
                "urgent"
            } else {
                "normal"
            },
        );
    }
    if !reads.is_empty() || !urgent.is_empty() || !watches.is_empty() {
        return Err(XtaskError::GeneratedContract(
            "read classification has unknown command tags".into(),
        ));
    }
    let mut output = String::from("\nexport const CLIENT_COMMAND_EXECUTION = {\n");
    for (tag, class) in classes {
        use std::fmt::Write as _;
        let _ = writeln!(output, "  {tag}: \"{class}\",");
    }
    output.push_str(
        "} as const satisfies Record<ClientCommand[\"type\"], \"read\" | \"control\">;\n",
    );
    output.push_str("\nexport const CLIENT_COMMAND_LANE = {\n");
    for (tag, lane) in lanes {
        use std::fmt::Write as _;
        let _ = writeln!(output, "  {tag}: \"{lane}\",");
    }
    output.push_str(
        "} as const satisfies Record<ClientCommand[\"type\"], \"normal\" | \"urgent\">;\n",
    );
    output.push_str("\nexport const CLIENT_COMMAND_READ_WATCH = {\n");
    for (tag, watched) in watch_classes {
        use std::fmt::Write as _;
        let _ = writeln!(output, "  {tag}: {watched},");
    }
    output.push_str("} as const satisfies Record<ClientCommand[\"type\"], boolean>;\n");
    Ok(output)
}
