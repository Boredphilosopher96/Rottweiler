//! Native mixed-package acceptance consumes explicit prebuilt component receipts.
#![allow(clippy::expect_used)]
#[path = "plugin_mixed/fixture.rs"]
mod fixture;
#[path = "plugin_mixed/process.rs"]
mod process;
#[path = "plugin_mixed/wasm.rs"]
mod wasm;

#[tokio::test]
#[ignore = "requires an explicit native bundle, compiled SDK and prepared WASM component"]
async fn native_mixed_packages_activate_independently_and_reject_changed_capabilities() {
    let fixture = fixture::Fixture::prepare().await;
    for (command, expected) in [
        ("/native-ping", "NATIVE_READY"),
        ("/source-ready", "SOURCE_READY"),
        ("/native-probe", "WASM_POLICY_OBSERVED"),
    ] {
        let result = fixture.run(command).await;
        assert!(result.success, "{command}: {}", result.text);
        assert!(result.text.contains(expected), "{command}: {}", result.text);
        assert!(
            !result.text.contains("activation_failed"),
            "{}",
            result.text
        );
        println!(
            "{}",
            serde_json::json!({"command":command,"elapsed_ms":result.elapsed.as_millis()})
        );
    }
    // The unrelated entry never completes initialize. Its activation must be
    // independently bounded, and cannot poison another command generation.
    let hung = fixture.run("/never-ready").await;
    assert!(
        hung.text.contains("activation") || hung.text.contains("timeout"),
        "{}",
        hung.text
    );
    let usable = fixture.run("/native-ping").await;
    assert!(
        usable.success && usable.text.contains("NATIVE_READY"),
        "{}",
        usable.text
    );
    fixture.change_capabilities();
    let changed = fixture.run("/native-ping").await;
    assert!(
        !changed.text.contains("NATIVE_READY"),
        "unapproved capabilities executed"
    );
    assert!(changed.text.contains("approval"), "{}", changed.text);
}
