#![allow(clippy::expect_used)]

use std::fs;

use serde_json::json;
use tempfile::tempdir;

use super::io::{atomic_write_if_unchanged, read_capped_snapshot};
use super::{EditTool, MatchMode, MultiEditTool, ReadTool, WriteTool, apply_edit};
use crate::registry::MutationScope;
use crate::symbols::WorkspaceSymbolIndex;
use crate::{Tool, ToolContext, ToolError, ToolLimits};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc};

#[tokio::test]
async fn added_root_is_writable_while_its_parent_remains_blocked() {
    let root = tempfile::tempdir().expect("root");
    let primary = root.path().join("primary");
    let added = root.path().join("added");
    std::fs::create_dir_all(&primary).expect("primary");
    std::fs::create_dir_all(&added).expect("added");
    std::fs::write(root.path().join("parent.txt"), "blocked").expect("parent fixture");
    let context = ToolContext::from_workspace_roots([&primary, &added]).expect("context");
    let write = WriteTool::new(ToolLimits::default());

    write
        .execute(
            &context,
            json!({"path": "@root/1/created.txt", "content": "from added root"}),
        )
        .await
        .expect("write added root");
    assert_eq!(
        std::fs::read_to_string(added.join("created.txt")).expect("created"),
        "from added root"
    );
    write
        .execute(
            &context,
            json!({"path": "../added/relative.txt", "content": "relative sibling"}),
        )
        .await
        .expect("relative added root");
    assert!(matches!(
        write
            .execute(
                &context,
                json!({"path": "../parent.txt", "content": "escape"}),
            )
            .await,
        Err(ToolError::PathEscape(_))
    ));
    assert!(matches!(
        write
            .execute(
                &context,
                json!({"path": "@root/1/../parent.txt", "content": "escape"}),
            )
            .await,
        Err(ToolError::PathEscape(_))
    ));
    assert_eq!(
        std::fs::read_to_string(root.path().join("parent.txt")).expect("parent preserved"),
        "blocked"
    );

    let nested = primary.join("nested");
    std::fs::create_dir_all(&nested).expect("nested");
    std::fs::write(nested.join("owned.txt"), "nested").expect("nested fixture");
    let nested_context =
        ToolContext::from_workspace_roots([&primary, &nested]).expect("nested context");
    let nested_file = std::fs::canonicalize(nested.join("owned.txt")).expect("canonical nested");
    assert_eq!(
        nested_context.relative_display(&nested_file),
        PathBuf::from("@root/1/owned.txt")
    );
}

#[test]
fn edit_is_exact_first_then_normalized_and_never_guesses() {
    let (exact, mode) = apply_edit("a  b\na b", "a  b", "x").expect("exact");
    assert_eq!(exact, "x\na b");
    assert_eq!(mode, MatchMode::Exact);

    let (normalized, mode) =
        apply_edit("before\na   b\nafter", "a b", "x").expect("normalized fallback");
    assert_eq!(normalized, "before\nx\nafter");
    assert_eq!(mode, MatchMode::WhitespaceNormalized);

    let error = apply_edit("a  b\nother\na\tb", "a b", "x").expect_err("ambiguous");
    assert!(matches!(
        error,
        ToolError::AmbiguousEdit { ref candidates } if candidates.len() == 2
    ));
    assert!(matches!(
        apply_edit("aaa", "aa", "x"),
        Err(ToolError::AmbiguousEdit { ref candidates }) if candidates.len() == 2
    ));

    assert_eq!(
        WriteTool::new(ToolLimits::default())
            .mutation_scope(&json!({"path": "src/lib.rs", "content": ""})),
        MutationScope::Paths(vec![PathBuf::from("src/lib.rs")])
    );
    assert_eq!(
        EditTool::new(ToolLimits::default()).mutation_scope(&Value::Null),
        MutationScope::OpaqueWorkspace
    );
    for unsafe_path in ["../outside.rs", "/tmp/outside.rs", "."] {
        assert_eq!(
            WriteTool::new(ToolLimits::default())
                .mutation_scope(&json!({"path": unsafe_path, "content": ""})),
            MutationScope::OpaqueWorkspace
        );
    }
}

