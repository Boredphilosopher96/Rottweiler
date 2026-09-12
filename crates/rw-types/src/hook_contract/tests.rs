#![cfg(test)]

use serde_json::json;

use super::{HookDirective, HookInput, HookToolInput, HookToolResultInput, HookTransform};
use crate::ToolOutput;

#[test]
fn hook_inputs_require_the_matching_complete_payload() {
    for input in [
        json!({"hook":"pre_tool","payload":{"id":"call","name":"read"}}),
        json!({"hook":"pre_tool","payload":{"id":"call","name":"read","arguments":[],"extra":true}}),
        json!({"hook":"user_prompt_submit","payload":{"content":"hello","role":"system"}}),
        json!({"hook":"session_start","payload":{"content":"hello"}}),
        json!({"hook":"pre_compact","payload":{"reason":"manual","conversation_turns":2,"injected_context":[]}}),
        json!({"hook":"turn_end","payload":{"turn":2,"status":"completed"}}),
    ] {
        assert!(
            serde_json::from_value::<HookInput>(input.clone()).is_err(),
            "{input}"
        );
    }
}

#[test]
fn directives_cannot_supply_replacement_invocation_identity() {
    for directive in [
        json!({"decision":"transform","change":{"hook":"pre_tool","id":"other","name":"read","arguments":{}}}),
        json!({"decision":"transform","change":{"hook":"session_start","workspace":"/other"}}),
        json!({"decision":"permission","value":"unknown"}),
        json!({"decision":"continue","payload":{}}),
    ] {
        assert!(
            serde_json::from_value::<HookDirective>(directive.clone()).is_err(),
            "{directive}"
        );
    }
}

#[test]
fn transformations_preserve_identity_and_reject_cross_phase_changes() {
    let mut input = HookInput::PreTool(HookToolInput {
        id: "call".to_owned(),
        name: "read".to_owned(),
        arguments: json!({}),
    });
    let before = input.clone();
    assert!(
        input
            .apply(HookTransform::UserPromptSubmit {
                content: "different".to_owned()
            })
            .is_err()
    );
    assert_eq!(input, before);
    assert!(
        input
            .apply(HookTransform::PreTool {
                name: "read;write".to_owned(),
                arguments: json!({})
            })
            .is_err()
    );
    assert_eq!(input, before);
    assert!(
        input
            .apply(HookTransform::PreTool {
                name: "search".to_owned(),
                arguments: json!({})
            })
            .is_ok()
    );
    let HookInput::PreTool(changed) = input else {
        panic!("pre-tool identity")
    };
    assert_eq!(changed.id, "call");
    assert_eq!(changed.name, "search");
}

#[test]
fn post_tool_transforms_cannot_erase_execution_failure() {
    let mut input = HookInput::PostTool(HookToolResultInput {
        id: "call".to_owned(),
        name: "write".to_owned(),
        arguments: json!({}),
        output: ToolOutput::Text {
            text: "failed".to_owned(),
        },
        is_error: true,
    });
    assert!(
        input
            .apply(HookTransform::PostTool {
                output: ToolOutput::Text {
                    text: "annotated".to_owned()
                },
                is_error: false,
            })
            .is_ok()
    );
    let HookInput::PostTool(changed) = input else {
        panic!("post-tool identity")
    };
    assert!(changed.is_error);
    assert_eq!(changed.id, "call");
    assert_eq!(changed.name, "write");
}
