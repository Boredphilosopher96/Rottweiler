#![allow(clippy::expect_used)]
use super::{
    MCP_HTTP_MAX_HEADER_BYTES, MCP_HTTP_MAX_HEADER_VALUE_BYTES, MCP_HTTP_MAX_HEADERS,
    validate_mcp_http_headers,
};

#[test]
fn combined_headers_reject_individually_legal_sets_over_count() {
    let mut custom = vec![("x-custom".into(), "value".into()); MCP_HTTP_MAX_HEADERS];
    validate_mcp_http_headers(&custom).expect("exact count");
    custom.push(("mcp-protocol-version".into(), "2025-11-25".into()));
    assert!(validate_mcp_http_headers(&custom).is_err());
}

#[test]
fn aggregate_and_value_boundaries_include_header_names() {
    let mut headers = vec![("x".into(), "a".repeat(MCP_HTTP_MAX_HEADER_VALUE_BYTES)); 3];
    let remaining = MCP_HTTP_MAX_HEADER_BYTES - 3 * (MCP_HTTP_MAX_HEADER_VALUE_BYTES + 1) - 1;
    headers.push(("y".into(), "b".repeat(remaining)));
    validate_mcp_http_headers(&headers).expect("exact aggregate bytes");
    headers[3].1.push('b');
    assert!(validate_mcp_http_headers(&headers).is_err());
    assert!(
        validate_mcp_http_headers(&[("x".into(), "a".repeat(MCP_HTTP_MAX_HEADER_VALUE_BYTES + 1))])
            .is_err()
    );
}

#[test]
fn rejects_invalid_names_and_line_injection_but_preserves_authorized_headers() {
    for pair in [
        ("bad name", "v"),
        ("x", "value\r\ninjected: value"),
        ("", "v"),
    ] {
        assert!(validate_mcp_http_headers(&[(pair.0.into(), pair.1.into())]).is_err());
    }
    validate_mcp_http_headers(&[
        ("authorization".into(), "Bearer admitted-token".into()),
        ("mcp-session-id".into(), "session".into()),
        ("last-event-id".into(), "event".into()),
    ])
    .expect("authority-specific construction is separate from aggregate validation");
}

#[test]
fn invalid_response_headers_are_protocol_failures_not_reconnectable_network_errors() {
    for headers in [
        vec![("x".into(), "v".into()); super::MCP_HTTP_MAX_RESPONSE_HEADERS + 1],
        vec![("x".into(), "v\r\nx-injected: yes".into())],
        vec![("x".into(), "v".repeat(MCP_HTTP_MAX_HEADER_VALUE_BYTES + 1))],
    ] {
        assert!(matches!(
            super::validate_response_headers(&headers),
            Err(crate::McpError::Protocol(_))
        ));
    }
}