#[tokio::test]
async fn multi_edit_does_not_write_a_partial_batch() {
    let root = tempdir().expect("temp directory");
    fs::write(root.path().join("sample.txt"), "one two three").expect("fixture");
    let context = ToolContext::new(root.path()).expect("context");
    let tool = MultiEditTool::new(ToolLimits::default());
    let error = tool
        .execute(
            &context,
            json!({
                "path": "sample.txt",
                "edits": [
                    {"old": "one", "new": "ONE"},
                    {"old": "missing", "new": "MISSING"}
                ]
            }),
        )
        .await
        .expect_err("second edit fails");
    assert!(matches!(error, ToolError::EditNotFound));
    assert_eq!(
        fs::read_to_string(root.path().join("sample.txt")).expect("unchanged fixture"),
        "one two three"
    );
}

#[tokio::test]
async fn edit_compare_and_swap_rejects_a_changed_snapshot_and_accepts_an_unchanged_one() {
    let root = tempdir().expect("temp directory");
    let path = root.path().join("sample.txt");
    fs::write(&path, "before").expect("fixture");
    let context = ToolContext::new(root.path()).expect("context");
    let resolved = context
        .resolve_existing(std::path::Path::new("sample.txt"))
        .expect("resolved fixture");
    let mut transaction = super::transaction::FileTransaction::default();
    let stale = read_capped_snapshot(&context, &resolved, 1024).expect("stale snapshot");
    fs::write(&path, "format").expect("concurrent formatter write");

    let error = atomic_write_if_unchanged(
        &mut transaction,
        &context,
        &resolved,
        b"edited",
        &stale,
        &crate::CancellationToken::default(),
    )
    .expect_err("stale snapshot must be rejected");
    assert!(matches!(error, ToolError::FileChangedSinceRead(ref changed) if changed == &resolved));
    assert_eq!(
        fs::read_to_string(&path).expect("preserved formatter output"),
        "format"
    );

    transaction.cleanup().expect("rollback temporary");
    let current = read_capped_snapshot(&context, &resolved, 1024).expect("current snapshot");
    atomic_write_if_unchanged(
        &mut transaction,
        &context,
        &resolved,
        b"edited",
        &current,
        &crate::CancellationToken::default(),
    )
    .expect("unchanged snapshot succeeds");
    assert_eq!(fs::read_to_string(&path).expect("edited output"), "edited");
}

#[tokio::test]
async fn read_and_write_apply_content_caps() {
    let root = tempdir().expect("temp directory");
    fs::write(root.path().join("large.txt"), "0123456789").expect("fixture");
    let context = ToolContext::new(root.path()).expect("context");
    let limits = ToolLimits {
        max_read_bytes: 4,
        max_write_bytes: 4,
        ..ToolLimits::default()
    };
    assert!(matches!(
        ReadTool::new(limits)
            .execute(&context, json!({"path": "large.txt"}))
            .await,
        Err(ToolError::SizeLimit { limit: 4 })
    ));
    assert!(matches!(
        WriteTool::new(limits)
            .execute(&context, json!({"path": "new.txt", "content": "12345"}))
            .await,
        Err(ToolError::SizeLimit { limit: 4 })
    ));
}

