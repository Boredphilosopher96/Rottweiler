#![allow(clippy::expect_used)]

use rw_tools::{
    EditTool, GlobTool, GrepTool, LsTool, MultiEditTool, ReadTool, Tool, ToolContext, ToolLimits,
    ToolResult, WriteTool,
};
use rw_types::{
    ToolOutput,
    extension_ui::{UiPresentation, UiProjectedField},
};
use serde_json::json;

fn surface(mut result: ToolResult) -> UiPresentation {
    let plan = result.take_presentation().expect("first-party result plan");
    let surface = plan
        .project(
            &ToolOutput::Structured { value: result.data },
            str::to_owned,
        )
        .expect("project authoritative result");
    assert_eq!(surface.owner.extension, "rottweiler");
    assert!(surface.descriptor.actions.is_empty());
    surface
}
fn scalar<'a>(surface: &'a UiPresentation, id: &str) -> &'a str {
    surface
        .projected
        .fields
        .iter()
        .find_map(|field| match field {
            UiProjectedField::Text {
                id: field_id,
                value,
            }
            | UiProjectedField::Badge {
                id: field_id,
                value,
            } if field_id == id => value.as_deref(),
            _ => None,
        })
        .expect("projected scalar")
}
fn table<'a>(surface: &'a UiPresentation, id: &str) -> &'a [Vec<String>] {
    surface
        .projected
        .fields
        .iter()
        .find_map(|field| match field {
            UiProjectedField::Table { id: field_id, rows } if field_id == id => {
                Some(rows.as_slice())
            }
            _ => None,
        })
        .expect("projected table")
}

#[tokio::test]
async fn file_results_supply_their_own_exact_display_fields() {
    let root = tempfile::tempdir().expect("workspace");
    let context = ToolContext::new(root.path()).expect("context");
    let limits = ToolLimits::default();
    let written = surface(
        WriteTool::new(limits)
            .execute(
                &context,
                json!({"path":"entry.txt","content":"hello\nworld"}),
            )
            .await
            .expect("write"),
    );
    assert_eq!(scalar(&written, "path"), "entry.txt");
    assert_eq!(scalar(&written, "bytes"), "11");
    let read = surface(
        ReadTool::new(limits)
            .execute(&context, json!({"path":"entry.txt","start_line":2}))
            .await
            .expect("read"),
    );
    assert_eq!(scalar(&read, "start"), "2");
    assert_eq!(scalar(&read, "lines"), "2");
    assert_eq!(scalar(&read, "bytes"), "11");
    let edited = surface(
        EditTool::new(limits)
            .execute(
                &context,
                json!({"path":"entry.txt","old":"hello","new":"hi"}),
            )
            .await
            .expect("edit"),
    );
    assert_eq!(scalar(&edited, "path"), "entry.txt");
    assert_eq!(scalar(&edited, "match"), "exact");
    let edited = surface(
        MultiEditTool::new(limits)
            .execute(
                &context,
                json!({"path":"entry.txt","edits":[{"old":"hi","new":"hello"}]}),
            )
            .await
            .expect("multi edit"),
    );
    assert_eq!(scalar(&edited, "edits"), "1");
}

#[tokio::test]
async fn search_results_select_bounded_rows_from_actual_result_schemas() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("entry.txt"), "hello\nhello").expect("fixture");
    let context = ToolContext::new(root.path()).expect("context");
    let limits = ToolLimits::default();
    let grep = surface(
        GrepTool::new(limits)
            .execute(&context, json!({"pattern":"hello","path":"."}))
            .await
            .expect("grep"),
    );
    assert_eq!(scalar(&grep, "count"), "2");
    assert_eq!(table(&grep, "matches")[0], ["entry.txt", "1", "hello"]);
    let glob = surface(
        GlobTool::new(limits)
            .execute(&context, json!({"pattern":"*.txt","path":"."}))
            .await
            .expect("glob"),
    );
    assert_eq!(scalar(&glob, "count"), "1");
    assert!(
        matches!(&glob.projected.fields[1], UiProjectedField::List {values,..} if values == &["entry.txt"])
    );
    let ls = surface(
        LsTool::new(limits)
            .execute(&context, json!({"path":"."}))
            .await
            .expect("list"),
    );
    assert_eq!(table(&ls, "entries")[0], ["file", "entry.txt", "11"]);
}
