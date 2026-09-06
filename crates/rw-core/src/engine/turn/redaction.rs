use crate::PermissionRequest;
use crate::engine::redaction::SecretRedactor;
use rw_types::ToolOutput;
use rw_types::ToolOutputPart;
use serde_json::Value;

pub(super) fn redact_json(value: &mut Value, redactor: &dyn SecretRedactor) {
    match value {
        Value::String(text) => *text = redactor.redact(text),
        Value::Array(values) => {
            for value in values {
                redact_json(value, redactor);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if sensitive_json_key(key) && !value.is_null() {
                    *value = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_json(value, redactor);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

pub(super) fn sensitive_json_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    matches!(
        normalized.as_str(),
        "authorization"
            | "proxy_authorization"
            | "cookie"
            | "set_cookie"
            | "api_key"
            | "access_token"
            | "refresh_token"
            | "id_token"
            | "auth_token"
            | "bearer_token"
            | "session_token"
            | "oauth_token"
            | "password"
            | "secret"
            | "client_secret"
            | "private_key"
            | "credential"
            | "credentials"
    ) || normalized.ends_with("_api_key")
        || normalized.ends_with("_token")
        || normalized.ends_with("_password")
        || normalized.ends_with("_secret")
        || normalized.ends_with("_private_key")
        || normalized.ends_with("_credential")
        || normalized.ends_with("_credentials")
}

pub(in crate::engine) fn redacted_json(mut value: Value, redactor: &dyn SecretRedactor) -> Value {
    redact_json(&mut value, redactor);
    value
}

pub(super) fn json_contains_redaction(value: &Value) -> bool {
    match value {
        Value::String(text) => text.contains("[REDACTED]"),
        Value::Array(values) => values.iter().any(json_contains_redaction),
        Value::Object(values) => values.values().any(json_contains_redaction),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

pub(super) fn redacted_permission_request(
    mut request: PermissionRequest,
    redactor: &dyn SecretRedactor,
) -> PermissionRequest {
    redact_json(&mut request.arguments, redactor);
    if let Some(diff) = &mut request.approval_diff {
        diff.unified_diff = redactor.redact(&diff.unified_diff);
        diff.path = redactor.redact(&diff.path);
    }
    request
}

pub(super) fn redact_tool_output(output: &mut ToolOutput, redactor: &dyn SecretRedactor) {
    match output {
        ToolOutput::Text { text } => *text = redactor.redact(text),
        ToolOutput::Structured { value } => redact_json(value, redactor),
        ToolOutput::Mixed { parts } => {
            for part in parts {
                match part {
                    ToolOutputPart::Text { text } => *text = redactor.redact(text),
                    ToolOutputPart::Structured { value } => redact_json(value, redactor),
                    ToolOutputPart::Image { .. } => {}
                }
            }
        }
    }
}
