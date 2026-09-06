#![allow(clippy::expect_used)]
use super::*;

#[test]
fn export_redaction_handles_delimiter_attached_paths_and_secrets() {
    let input = "cwd=/Users/alice/repo (file:///home/bob/private) token=sk-AbCdEf0123456789GhIjKlMn <b>unsafe</b>";
    let redacted =
        redact_export_string(input, &FixtureRedactor::default(), 4096).expect("bounded redaction");
    assert_eq!(
        redacted,
        "cwd=[REDACTED_PATH] ([REDACTED_PATH]) token=[REDACTED] <b>unsafe</b>"
    );
    let mut html = Output::new(4096);
    html.html(&redacted).expect("bounded HTML escaping");
    let html = html.text().expect("HTML text");
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
    let redacted = redact_export_string(input, &redactor, 4096).expect("bounded redaction");
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
    let redacted =
        redact_export_string(input, &FixtureRedactor::default(), 4096).expect("bounded redaction");
    assert_eq!(redacted, input);
    assert_eq!(
        redact_export_string(
            "read /Users/alice/private",
            &FixtureRedactor::default(),
            4096
        )
        .expect("bounded redaction"),
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
        4096,
    )
    .expect("bounded redaction");
    assert_eq!(redacted["text"], "summary");
    assert_eq!(redacted["signature"], "[REDACTED]");
}

#[test]
fn redaction_expansion_and_aggregate_strings_reserve_before_replacement() {
    let redactor = FixtureRedactor::default();
    assert!(redact_export_string("/a", &redactor, 2).is_err());
    assert_eq!(
        redact_export_string("/a", &redactor, 15).expect("exact expansion"),
        "[REDACTED_PATH]"
    );
    let value = serde_json::json!(["/a", "/b"]);
    assert!(redact_export_value(value, &redactor, 20).is_err());
}
