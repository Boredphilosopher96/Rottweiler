#![allow(clippy::expect_used)]
use super::{ToolEffectGrant, ToolEffectScope};
use crate::{
    CapabilityManifest, MutationScope, ReadTool, ToolContext, ToolLimits, WebFetchTool, WriteTool,
};
use rw_types::ToolCapability::{Network, ReadFilesystem, WriteFilesystem};
use serde_json::json;
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, ToolContext) {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("approved.txt"), "before").expect("file");
    std::fs::write(root.path().join("other.txt"), "other").expect("file");
    let context = ToolContext::new(root.path()).expect("pinned context");
    (root, context)
}
fn grant() -> ToolEffectGrant {
    ToolEffectGrant::new(
        CapabilityManifest::new([ReadFilesystem, WriteFilesystem]),
        &[],
    )
    .expect("filesystem declaration")
}

#[tokio::test]
async fn nested_write_changes_only_the_file_covered_by_the_outer_checkpoint() {
    let (root, context) = fixture();
    let scope = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([ReadFilesystem, WriteFilesystem]),
        &MutationScope::Paths(vec!["approved.txt".into()]),
    )
    .expect("scope");
    let tool = WriteTool::new(ToolLimits::default());
    let input = json!({"path":"approved.txt", "content":"after"});
    let approved = scope
        .authorize(&context, &grant(), &tool, &input)
        .expect("covered write");
    crate::Tool::execute(&tool, &approved, input)
        .await
        .expect("actual write");
    crate::Tool::settle_effects(&tool)
        .await
        .expect("physical write settled");
    assert_eq!(
        std::fs::read(root.path().join("approved.txt")).expect("written file"),
        b"after"
    );
    assert!(
        scope
            .authorize(
                &context,
                &grant(),
                &tool,
                &json!({"path":"other.txt", "content":"forbidden"})
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read(root.path().join("other.txt")).expect("uncovered file"),
        b"other"
    );
}

#[test]
fn capability_flags_do_not_replace_checkpoint_or_plugin_declaration_authority() {
    let (_root, context) = fixture();
    let tool = WriteTool::new(ToolLimits::default());
    let input = json!({"path":"approved.txt", "content":"after"});
    let scope = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([ReadFilesystem, WriteFilesystem]),
        &MutationScope::None,
    )
    .expect("read-only checkpoint");
    assert!(scope.authorize(&context, &grant(), &tool, &input).is_err());
    let writable = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([ReadFilesystem, WriteFilesystem]),
        &MutationScope::OpaqueWorkspace,
    )
    .expect("workspace checkpoint");
    let read_only = ToolEffectGrant::new(CapabilityManifest::new([ReadFilesystem]), &[])
        .expect("read declaration");
    assert!(
        writable
            .authorize(&context, &read_only, &tool, &input)
            .is_err()
    );
    let read = ReadTool::new(ToolLimits::default());
    assert!(
        scope
            .authorize(
                &context,
                &read_only,
                &read,
                &json!({"path":"approved.txt", "line_count":null})
            )
            .is_ok()
    );
}

#[cfg(unix)]
#[test]
fn aliases_resolve_against_exact_checkpoint_files_and_cannot_escape_the_workspace() {
    let (root, context) = fixture();
    let outside = tempfile::tempdir().expect("outside");
    std::fs::write(outside.path().join("secret"), "secret").expect("outside file");
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).expect("symlink");
    let scope = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([ReadFilesystem, WriteFilesystem]),
        &MutationScope::OpaqueWorkspace,
    )
    .expect("workspace scope");
    let read = ReadTool::new(ToolLimits::default());
    assert!(
        scope
            .authorize(
                &context,
                &grant(),
                &read,
                &json!({"path":"escape/secret", "line_count":null})
            )
            .is_err()
    );
}

struct NoFetch;
#[async_trait::async_trait]
impl crate::WebFetcher for NoFetch {
    async fn fetch(
        &self,
        _: crate::FetchRequest,
        _: crate::CancellationToken,
    ) -> Result<crate::FetchResponse, crate::ToolError> {
        panic!("authorization must not perform HTTP");
    }
}
#[test]
fn http_scope_is_explicit_and_survives_as_redirect_authority() {
    let (_root, context) = fixture();
    let scope = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([Network]),
        &MutationScope::None,
    )
    .expect("network approval");
    let grant = ToolEffectGrant::new(CapabilityManifest::new([Network]), &["example.com".into()])
        .expect("domain grant");
    let tool = WebFetchTool::new(Arc::new(NoFetch), ToolLimits::default());
    let approved = scope
        .authorize(
            &context,
            &grant,
            &tool,
            &json!({"url":"https://api.example.com/data"}),
        )
        .expect("approved host");
    assert_eq!(
        approved.effect_domains().expect("redirect policy").as_ref(),
        ["example.com"]
    );
    for host in [
        "example.com.evil.test",
        "badexample.com",
        "registry.npmjs.org",
    ] {
        assert!(
            scope
                .authorize(
                    &context,
                    &grant,
                    &tool,
                    &json!({"url":format!("https://{host}/")})
                )
                .is_err()
        );
    }
}

#[test]
fn cancellation_remains_owned_by_the_outer_invocation() {
    let (_root, context) = fixture();
    let scope = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([ReadFilesystem]),
        &MutationScope::None,
    )
    .expect("read scope");
    context.cancellation.cancel();
    let read = ReadTool::new(ToolLimits::default());
    assert!(matches!(
        scope.authorize(
            &context,
            &grant(),
            &read,
            &json!({"path":"approved.txt", "line_count":null})
        ),
        Err(crate::ToolError::Cancelled)
    ));
}

#[tokio::test]
#[cfg(unix)]
async fn retargeted_input_link_cannot_escape_the_captured_checkpoint() {
    let (root, context) = fixture();
    let link = root.path().join("input-link");
    std::os::unix::fs::symlink("approved.txt", &link).expect("input link");
    let scope = ToolEffectScope::new(
        &context,
        CapabilityManifest::new([ReadFilesystem, WriteFilesystem]),
        &MutationScope::Paths(vec!["approved.txt".into()]),
    )
    .expect("checkpoint scope");
    let tool = WriteTool::new(ToolLimits::default());
    let input = json!({"path":"input-link","content":"outside checkpoint"});
    let authorized = scope
        .authorize(&context, &grant(), &tool, &input)
        .expect("initial covered target");
    std::fs::remove_file(&link).expect("replace link");
    std::os::unix::fs::symlink("other.txt", &link).expect("uncovered target");
    let error = crate::Tool::execute(&tool, &authorized, input)
        .await
        .expect_err("actual IO rejects changed authority");
    assert!(matches!(error, crate::ToolError::DelegationDenied(_)));
    crate::Tool::settle_effects(&tool)
        .await
        .expect("no file effect remains");
    assert_eq!(
        std::fs::read_to_string(root.path().join("approved.txt")).expect("covered file"),
        "before"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("other.txt")).expect("other file"),
        "other"
    );
}
