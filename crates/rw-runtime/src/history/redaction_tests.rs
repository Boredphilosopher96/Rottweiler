use super::super::escape_html;
use super::*;

#[test]
fn export_redaction_handles_delimiter_attached_paths_and_secrets() {
    let input = "cwd=/Users/alice/repo (file:///home/bob/private) token=sk-AbCdEf0123456789GhIjKlMn <b>unsafe</b>";
    let redacted = redact_export_string(input, &FixtureRedactor::default());
    assert_eq!(
        redacted,
        "cwd=[REDACTED_PATH] ([REDACTED_PATH]) token=[REDACTED] <b>unsafe</b>"
    );
    let html = escape_html(&redacted);
    assert!(!html.contains("<b>"));
    assert!(html.contains("&lt;b&gt;"));
}

#[test]
fn export_redaction_combines_known_environment_values_and_arbitrary_absolute_paths() {
    let redactor = FixtureRedactor::default();
    redactor.register_known_value("correct-horse-battery-staple");
    let input = concat!(
        "token=correct-horse-battery-staple ",
        "unix=/private/tmp/rottweiler/repo ",
        "windows=D:\\work\\private\\repo ",
        "unc=\\\\server\\share\\repo ",
        "url=https://example.invalid/public/path relative=src/main.rs"
    );
    let redacted = redact_export_string(input, &redactor);
    assert!(!redacted.contains("correct-horse"));
    assert!(!redacted.contains("/private/tmp"));
    assert!(!redacted.contains("D:\\work"));
    assert!(!redacted.contains("\\\\server"));
    assert!(redacted.contains("https://example.invalid/public/path"));
    assert!(redacted.contains("relative=src/main.rs"));
    assert_eq!(redacted.matches("[REDACTED_PATH]").count(), 3);
}

#[test]
fn export_redaction_preserves_timestamps_and_slash_command_help() {
    let input = "at 2026-07-12T14:23:45.123Z use /add-dir <path> then /models";
    let redacted = redact_export_string(input, &FixtureRedactor::default());
    assert_eq!(redacted, input);
    assert_eq!(
        redact_export_string("read /Users/alice/private", &FixtureRedactor::default()),
        "read [REDACTED_PATH]"
    );
}

#[test]
fn export_json_redacts_opaque_reasoning_signatures_by_field_name() {
    let redacted = redact_export_value(
        serde_json::json!({
            "type": "thinking_delta",
            "text": "summary",
            "signature": "provider-opaque-ciphertext",
        }),
        &FixtureRedactor::default(),
    );
    assert_eq!(redacted["text"], "summary");
    assert_eq!(redacted["signature"], "[REDACTED]");
}
