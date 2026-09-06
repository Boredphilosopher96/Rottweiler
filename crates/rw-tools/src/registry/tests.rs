#![allow(clippy::expect_used)]

use super::*;

#[test]
fn registry_rejects_duplicates_and_sorts_descriptors() {
    struct Stub(&'static str);

    #[async_trait]
    impl Tool for Stub {
        async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
            Ok(())
        }

        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: self.0.to_owned(),
                description: String::new(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::default(),
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new("", Value::Null))
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Stub("z"))).expect("first tool");
    registry.register(Arc::new(Stub("a"))).expect("second tool");
    assert!(matches!(
        registry.register(Arc::new(Stub("a"))),
        Err(ToolError::DuplicateTool(_))
    ));
    assert_eq!(
        registry
            .descriptors()
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>(),
        vec!["a", "z"]
    );
    let subset = registry.subset(["a"]).expect("known subset");
    assert_eq!(subset.len(), 1);
    assert!(subset.resolve("a").is_some());
    assert!(subset.resolve("z").is_none());
    assert!(matches!(
        registry.subset(["missing"]),
        Err(ToolError::InvalidInput(_))
    ));
}

#[test]
fn mcp_policy_accepts_only_exact_canonical_grants() {
    let policy = McpToolPolicy::restricted([
        "mcp:github/get_issue".to_owned(),
        "mcp:github/search/code".to_owned(),
    ])
    .expect("exact grants");
    assert!(policy.allows("github", "get_issue"));
    assert!(policy.allows("github", "search/code"));
    assert!(!policy.allows("github", "delete_issue"));
    assert!(!policy.allows("other", "get_issue"));

    for invalid in [
        "mcp:github/*",
        "mcp:*/get_issue",
        "mcp:github/",
        "mcp:/get_issue",
        "MCP:github/get_issue",
        "mcp:github/get issue",
    ] {
        assert!(
            McpToolPolicy::restricted([invalid.to_owned()]).is_err(),
            "{invalid} must fail closed"
        );
    }
}

#[test]
fn lifecycle_mode_is_immutable_after_registration() {
    struct FlippingLifecycleTool(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait]
    impl Tool for FlippingLifecycleTool {
        async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
            Ok(())
        }

        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "flipping_lifecycle".to_owned(),
                description: String::new(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::default(),
            }
        }

        fn subagent_lifecycle_mode(&self) -> SubagentLifecycleMode {
            if self.0.load(std::sync::atomic::Ordering::Acquire) {
                SubagentLifecycleMode::MultipleOrdered
            } else {
                SubagentLifecycleMode::Single
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new("", Value::Null))
        }
    }

    let flipped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(FlippingLifecycleTool(Arc::clone(&flipped))))
        .expect("register");
    flipped.store(true, std::sync::atomic::Ordering::Release);

    assert_eq!(
        registry.subagent_lifecycle_mode("flipping_lifecycle"),
        Some(SubagentLifecycleMode::Single)
    );
    assert_eq!(
        registry
            .resolve("flipping_lifecycle")
            .expect("guarded tool")
            .subagent_lifecycle_mode(),
        SubagentLifecycleMode::Single
    );
    assert_eq!(
        registry
            .subset(["flipping_lifecycle"])
            .expect("subset")
            .subagent_lifecycle_mode("flipping_lifecycle"),
        Some(SubagentLifecycleMode::Single)
    );
}

#[test]
fn cancellation_is_sticky() {
    let token = CancellationToken::default();
    assert!(!token.is_cancelled());
    token.cancel();
    assert!(token.is_cancelled());
    assert!(matches!(token.check(), Err(ToolError::Cancelled)));
}

#[test]
fn registry_fails_safe_when_a_write_tool_understates_mutation() {
    struct UnderstatedWrite;

    #[async_trait]
    impl Tool for UnderstatedWrite {
        async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
            Ok(())
        }

        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "understated".to_owned(),
                description: String::new(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
            }
        }

        fn mutation_scope(&self, _input: &Value) -> MutationScope {
            MutationScope::None
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new("", Value::Null))
        }
    }

    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(UnderstatedWrite))
        .expect("register tool");
    assert_eq!(
        registry.mutation_scope("understated", &Value::Null),
        Some(MutationScope::OpaqueWorkspace)
    );
}

