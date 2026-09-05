use super::{UiContributionOwner, UiDisplayDescriptor, UiGenerationId, UiPresentation};
use crate::extension_ui::{
    MAX_UI_SURFACE_BYTES, UiAction, UiContribution, UiField, UiSelectorStep,
};
use serde_json::json;
#[test]
fn display_descriptor_does_not_expose_selectors_or_executable_action_arguments() {
    let contribution = UiContribution::Panel {
        id: "details".into(),
        title: "Details".into(),
        fields: vec![UiField::Text {
            id: "text".into(),
            label: "Text".into(),
            path: vec![UiSelectorStep::Field {
                name: "private_selector".into(),
            }],
        }],
        actions: vec![UiAction {
            id: "inspect".into(),
            label: "Inspect".into(),
            command: "private_command".into(),
            arguments: json!({"private_arg":"value"}),
        }],
    };
    let descriptor = UiDisplayDescriptor::from_declaration(&contribution)
        .unwrap_or_else(|error| panic!("{error}"));
    let wire = serde_json::to_string(&descriptor).unwrap_or_else(|error| panic!("{error}"));
    assert!(!wire.contains("private_"));
    let projected = UiPresentation::project(
        UiContributionOwner {
            extension: "example".into(),
            generation: UiGenerationId::from_bytes([1; 16]),
        },
        &contribution,
        &json!({"private_selector":"text".repeat(20000)}),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(projected.projected.truncated);
    assert!(
        serde_json::to_vec(&projected)
            .unwrap_or_else(|error| panic!("{error}"))
            .len()
            <= MAX_UI_SURFACE_BYTES
    );
}
#[test]
fn generation_identity_rejects_unbounded_and_noncanonical_data() {
    for input in [
        "",
        "ABCDEF0123456789ABCDEF0123456789",
        "../owner",
        "000000000000000000000000000000000",
    ] {
        assert!(serde_json::from_value::<UiGenerationId>(json!(input)).is_err());
    }
    let identity = UiGenerationId::from_bytes([255; 16]);
    assert_eq!(identity.as_str(), "ffffffffffffffffffffffffffffffff");
    assert_eq!(
        serde_json::from_value::<UiGenerationId>(json!(identity.as_str())).ok(),
        Some(identity)
    );
}

#[test]
fn decoded_surface_and_catalog_reject_retention_overflow_and_duplicate_fields() {
    let contribution = UiContribution::Panel {
        id: "p".into(),
        title: "Panel".into(),
        fields: vec![UiField::Text {
            id: "text".into(),
            label: "Text".into(),
            path: Vec::new(),
        }],
        actions: Vec::new(),
    };
    let presentation = UiPresentation::project(
        UiContributionOwner {
            extension: "example".into(),
            generation: UiGenerationId::from_bytes([2; 16]),
        },
        &contribution,
        &json!("ok"),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let mut value = serde_json::to_value(&presentation).unwrap_or_else(|error| panic!("{error}"));
    value["projected"]["fields"][0]["value"] = json!("🔐".repeat(1025));
    assert!(serde_json::from_value::<UiPresentation>(value.clone()).is_err());
    value["projected"]["fields"][0]["value"] = json!("ok");
    value["descriptor"]["fields"]
        .as_array_mut()
        .unwrap_or_else(|| panic!("fields"))
        .push(json!({"kind":"text","id":"text","label":"Duplicate"}));
    assert!(serde_json::from_value::<UiPresentation>(value).is_err());
    let entry = json!({"owner":presentation.owner,"descriptor":presentation.descriptor});
    assert!(
        serde_json::from_value::<super::UiCatalog>(json!({"entries":[entry.clone(),entry]}))
            .is_err()
    );
}
