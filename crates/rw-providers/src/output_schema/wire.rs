//! Official OpenAI strict schema projection. Generic compatible dialects do not opt in.
use super::{OutputContract, OutputSchema};
use serde_json::{Map, Value, json};

pub(crate) fn apply(
    contract: &OutputContract,
    body: &mut Map<String, Value>,
    mode: crate::OpenAiWireMode,
) {
    let OutputContract::JsonSchema { name, schema } = contract else {
        return;
    };
    let schema = project(schema);
    match mode {
        crate::OpenAiWireMode::ChatCompletions => {
            body.insert("response_format".into(), json!({"type":"json_schema", "json_schema":{"name":name,"strict":true,"schema":schema}}));
        }
        crate::OpenAiWireMode::Responses => {
            // Retain unrelated current text controls while owning the format contract.
            let text = body.entry("text").or_insert_with(|| json!({}));
            if !text.is_object() {
                *text = json!({});
            }
            text["format"] =
                json!({"type":"json_schema","name":name,"strict":true,"schema":schema});
        }
    }
}
fn project(schema: &OutputSchema) -> Value {
    match schema {
        OutputSchema::String {} => json!({"type":"string"}),
        OutputSchema::Number {} => json!({"type":"number"}),
        OutputSchema::Integer {} => json!({"type":"integer"}),
        OutputSchema::Boolean {} => json!({"type":"boolean"}),
        OutputSchema::Null {} => json!({"type":"null"}),
        OutputSchema::Array { items } => json!({"type":"array","items":project(items)}),
        OutputSchema::Nullable { value } => json!({"anyOf":[project(value),{"type":"null"}]}),
        OutputSchema::Object { fields } => {
            let properties: Map<String, Value> = fields
                .iter()
                .map(|f| (f.name.clone(), project(&f.schema)))
                .collect();
            let required: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
            json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
        }
    }
}
pub(crate) fn is_refusal(value: &Value) -> bool {
    if value["type"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("response.refusal."))
    {
        return true;
    }
    let refused = |part: &Value| {
        part["type"] == "refusal" || part.get("refusal").is_some_and(|v| !v.is_null())
    };
    refused(&value["part"])
        || value["item"]["content"]
            .as_array()
            .is_some_and(|parts| parts.iter().any(&refused))
        || value["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                let delta = &choice["delta"];
                refused(delta)
                    || delta["content"]
                        .as_array()
                        .is_some_and(|parts| parts.iter().any(&refused))
            })
        })
        || value["response"]["output"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(&refused))
            })
        })
}
