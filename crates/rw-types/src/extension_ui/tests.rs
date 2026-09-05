#![allow(clippy::expect_used)]
use super::*;
use serde_json::json;

fn field() -> UiField {
    UiField::Text {
        id: "summary".into(),
        label: "Summary".into(),
        path: vec![UiSelectorStep::Field {
            name: "summary".into(),
        }],
    }
}
fn descriptor(fields: Vec<UiField>) -> UiContribution {
    UiContribution::Tool {
        id: "review".into(),
        tool_name: "review".into(),
        title: "Review".into(),
        fields,
        actions: vec![],
    }
}

#[test]
fn selectors_are_structural_and_preserve_declared_shape() {
    let fields = vec![
        field(),
        UiField::Table {
            id: "files".into(),
            label: "Files".into(),
            path: vec![UiSelectorStep::Field {
                name: "files".into(),
            }],
            columns: vec![UiTableColumn {
                label: "Path".into(),
                path: vec![UiSelectorStep::Field {
                    name: "path".into(),
                }],
            }],
            max_rows: 2,
        },
    ];
    let source =
        json!({"summary":"done","files":[{"path":"src/a.rs"},{"path":false},{"path":"not shown"}]});
    let projected = project_fields(&fields, &source).expect("projection");
    assert_eq!(
        projected.fields[0],
        UiProjectedField::Text {
            id: "summary".into(),
            value: Some("done".into())
        }
    );
    assert_eq!(
        projected.fields[1],
        UiProjectedField::Table {
            id: "files".into(),
            rows: vec![vec!["src/a.rs".into()], vec![String::new()]]
        }
    );
    assert!(projected.truncated);
    assert!(validate_projected_fields(&fields, &projected).is_ok());
}

#[test]
fn output_is_bounded_before_retention_with_json_escape_cost_and_unicode() {
    let fields = (0..MAX_UI_FIELDS)
        .map(|index| UiField::List {
            id: format!("f{index}"),
            label: "Values".into(),
            path: vec![],
            max_items: 32,
        })
        .collect::<Vec<_>>();
    for text in [
        "\\\"".repeat(4096),
        "界".repeat(4096),
        "\u{1b}".repeat(16_384),
    ] {
        let projected =
            project_fields(&fields, &json!(vec![text; 32])).expect("bounded projection");
        assert!(serde_json::to_vec(&projected).expect("JSON").len() <= MAX_UI_SURFACE_BYTES);
        assert!(projected.truncated);
        assert!(validate_projected_fields(&fields, &projected).is_ok());
    }
}

#[test]
fn descriptors_reject_unbounded_and_ambiguous_capabilities() {
    assert!(validate_contributions(&[descriptor(vec![field()])]).is_ok());
    assert!(validate_contributions(&[descriptor(vec![field(), field()])]).is_err());
    assert!(
        validate_contributions(&[descriptor(vec![field()]), descriptor(vec![field()])]).is_err()
    );
    let mut too_deep = field();
    if let UiField::Text { path, .. } = &mut too_deep {
        *path = vec![UiSelectorStep::Index { index: 0 }; MAX_UI_SELECTOR_STEPS + 1];
    }
    assert!(validate_contributions(&[descriptor(vec![too_deep])]).is_err());
    let mut action = descriptor(vec![]);
    if let UiContribution::Tool { actions, .. } = &mut action {
        actions.push(UiAction {
            id: "open".into(),
            label: "Open".into(),
            command: "sh -c whoami".into(),
            arguments: json!({}),
        });
    }
    assert!(validate_contributions(&[action]).is_err());
}

#[test]
fn projection_rejects_extra_fields_changed_ids_and_missing_null() {
    assert!(serde_json::from_value::<UiProjectedField>(json!({"kind":"text","id":"x"})).is_err());
    assert!(
        serde_json::from_value::<UiSelectorStep>(
            json!({"step":"field","name":"x","script":"execute"})
        )
        .is_err()
    );
    let mut result = project_fields(&[field()], &json!({"summary":"ok"})).expect("projection");
    if let UiProjectedField::Text { id, .. } = &mut result.fields[0] {
        *id = "other".into();
    }
    assert!(validate_projected_fields(&[field()], &result).is_err());
}
