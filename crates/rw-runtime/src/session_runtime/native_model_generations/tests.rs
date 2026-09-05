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
        children: Arc::new(move |_, _| Arc::new(Model(name))),
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
    let child = NativeModelGenerations::capture_child(
        &owner.child_source(),
        std::path::Path::new("/workspace"),
        "first",
    )
    .expect("child");
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
    assert!(
        NativeModelGenerations::capture_child(
            &owner.child_source(),
            std::path::Path::new("/workspace"),
            "first"
        )
        .is_err()
    );
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
    let child = NativeModelGenerations::capture_child(
        &owner.child_source(),
        std::path::Path::new("/workspace"),
        "first",
    )
    .expect("new child");
    assert!(child.provider.has_model_alias("second"));
}

#[test]
fn abandoned_replacement_stays_closed_without_a_strong_source_cycle() {
    let owner = owner();
    let source = owner.source();
    let weak = Arc::downgrade(&owner);
    drop(owner.begin_replacement().expect("exclusive replacement"));
    assert!(source.resolve().is_err());
    assert!(
        NativeModelGenerations::capture_child(&weak, std::path::Path::new("/workspace"), "first")
            .is_err()
    );
    drop(owner);
    assert!(weak.upgrade().is_none());
    assert!(source.resolve().is_err());
}

#[tokio::test]
async fn live_child_model_selection_is_private_to_each_session() {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::RwLock;
    let root = tempfile::tempdir().expect("root");
    let mut config = rw_core::Config::default();
    config.providers = BTreeMap::from([(
        "local".into(),
        rw_types::config::ProviderConfig {
            kind: "extension".into(),
            ..Default::default()
        },
    )]);
    config.models.aliases = BTreeMap::from([
        ("first".into(), vec!["local/first-model".into()]),
        ("second".into(), vec!["local/second-model".into()]),
    ]);
    config.models.default = "first".into();
    let recipe = NativeModelRecipe {
        provider: NativeProviderRecipe::Live {
            credentials: root.path().join("credentials.json"),
            pricing: rw_providers::PricingTable::default(),
            config,
        },
        redactor: FixtureRedactor::default(),
        prompt_shapes: None,
        catalog_path: root.path().join("catalog.json"),
        instruction_roots: Arc::new(RwLock::new(vec![root.path().to_owned()])),
        active_sources: Arc::new(RwLock::new(BTreeSet::new())),
    };
    let provider: Arc<dyn rw_providers::Provider> = Arc::new(
        super::super::script_provider::ScriptProvider::new("local".into(), Vec::new(), 0),
    );
    let compose = recipe.child_composer(vec![("local/".into(), provider)]);
    let first = compose(root.path(), "first");
    let second = compose(root.path(), "second");
    first
        .prepare_model("first")
        .await
        .expect("first preparation requires no network");
    first.commit_prepared_model("first");
    second
        .prepare_model("second")
        .await
        .expect("second preparation requires no network");
    second.commit_prepared_model("second");
    assert!(first.has_model_alias("first"));
    assert!(!first.has_model_alias("second"));
    assert!(second.has_model_alias("second"));
    assert!(!second.has_model_alias("first"));
    first.settle_effects().await.expect("first settled");
    second.settle_effects().await.expect("second settled");
}