#[tokio::test]
async fn committed_writes_fail_open_and_remove_stale_symbols_when_indexing_rejects_content() {
    let root = tempdir().expect("temp directory");
    let path = root.path().join("lib.rs");
    fs::write(&path, "struct Old;").expect("fixture");
    let index = Arc::new(
        WorkspaceSymbolIndex::new_with_limits(
            [root.path()],
            rw_intel::IndexLimits {
                max_file_bytes: 32,
                ..rw_intel::IndexLimits::default()
            },
        )
        .expect("index"),
    );
    index
        .update_source("lib.rs", "struct Old;")
        .expect("old symbol");
    let replacement = "pub struct NewName;\n".repeat(4);
    let context = ToolContext::new(root.path()).expect("context");
    WriteTool::new(ToolLimits::default())
        .with_symbol_index(Arc::clone(&index))
        .execute(&context, json!({"path": "lib.rs", "content": replacement}))
        .await
        .expect("committed write remains successful");
    assert_eq!(
        fs::read_to_string(&path).expect("committed content"),
        replacement
    );
    assert!(
        index
            .symbols_for_file("lib.rs")
            .expect("symbols")
            .is_empty()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn edit_preserves_executable_mode_and_updates_the_symbol_index() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().expect("temp directory");
    let path = root.path().join("tool.rs");
    fs::write(&path, "fn before() {}\n").expect("fixture");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("permissions");
    let index = Arc::new(WorkspaceSymbolIndex::new([root.path()]).expect("index"));
    let context = ToolContext::new(root.path()).expect("context");
    EditTool::new(ToolLimits::default())
        .with_symbol_index(Arc::clone(&index))
        .execute(
            &context,
            json!({"path": "tool.rs", "old": "before", "new": "after"}),
        )
        .await
        .expect("edit");
    assert_eq!(
        fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
        0o755
    );
    let symbols = index.symbols_for_file("tool.rs").expect("indexed symbols");
    assert!(symbols.iter().any(|symbol| symbol.name == "after"));
    assert!(!symbols.iter().any(|symbol| symbol.name == "before"));
}

#[tokio::test]
async fn edit_updates_only_the_stable_added_root_symbol_index() {
    let primary = tempdir().expect("primary");
    let added = tempdir().expect("added");
    fs::write(primary.path().join("same.rs"), "fn primary_before() {}\n").expect("primary source");
    fs::write(added.path().join("same.rs"), "fn added_before() {}\n").expect("added source");
    let index = Arc::new(WorkspaceSymbolIndex::new([primary.path(), added.path()]).expect("index"));
    index
        .update_source("same.rs", "fn primary_before() {}\n")
        .expect("primary index");
    index
        .update_source("@root/1/same.rs", "fn added_before() {}\n")
        .expect("added index");
    let context =
        ToolContext::from_workspace_roots([primary.path(), added.path()]).expect("context");
    EditTool::new(ToolLimits::default())
        .with_symbol_index(index.clone())
        .execute(
            &context,
            json!({"path":"@root/1/same.rs","old":"added_before","new":"added_after"}),
        )
        .await
        .expect("edit added root");
    assert!(
        index
            .symbols_for_file("same.rs")
            .expect("primary symbols")
            .iter()
            .any(|symbol| symbol.name == "primary_before")
    );
    let added_symbols = index
        .symbols_for_file("@root/1/same.rs")
        .expect("added symbols");
    assert!(
        added_symbols
            .iter()
            .any(|symbol| symbol.name == "added_after")
    );
    assert!(
        added_symbols
            .iter()
            .all(|symbol| symbol.name != "added_before")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn direct_file_tools_reject_symlinks_escaping_the_workspace() {
    use std::os::unix::fs::symlink;

    let root = tempdir().expect("workspace");
    let outside = tempdir().expect("outside");
    fs::write(outside.path().join("secret.txt"), "secret").expect("outside file");
    symlink(outside.path(), root.path().join("escape")).expect("symlink");
    let context = ToolContext::new(root.path()).expect("context");
    assert!(matches!(
        ReadTool::new(ToolLimits::default())
            .execute(&context, json!({"path": "escape/secret.txt"}))
            .await,
        Err(ToolError::PathEscape(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn special_files_are_rejected_without_blocking_and_write_only_mode_is_preserved() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempdir().expect("workspace");
    let fifo = root.path().join("pipe");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("run mkfifo")
            .success()
    );
    let context = ToolContext::new(root.path()).expect("context");
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        ReadTool::new(ToolLimits::default()).execute(&context, json!({"path": "pipe"})),
    )
    .await
    .expect("read must not block");
    assert!(matches!(read, Err(ToolError::InvalidInput(_))));
    let write = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        WriteTool::new(ToolLimits::default())
            .execute(&context, json!({"path": "pipe", "content": "replacement"})),
    )
    .await
    .expect("write must not block");
    assert!(matches!(write, Err(ToolError::InvalidInput(_))));

    let write_only = root.path().join("write-only.txt");
    fs::write(&write_only, "old").expect("write-only fixture");
    fs::set_permissions(&write_only, fs::Permissions::from_mode(0o200))
        .expect("write-only permissions");
    WriteTool::new(ToolLimits::default())
        .execute(
            &context,
            json!({"path": "write-only.txt", "content": "new"}),
        )
        .await
        .expect("write-only replacement");
    assert_eq!(
        fs::metadata(&write_only)
            .expect("write-only metadata")
            .permissions()
            .mode()
            & 0o777,
        0o200
    );
    fs::set_permissions(&write_only, fs::Permissions::from_mode(0o600))
        .expect("restore readable mode");
    assert_eq!(
        fs::read_to_string(&write_only).expect("replaced content"),
        "new"
    );
}