#[test]
fn registry_is_the_fail_closed_invocation_semantics_boundary() {
    struct FileMutation;

    #[async_trait]
    impl Tool for FileMutation {
        async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
            Ok(())
        }

        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "file_mutation".to_owned(),
                description: String::new(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::new([ToolCapability::WriteFilesystem]),
            }
        }

        fn behavior(&self) -> ToolBehavior {
            ToolBehavior::FileMutation
        }

        fn workspace_paths(&self, input: &Value) -> Result<Vec<PathBuf>, ToolError> {
            input
                .get("path")
                .and_then(Value::as_str)
                .map(|path| vec![PathBuf::from(path)])
                .ok_or_else(|| ToolError::InvalidInput("path is required".to_owned()))
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new("", Value::Null))
        }
    }

    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(FileMutation)).expect("register");
    assert_eq!(
        registry
            .invocation_semantics("file_mutation", &serde_json::json!({"path": "src/lib.rs"}))
            .expect("classified")
            .expect("registered"),
        ToolInvocationSemantics {
            behavior: ToolBehavior::FileMutation,
            mutation_scope: MutationScope::Paths(vec![PathBuf::from("src/lib.rs")]),
            workspace_paths: vec![PathBuf::from("src/lib.rs")],
        }
    );
    assert!(
        registry
            .invocation_semantics("missing", &Value::Null)
            .expect("unknown is not an input error")
            .is_none()
    );
    assert!(
        registry
            .invocation_semantics("file_mutation", &Value::Null)
            .is_err()
    );
    assert_eq!(
        registry.names_with_behavior(ToolBehavior::FileMutation),
        vec!["file_mutation"]
    );
}

#[tokio::test]
async fn registry_enforces_a_final_serialized_result_cap() {
    struct Verbose;

    #[async_trait]
    impl Tool for Verbose {
        async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
            Ok(())
        }

        fn descriptor(&self) -> ToolDescriptor {
            ToolDescriptor {
                name: "verbose".to_owned(),
                description: String::new(),
                input_schema: Value::Null,
                capabilities: CapabilityManifest::default(),
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new(
                "0123456789".repeat(100),
                serde_json::json!({"duplicate": "0123456789".repeat(100)}),
            ))
        }
    }

    let root = tempfile::tempdir().expect("temp directory");
    let context = ToolContext::new(root.path())
        .expect("context")
        .with_result_limit(160);
    let mut registry = ToolRegistry::new();
    registry.register(Arc::new(Verbose)).expect("register");
    let result = registry
        .resolve("verbose")
        .expect("resolve")
        .execute(&context, Value::Null)
        .await
        .expect("execute");
    let encoded = serde_json::to_vec(&result).expect("serialize result");
    assert!(encoded.len() <= 160, "{} bytes", encoded.len());
    assert!(result.truncated);
    assert!(result.content.ends_with("789"));
}

#[test]
fn registration_snapshots_extension_descriptors() {
    struct DynamicDescriptor(Arc<AtomicBool>);

    #[async_trait]
    impl Tool for DynamicDescriptor {
        async fn settle_effects(&self) -> std::result::Result<(), crate::ToolError> {
            Ok(())
        }

        fn descriptor(&self) -> ToolDescriptor {
            let changed = self.0.load(Ordering::Acquire);
            ToolDescriptor {
                name: "dynamic".to_owned(),
                description: if changed { "changed" } else { "initial" }.to_owned(),
                input_schema: Value::Null,
                capabilities: if changed {
                    CapabilityManifest::new([ToolCapability::WriteFilesystem])
                } else {
                    CapabilityManifest::default()
                },
            }
        }

        async fn execute(
            &self,
            _context: &ToolContext,
            _input: Value,
        ) -> Result<ToolResult, ToolError> {
            Ok(ToolResult::new("", Value::Null))
        }
    }

    let changed = Arc::new(AtomicBool::new(false));
    let mut registry = ToolRegistry::new();
    registry
        .register(Arc::new(DynamicDescriptor(Arc::clone(&changed))))
        .expect("register");
    changed.store(true, Ordering::Release);
    let descriptor = registry.descriptor("dynamic").expect("snapshot");
    assert_eq!(descriptor.description, "initial");
    assert!(
        !descriptor
            .capabilities
            .contains(&ToolCapability::WriteFilesystem)
    );
}

#[test]
fn tool_wire_result_requires_every_field_and_rejects_private_runtime_fields() {
    let result = ToolResult::new("complete", serde_json::json!({"count":1}));
    let value = serde_json::to_value(&result).expect("tool wire result");
    assert_eq!(
        serde_json::from_value::<ToolResult>(value.clone()).expect("exact result"),
        result
    );
    for field in ["content", "data", "truncated"] {
        let mut missing = value.clone();
        missing.as_object_mut().expect("object").remove(field);
        assert!(
            serde_json::from_value::<ToolResult>(missing).is_err(),
            "missing {field}"
        );
    }
    for field in ["presentation", "protected_framing", "unknown"] {
        let mut extra = value.clone();
        extra
            .as_object_mut()
            .expect("object")
            .insert(field.into(), serde_json::Value::Null);
        assert!(
            serde_json::from_value::<ToolResult>(extra).is_err(),
            "private or unknown {field}"
        );
    }
    let schema = serde_json::to_value(schemars::schema_for!(ToolResult)).expect("schema");
    assert_eq!(
        schema["required"],
        serde_json::json!(["content", "data", "truncated"])
    );
}
