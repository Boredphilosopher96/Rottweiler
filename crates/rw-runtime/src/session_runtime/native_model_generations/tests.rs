#![allow(clippy::expect_used)]
use super::*;

struct Model(&'static str);
#[async_trait]
impl ModelDriver for Model {
    async fn settle_effects(&self) -> Result<(), AgentLoopError> {
        Ok(())
    }
    fn stream(
        &self,
        _: &str,
        _: rw_providers::ProviderRequest,
        _: rw_core::provider_admission::ProviderInvocation,
    ) -> Result<rw_providers::BoxEventStream, AgentLoopError> {
        Err(AgentLoopError::Provider(
            "fixture never performs inference".into(),
        ))
    }
    fn has_model_alias(&self, alias: &str) -> bool {
        self.0 == alias
    }
}
fn generation(name: &'static str) -> NativeModelGeneration {
    let model: Arc<dyn ModelDriver> = Arc::new(Model(name));
    NativeModelGeneration {
        provider: Arc::clone(&model),
        model,
        catalog: None,
        redactor: FixtureRedactor::default(),
    }
}
fn owner() -> Arc<NativeModelGenerations> {
    NativeModelGenerations::new(generation("first"), Arc::new(|_| Ok(generation("second"))))
}
fn input(root: &std::path::Path) -> NativeModelInput {
    NativeModelInput {
        providers: Vec::new(),
        tools: Arc::new(ToolRegistry::new()),
        roots: vec![root.to_owned()],
        alias: "second".into(),
        websearch: None,
    }
}

#[tokio::test]
async fn child_configuration_retains_its_generation_through_shutdown_and_drop() {
    let owner = owner();
    let child = NativeModelGenerations::capture_child(&owner.child_source()).expect("child");
    assert!(child.provider.has_model_alias("first"));
    assert!(owner.begin_replacement().is_err());
    let resource = Arc::clone(&child.resources);
    child.resources.shutdown().await.expect("child shutdown");
    drop(child);
    assert!(
        owner.begin_replacement().is_err(),
        "another actor configuration retains the lease"
    );
    drop(resource);
    assert!(owner.begin_replacement().is_ok());
}

#[test]
fn replacement_closes_admission_and_publishes_model_source_together() {
    let root = tempfile::tempdir().expect("root");
    let owner = owner();
    let source = owner.source();
    let first = source.resolve().expect("initial provider");
    let old_generation = owner.generation();
    let replacement = owner.begin_replacement().expect("exclusive replacement");
    assert!(owner.generation() > old_generation);
    assert!(source.resolve().is_err());
    assert!(NativeModelGenerations::capture_child(&owner.child_source()).is_err());
    assert!(owner.begin_replacement().is_err());
    let candidate = replacement
        .prepare(input(root.path()))
        .expect("inert candidate");
    assert!(candidate.model().has_model_alias("second"));
    assert!(
        source.resolve().is_err(),
        "preparation cannot reopen admission"
    );
    candidate.publish();
    assert!(
        source
            .resolve()
            .expect("published provider")
            .has_model_alias("second")
    );
    assert!(
        first.has_model_alias("first"),
        "captured drivers never change underneath consumers"
    );
    let child = NativeModelGenerations::capture_child(&owner.child_source()).expect("new child");
    assert!(child.provider.has_model_alias("second"));
}

#[test]
fn abandoned_replacement_stays_closed_without_a_strong_source_cycle() {
    let owner = owner();
    let source = owner.source();
    let weak = Arc::downgrade(&owner);
    drop(owner.begin_replacement().expect("exclusive replacement"));
    assert!(source.resolve().is_err());
    assert!(NativeModelGenerations::capture_child(&weak).is_err());
    drop(owner);
    assert!(weak.upgrade().is_none());
    assert!(source.resolve().is_err());
}
